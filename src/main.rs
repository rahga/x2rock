mod bookmarks;
mod catalogue;
mod cli;
mod commands;
mod completions;
mod credentials;
mod daemon;
mod discover;
mod hint;
mod mpris;
mod netid;
mod restart;
mod service;
mod session;
mod sonos;
mod state;
mod stations;
mod store;
mod streams;
#[cfg(test)]
mod testdir;
mod tui;

use anyhow::{Result, ensure};
use clap::{CommandFactory, FromArgMatches};

use cli::{Cli, Command, RawTransport};
use commands::playback::{apply_crossfade, apply_repeat, apply_shuffle, play_or_resume};
use commands::speaker::{
    ToneRequest, apply_buttons, apply_eq, apply_led, apply_remote, apply_rename, apply_sleep,
    apply_snooze,
};
use commands::{
    admin, content, fan_out, fans_out, household, playback, raw, services, speaker, status, stream,
    too_many_rooms, volume,
};
use state::State;

/// What this build is, for `--version`, the daemon's first log line and the
/// header of the unit `service install` writes. The crate version plus the
/// commit it was built from, stamped in by `build.rs` - see there for why the
/// crate version alone cannot answer it.
pub const VERSION: &str = env!("X2ROCK_VERSION");

/// Exit quietly when the thing reading our output has gone away.
///
/// Rust sets `SIGPIPE` to ignore before `main`, so a write to a pipe whose
/// reader has exited returns `EPIPE` - and `println!` answers that by
/// panicking, which `panic = "abort"` then turns into a core dump.
/// `x2rock favorites --json | head` is an ordinary thing to type and deserves
/// the ordinary answer, which is to stop without a word.
///
/// Restoring the default disposition is the usual fix for a CLI and is the
/// wrong one here, because this binary is also a daemon. The same signal would
/// then kill it on a write to a closed *socket*, where today the write returns
/// an error, `follow` logs "connection lost" and the reconnect machinery takes
/// over. Ignoring `SIGPIPE` is what keeps that working, so the pipe is answered
/// where it actually goes wrong - in the panic - and nowhere else.
///
/// Matching on the message std uses is the fragile part, and it fails safe: if
/// that wording ever changes, the panic reaches the default hook and behaves as
/// it does today.
fn quiet_broken_pipe() {
    let inherited = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let printing = info
            .payload()
            .downcast_ref::<String>()
            .is_some_and(|m| m.starts_with("failed printing to "));
        if printing {
            // 128 + SIGPIPE, which is what a shell reports when `head` closes
            // the pipe. Nothing is said about it: the stream to say it on is
            // the one that just broke.
            std::process::exit(141);
        }
        inherited(info);
    }));
}

/// Wait for whichever asks the daemon to stop, and name it for the log.
///
/// Ctrl-C is not the usual one: as a systemd user service, `systemctl stop` and
/// the restart on upgrade both send SIGTERM. Left unhandled that is a default
/// kill - no unwinding, no line in the journal saying why the daemon went away.
async fn stop_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        // Nothing to be done about it, and Ctrl-C still works.
        let _ = tokio::signal::ctrl_c().await;
        return "interrupt";
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "interrupt",
        _ = terminate.recv() => "SIGTERM",
    }
}

#[tokio::main]
async fn main() {
    quiet_broken_pipe();
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    // `X2ROCK_ROOM` is the default room, and `--all` means every room: the
    // default has nothing to add to that, so it is set aside rather than
    // fought over. A `-r` someone typed alongside `--all` is a contradiction,
    // and `run` refuses it.
    if cli.all && matches.value_source("room") == Some(clap::parser::ValueSource::EnvVariable) {
        cli.room.clear();
    }
    // An exported-but-empty `X2ROCK_ROOM=` is no default, not a room named "".
    cli.room.retain(|room| !room.is_empty());
    // Decided before the command runs, so a failure knows how to report itself.
    let json = cli.command.json();
    if let Err(e) = run(cli).await {
        // The same answer the panic hook gives, for the paths that return a
        // broken pipe rather than panicking on it - `completions`, which writes
        // through `io::Write` and `?`. The reader left; there is no failure to
        // report, and no stream left to report it on.
        if e.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
        }) {
            std::process::exit(141);
        }
        if json {
            // Structured for an agent: the message it always printed, plus a
            // stable code, the fix command when the error carried one, and any
            // detail the hint attached (unknown_room's did_you_mean, say).
            let obj = hint::error_json(&e);
            eprintln!(
                "{}",
                serde_json::to_string(&obj).unwrap_or_else(|_| format!("{{\"error\":{e:?}}}"))
            );
        } else {
            eprintln!("Error: {e:#}");
        }
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    // The single room most commands act on: the first `--room`, bound from the
    // field (not a `&self` method) so it stays disjoint from `match cli.command`
    // moving the command out. Multi-room commands read `cli.room` (the whole
    // list) instead.
    let room = cli.room.first().map(String::as_str);
    // Refuse a misapplied --all before dispatch: most commands return from the
    // match below without ever reaching the fan-out, and a silently ignored
    // flag reads as whole-house semantics honored. `bookmarks` is exempt: its
    // own `-a/--all` ("include daemon history") shares clap's arg id with this
    // flag, so setting either sets both.
    // `--all` fans over group *coordinators*, so a ramp there would slide one
    // speaker per group and report it as the group - the one combination that
    // cannot be made honest. Several `--room` and `--each` both fan over
    // speakers, which is exactly what a ramp wants, so they are carried through
    // rather than refused.
    if let Command::Vol { ramp: true, .. } = &cli.command {
        ensure!(
            !cli.all,
            "--ramp slides one speaker at a time and --all fans over groups; \
             name the rooms with --room, or use --each for one group's members"
        );
    }
    // `--all` is global, so it reaches every subcommand - and two of them give
    // it their own meaning rather than "every room". `bookmarks --all` includes
    // daemon-noticed history; `unlink --all` wipes every stored token. Both
    // must be let past the fan-out guard below, which otherwise refuses them as
    // "not a per-room command" - which is how `unlink --all` came to error from
    // the day it shipped, the global flag shadowing the subcommand's own.
    if cli.all && !commands::all_is_the_commands_own(&cli.command) {
        ensure!(
            cli.room.is_empty(),
            "--all already means every room; drop the -r (an exported X2ROCK_ROOM is set aside on its own)"
        );
        ensure!(
            fans_out(&cli.command),
            "--all applies only to the per-room commands (volume, transport, repeat, shuffle, crossfade)"
        );
    }
    match cli.command {
        Command::Discover => return household::discover_and_remember().await,
        Command::Households { json, redact } => {
            return household::run_households(json, redact).await;
        }
        Command::Skill {
            agent,
            ref dir,
            print,
            remove,
        } => {
            return admin::handle_skill(agent, dir.as_deref(), print, remove);
        }
        Command::Desktop { action, force } => return admin::desktop(action, force),
        Command::Service { action, json } => {
            return admin::service(action, json, cli.household.as_deref());
        }
        Command::Completions {
            shell,
            install,
            uninstall,
        } => return admin::completions(shell, install, uninstall),
        Command::Complete {
            ref what,
            ref prefix,
        } => {
            return completions::complete(what, prefix.as_deref(), &mut std::io::stdout());
        }
        Command::PlayItem {
            ref service,
            ref id,
            ref title,
            ref kind,
        } => {
            return content::run_play_item(
                cli.ip,
                cli.household.as_deref(),
                room,
                service,
                kind.as_deref(),
                id,
                title.as_ref(),
            )
            .await;
        }
        Command::Stations {
            ref query,
            ref tag,
            ref country,
            limit,
            play,
            no_wait,
            json,
        } => {
            return stream::run_stations(
                cli.ip,
                cli.household.as_deref(),
                room,
                query.as_deref(),
                tag.as_deref(),
                country.as_deref(),
                limit,
                play,
                no_wait,
                json,
            )
            .await;
        }
        Command::PlayUrl {
            ref url,
            ref title,
            no_wait,
            json,
        } => {
            return stream::run_play_url(
                cli.ip,
                cli.household.as_deref(),
                room,
                url,
                title.as_deref(),
                no_wait,
                json,
            )
            .await;
        }
        Command::QueueItem {
            ref service,
            ref id,
            ref title,
            ref kind,
        } => {
            return content::run_queue_item(
                cli.ip,
                cli.household.as_deref(),
                room,
                service,
                kind.as_deref(),
                id,
                title.as_ref(),
            )
            .await;
        }
        Command::Browse {
            ref service,
            ref container,
            count,
            index,
            play,
            refresh,
            json,
        } => {
            return services::run_browse(
                cli.ip,
                cli.household.as_deref(),
                room,
                service.as_ref(),
                container.as_deref(),
                count,
                index,
                play,
                refresh,
                json,
            )
            .await;
        }
        Command::Link {
            ref service,
            json,
            no_open,
            ref nickname,
            no_match,
            from_player,
            from_household,
            callback_port,
            dry_run,
        } => {
            return services::run_link(
                cli.ip,
                cli.household.as_deref(),
                service.as_ref(),
                json,
                no_open,
                nickname.as_ref(),
                no_match,
                from_player,
                from_household,
                callback_port,
                dry_run,
            )
            .await;
        }
        // Both of these are about a file on this machine, so neither needs a
        // player and both work with the household unreachable.
        Command::Unlink {
            ref service,
            all,
            ref account,
        } => {
            return services::unlink(
                service.as_deref(),
                all,
                account.as_deref(),
                cli.household.as_deref(),
            );
        }
        Command::Bookmarks {
            ref action,
            ref query,
            all,
            json,
        } => {
            return content::run_bookmarks(action.as_ref(), query.as_deref(), all, json);
        }
        Command::Accounts {
            content,
            ref prefer,
            json,
        } => {
            return services::accounts(
                cli.ip,
                cli.household.as_deref(),
                room,
                content,
                prefer.as_deref(),
                json,
            )
            .await;
        }
        Command::Search {
            ref term,
            ref service,
            ref category,
            all_categories,
            only_linked,
            per_service,
            count,
            index,
            play,
            refresh,
            json,
        } => {
            return services::run_search(
                cli.ip,
                cli.household.as_deref(),
                room,
                term.as_ref(),
                service.as_ref(),
                category.as_ref(),
                all_categories,
                only_linked,
                per_service,
                count,
                index,
                play,
                refresh,
                json,
            )
            .await;
        }
        // Before the household session below: the TUI reads the daemon, which has
        // its own connection, and opening a second one here would be a
        // connection nothing in the TUI ever uses.
        Command::Tui => return tui::run(cli.ip).await,
        Command::Daemon {
            verbose,
            log_events,
        } => {
            daemon::init_logging(verbose, log_events);
            tokio::select! {
                result = daemon::run(cli.ip, cli.household.as_deref()) => return result,
                signal = stop_signal() => {
                    eprintln!("x2rock: stopping on {signal}");
                    return Ok(());
                }
            }
        }
        _ => {}
    }

    let mut state = State::load()?;
    let session = session::connect(cli.ip, &mut state, cli.household.as_deref(), room).await?;

    if let Command::Rooms { json } = cli.command {
        status::print_rooms(&session.groups, json);
        return Ok(());
    }

    // Like `rooms`, this is a whole-household view and must not be forced to a
    // single group; it queries every coordinator itself, so it runs here rather
    // than after the single-room resolution below.
    if let Command::Status { json, full } = cli.command {
        return status::print_status(&session, json, full).await;
    }

    // Favorites belong to the household, not a group, so listing them needs no
    // room and works when several groups would otherwise force a choice.
    if let Command::Favorites { query, json } = &cli.command {
        return content::favorites(&session, query.as_deref(), *json).await;
    }

    // Every speaker has its own firmware, so this asks each rather than the
    // group's coordinator - and needs no target at all.
    if let Command::Update { json } = &cli.command {
        return household::update(&session, *json).await;
    }

    // Players, not rooms - so this reads the topology rather than `getGroups`,
    // which has no word for a Sub. One player answers for the whole household,
    // and each one is then asked to describe itself.
    if let Command::System { json, redact } = &cli.command {
        return household::system(&session, *json, *redact).await;
    }

    // Per-player too, and for the same reason: a battery belongs to a speaker,
    // not to whichever group it happens to be in.
    if let Command::Battery { room: one, json } = &cli.command {
        return household::battery(&session, one.as_deref().or(room), *json).await;
    }

    // Household-wide, and addressed by id rather than by room, so these run
    // before a target is resolved - `alarms` in a two-group house must not
    // demand a --room it has no use for.
    if let Command::Alarms { action, json } = &cli.command {
        return speaker::alarms(&session, room, action.as_ref(), *json).await;
    }

    if let Command::Alarm { id, action } = &cli.command {
        return speaker::alarm(&session, *id, action).await;
    }

    if let Command::Raw {
        transport:
            RawTransport::Upnp {
                service,
                action,
                args,
                scope,
            },
    } = &cli.command
    {
        return raw::raw_upnp(&session, room, service, action, args, *scope).await;
    }

    if let Command::Raw {
        transport:
            RawTransport::Api {
                namespace,
                command,
                options,
                scope,
                watch,
                session: session_id,
            },
    } = &cli.command
    {
        return raw::api(
            &session,
            room,
            namespace,
            command,
            options.as_deref(),
            *scope,
            *watch,
            session_id.as_deref(),
        )
        .await;
    }

    // Grouping resolves rooms itself: `ungroup` names its room positionally and
    // must work without --room, which the shared target resolution below would
    // refuse while the household has several groups.
    if let Command::Group { rooms } = &cli.command {
        return household::group(&session, room, rooms).await;
    }

    if let Command::Party { mode } = &cli.command {
        return household::party(&session, room, mode.as_deref()).await;
    }

    if let Command::Ungroup { room } = &cli.command {
        return household::ungroup(&session, room).await;
    }

    // `vol --each` sets every speaker in one group individually - the flatten
    // the group slider cannot do, because the slider preserves the members'
    // balance to match the Sonos app. It desugars to fanning `--player` over
    // the group's own members, read from the current topology, so all the
    // per-player machinery (per-speaker connection, clamp, fixed-volume
    // refusal, json) is reused rather than duplicated.
    if let Command::Vol {
        each: true,
        change,
        ramp,
        json,
        ..
    } = &cli.command
    {
        return volume::each(
            &session,
            room,
            cli.all,
            cli.room.len(),
            change.clone(),
            *ramp,
            *json,
        )
        .await;
    }

    // --all fans a per-room command across every group, resolved by each
    // group's coordinator name (a real room name; the composite group name is
    // not addressable). Already vetted against fans_out at the top of run().
    if cli.all {
        let every: Vec<String> = session
            .groups
            .groups
            .iter()
            .filter_map(|g| session.groups.player(&g.coordinator_id))
            .map(|p| p.name.clone())
            .collect();
        return fan_out(&session, &every, &cli.command).await;
    }

    // Several --room fan a per-room command across each, topology already in
    // hand from the one connect above. A single --room (or none) falls through
    // to the ordinary path; more than one on a command that does not fan out is
    // an error, not a silent act on the first.
    if cli.room.len() > 1 {
        if !fans_out(&cli.command) {
            return Err(too_many_rooms());
        }
        return fan_out(&session, &cli.room, &cli.command).await;
    }

    let target = session::target(&session.groups, room)?;
    let player = session::coordinator(&session, &target).await?;
    let group = target.group_id.as_str();

    match cli.command {
        Command::Now { json } => status::now(&player, &target, json).await?,
        Command::Rate {
            direction,
            refresh,
            json,
        } => services::run_rate(&player, group, &target.name, direction, refresh, json).await?,
        Command::Play { track: None } => play_or_resume(&session, &player, &target).await?,
        Command::Play { track: Some(n) } => playback::play_track(&player, &target, n).await?,
        Command::Keep { name, container } => {
            content::keep(&session, &player, group, name, container).await?;
        }
        Command::Bookmark { query, next } => {
            content::bookmark(&session, &player, &target, room, &query, next).await?;
        }
        Command::Favorite { query } => {
            content::favorite(&session, &player, &target, &query).await?;
        }
        Command::Playlist { query } => {
            content::playlist(&session, &player, &target, &query).await?;
        }
        Command::Tv => speaker::tv(&session, &player, &target, room).await?,
        Command::Chime { volume } => {
            stream::play_audio_clip(&session, &target, room, None, volume).await?;
        }
        Command::Notify { url, volume } => {
            stream::require_http_url(&url)?;
            stream::play_audio_clip(&session, &target, room, Some(&url), volume).await?;
        }
        Command::Eq {
            bass,
            treble,
            loudness,
            json,
            trueplay,
            night,
            dialog,
        } => {
            let want = ToneRequest {
                bass,
                treble,
                loudness,
                trueplay,
                night,
                dialog,
            };
            apply_eq(&session, &target, room, want, json).await?;
        }
        Command::Queue { action, json } => content::queue(&player, &target, action, json).await?,
        Command::Repeat { mode, json } => apply_repeat(&session, &target, mode, json).await?,
        Command::Shuffle { mode, json } => apply_shuffle(&session, &target, mode, json).await?,
        Command::Crossfade { mode, json } => apply_crossfade(&session, &target, mode, json).await?,
        Command::Remote {
            feedback,
            repeater,
            json,
        } => apply_remote(&session, &target, room, feedback, repeater, json).await?,
        Command::Rename { name } => {
            apply_rename(&session, &mut state, &target, room, &name).await?;
        }
        Command::Led { mode, json } => apply_led(&session, &target, room, mode, json).await?,
        Command::Buttons { mode, json } => {
            apply_buttons(&session, &target, room, mode, json).await?;
        }
        Command::Sleep { duration, json } => {
            apply_sleep(&target, player.ip(), duration, json).await?;
        }
        Command::Snooze { duration, json } => {
            apply_snooze(&target, player.ip(), duration, json).await?;
        }
        Command::Pause => playback::transport(&player, group, "pause").await?,
        Command::Toggle => playback::transport(&player, group, "togglePlayPause").await?,
        Command::Next => playback::transport(&player, group, "skipToNextTrack").await?,
        Command::Prev => playback::transport(&player, group, "skipToPreviousTrack").await?,
        Command::Vol {
            change,
            player: one_room,
            ramp,
            json,
            ..
        } => volume::apply_vol(&session, &target, room, change, one_room, ramp, json).await?,
        Command::Rooms { .. }
        | Command::Status { .. }
        | Command::Favorites { .. }
        | Command::Alarms { .. }
        | Command::Alarm { .. }
        | Command::Update { .. }
        | Command::System { .. }
        | Command::Battery { .. }
        | Command::Group { .. }
        | Command::Ungroup { .. }
        | Command::Party { .. }
        | Command::Raw { .. }
        | Command::Bookmarks { .. }
        | Command::Search { .. }
        | Command::PlayItem { .. }
        | Command::PlayUrl { .. }
        | Command::Stations { .. }
        | Command::QueueItem { .. }
        | Command::Browse { .. }
        | Command::Link { .. }
        | Command::Unlink { .. }
        | Command::Accounts { .. }
        | Command::Discover
        | Command::Households { .. }
        | Command::Skill { .. }
        | Command::Desktop { .. }
        | Command::Service { .. }
        | Command::Completions { .. }
        | Command::Complete { .. }
        | Command::Tui
        | Command::Daemon { .. } => unreachable!("handled above"),
    }
    Ok(())
}
