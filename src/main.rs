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
mod tui;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{CommandFactory, FromArgMatches};
use serde_json::json;

use cli::{Cli, Command, RawScope, RawTransport, UpnpScope};
use commands::playback::{
    apply_crossfade, apply_repeat, apply_shuffle, apply_transport, play_or_resume,
};
use commands::speaker::{
    ToneRequest, apply_buttons, apply_eq, apply_led, apply_remote, apply_rename, apply_sleep,
    apply_snooze, named_speaker,
};
use commands::volume::apply_vol;
use commands::{admin, content, household, playback, services, speaker, status, stream, volume};

use sonos::local::Connection;
use sonos::upnp::{self, Upnp};
use state::State;

/// `raw upnp`: one SOAP action against one speaker.
///
/// Separate from the Control API path rather than folded into it because
/// almost nothing is shared: a different transport, a different address (a
/// player, never a group id), a different argument shape, and a different
/// answer. What they do share is the contract that **a refusal is a result** -
/// a UPnP fault prints and exits 0, so a probe that discovers an action is
/// unsupported has succeeded at what it was for.
async fn raw_upnp(
    session: &session::Session,
    room: Option<&str>,
    service: &str,
    action: &str,
    args: &[String],
    scope: UpnpScope,
) -> Result<()> {
    ensure!(
        !action.is_empty()
            && action
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && action.chars().all(|c| c.is_ascii_alphanumeric()),
        "{action:?} is not a usable action name - UPnP action names are letters \
         and digits, starting with a letter"
    );
    let Some(entry) = upnp::service_entry(service) else {
        let names: Vec<&str> = upnp::SERVICES.iter().map(|s| s.name).collect();
        bail!(
            "no UPnP service named {service:?}. There are {}: {}",
            names.len(),
            names.join(", ")
        );
    };

    // SOAP arguments are a flat list of named strings. Split on the first `=`
    // only: values carry URIs, and a URI carries `=`.
    let mut parsed = Vec::with_capacity(args.len());
    for arg in args {
        let Some((name, value)) = arg.split_once('=') else {
            bail!(
                "UPnP arguments are Name=Value pairs; {arg:?} has no `=`. \
                 Most actions need InstanceID=0."
            );
        };
        // The name becomes an XML tag verbatim - only the value is escaped -
        // so anything that is not a valid tag produces an opaque parse failure
        // from the player instead of a message pointing at the typo.
        ensure!(
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')),
            "{name:?} is not a usable argument name - UPnP argument names are \
             letters, digits, _ - and . , starting with a letter. Most actions \
             need InstanceID=0."
        );
        parsed.push((name.to_owned(), value.to_owned()));
    }

    // UPnP addresses a speaker, and `UpnpScope` has only the two that mean
    // something: `group` aims at the coordinator, the only player that answers
    // for the group's transport, and `player` at the room's own speaker, which
    // is what RenderingControl and DeviceProperties are per. The Control API's
    // household and unaddressed scopes are absent from the type rather than
    // rejected at runtime.
    let target = session::target(&session.groups, room)?;
    let ip = match scope {
        UpnpScope::Group => target
            .coordinator_ip
            .ok_or_else(|| anyhow!("no address for {}'s coordinator", target.name))?,
        // The same resolution every per-player command uses, so `raw upnp
        // --scope player` and `led` cannot drift on which speaker `--room`
        // means.
        UpnpScope::Player => named_speaker(session, &target, room)?.1.ip(),
    };

    match Upnp::new(ip).raw_action(entry, action, &parsed).await {
        Ok(out) if out.is_empty() => println!("{} {action}: ok, no output", entry.name),
        // An array of name/value pairs rather than an object, because a probe
        // is reading a shape it does not know yet: `serde_json::Map` is a
        // BTreeMap here (no `preserve_order` feature), so an object would
        // re-sort the player's own argument order alphabetically and keep only
        // the last of any repeated name. Both are exactly what `raw_action`
        // returns a Vec to avoid losing.
        Ok(out) => {
            let pairs: Vec<serde_json::Value> = out
                .into_iter()
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect();
            println!("{}", serde_json::to_string_pretty(&pairs)?);
        }
        // A *refusal* is the finding: the player was reached and said no, which
        // is a result worth printing and worth exiting 0 for, so a shell loop
        // over candidate actions is not stopped by the first unsupported one.
        // Anything else - an unreachable speaker, a timeout, an unparseable
        // envelope, or UPnP switched off for the whole household - is a real
        // failure and must propagate, or a script's `|| handle_failure` never
        // fires. The last one matters most for a loop over candidate actions,
        // which would otherwise conclude every service is unsupported.
        Err(e) if upnp::Fault::of(&e).is_some_and(upnp::Fault::is_per_action) => {
            eprintln!("{} {action}: {e:#}", entry.name)
        }
        Err(e) => return Err(e),
    }
    Ok(())
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

/// Fan a per-room command across several `--room`, topology resolved once. Only
/// the per-room-state commands accept it; anything else is refused with a clear
/// message rather than silently acting on the first room. A failure on one room
/// stops the run - a half-applied "set them all to 10" is worse than a clear
/// stop naming the room that failed.
async fn fan_out(session: &session::Session, rooms: &[String], command: &Command) -> Result<()> {
    let Some(action) = per_room(command) else {
        return Err(too_many_rooms());
    };
    for name in rooms {
        let target = session::target(&session.groups, Some(name))?;
        let outcome = match action {
            PerRoom::Vol {
                change,
                one_room,
                ramp,
                json,
            } => {
                apply_vol(
                    session,
                    &target,
                    Some(name),
                    change.clone(),
                    one_room,
                    ramp,
                    json,
                )
                .await
            }
            PerRoom::Repeat { mode, json } => {
                apply_repeat(session, &target, mode.clone(), json).await
            }
            PerRoom::Shuffle { mode, json } => {
                apply_shuffle(session, &target, mode.clone(), json).await
            }
            PerRoom::Crossfade { mode, json } => {
                apply_crossfade(session, &target, mode.clone(), json).await
            }
            // `play` alone confirms and resumes; the other verbs have nothing
            // to confirm against, and `pause` on an idle room is a no-op that
            // must not spend the failure budget.
            PerRoom::Transport("play") => match session::coordinator(session, &target).await {
                Ok(player) => play_or_resume(session, &player, &target).await,
                Err(e) => Err(e),
            },
            PerRoom::Transport(verb) => apply_transport(session, &target, verb).await,
        };
        // Name the room the batch stopped on: a fan-out that halts silently on
        // the third of five rooms is a debugging puzzle. The rooms before it
        // already applied; the ones after did not.
        outcome.with_context(|| format!("on room {name:?}"))?;
    }
    Ok(())
}

/// Several `--room` on a command that takes one. Its own code, not the generic
/// `error` bucket, so an agent drops the extra `--room` from the code rather
/// than parsing the sentence. No `fix` command: the remedy is to re-run with a
/// single `--room`, which is not a canned line.
fn too_many_rooms() -> anyhow::Error {
    hint::Hint::new(
        "several --room were given, but this command takes a single room",
        "too_many_rooms",
        None,
    )
    .into()
}

/// What a per-room command does to one room. Borrowed from the `Command`, so
/// [`fan_out`] can apply it to each room in turn without re-matching.
#[derive(Clone, Copy)]
enum PerRoom<'a> {
    Vol {
        change: &'a Option<String>,
        one_room: bool,
        ramp: bool,
        json: bool,
    },
    Repeat {
        mode: &'a Option<String>,
        json: bool,
    },
    Shuffle {
        mode: &'a Option<String>,
        json: bool,
    },
    Crossfade {
        mode: &'a Option<String>,
        json: bool,
    },
    /// A `playback:1` verb.
    Transport(&'static str),
}

/// The per-room reading of a command, or `None` for the read, whole-household
/// and single-target commands, which several `--room` do not fan out. The one
/// list: [`fans_out`] asks whether a command is on it and [`fan_out`] applies
/// what it finds, so a command cannot be admitted by one and missed by the
/// other - which is how `--all crossfade on` came to fail with "several --room
/// were given" on a command line that gave none.
fn per_room(command: &Command) -> Option<PerRoom<'_>> {
    Some(match command {
        Command::Vol {
            change,
            player,
            ramp,
            json,
            ..
        } => PerRoom::Vol {
            change,
            one_room: *player,
            ramp: *ramp,
            json: *json,
        },
        Command::Repeat { mode, json } => PerRoom::Repeat { mode, json: *json },
        Command::Shuffle { mode, json } => PerRoom::Shuffle { mode, json: *json },
        Command::Crossfade { mode, json } => PerRoom::Crossfade { mode, json: *json },
        Command::Play { track: None } => PerRoom::Transport("play"),
        Command::Pause => PerRoom::Transport("pause"),
        Command::Toggle => PerRoom::Transport("togglePlayPause"),
        Command::Next => PerRoom::Transport("skipToNextTrack"),
        Command::Prev => PerRoom::Transport("skipToPreviousTrack"),
        _ => return None,
    })
}

/// Whether a command applies per room, so several `--room` fan it out.
fn fans_out(command: &Command) -> bool {
    per_room(command).is_some()
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
    if cli.all && !matches!(cli.command, Command::Bookmarks { .. }) {
        ensure!(
            cli.room.is_empty(),
            "--all already means every room; drop the -r (an exported X2ROCK_ROOM is set aside on its own)"
        );
        ensure!(
            fans_out(&cli.command),
            "--all applies only to the per-room commands (volume, transport, repeat, shuffle)"
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
        Command::Desktop { action } => return admin::desktop(action),
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
            no_open,
            ref nickname,
            no_match,
            from_player,
        } => {
            return services::run_link(
                cli.ip,
                cli.household.as_deref(),
                service.as_ref(),
                no_open,
                nickname.as_ref(),
                no_match,
                from_player,
            )
            .await;
        }
        // Both of these are about a file on this machine, so neither needs a
        // player and both work with the household unreachable.
        Command::Unlink { ref service } => return services::unlink(service),
        Command::Bookmarks {
            ref action,
            ref query,
            all,
            json,
        } => {
            return content::run_bookmarks(action.as_ref(), query.as_deref(), all, json);
        }
        Command::Accounts { content, json } => {
            return services::accounts(cli.ip, cli.household.as_deref(), room, content, json).await;
        }
        Command::Search {
            ref term,
            ref service,
            ref category,
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
        return raw_upnp(&session, room, service, action, args, *scope).await;
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
        let options: serde_json::Value = match options.as_deref() {
            None => json!({}),
            Some(text) => serde_json::from_str(text)
                .with_context(|| format!("options must be a JSON object: {text}"))?,
        };
        ensure!(
            options.is_object(),
            "options must be a JSON object, not {}",
            match &options {
                serde_json::Value::Array(_) => "an array",
                serde_json::Value::Null => "null",
                _ => "a scalar",
            }
        );

        let mut envelope = json!({ "namespace": namespace, "command": command });
        // Group commands are answered by the coordinator, so a probe that does
        // not go there measures the wrong player's refusal.
        let mut connection = session.connection.clone();
        // A session id is an explicit address, so it wins over --scope rather
        // than combining with it: the two would name different targets.
        if let Some(id) = session_id {
            envelope["sessionId"] = json!(id);
        }
        match scope {
            _ if session_id.is_some() => {}
            RawScope::Household => {
                envelope["householdId"] = json!(session.connection.household_id().await?);
            }
            RawScope::Group => {
                let target = session::target(&session.groups, room)?;
                envelope["groupId"] = json!(target.group_id);
                connection = session::coordinator(&session, &target).await?;
            }
            RawScope::Player => {
                // A player answers player-scoped commands only for itself, so
                // naming one over a socket to another gets ERROR_INVALID_OBJECT_ID
                // - "Incorrect playerId" - for an id that is perfectly correct.
                let player = match room {
                    Some(room) => session.groups.player_named(room)?,
                    None => {
                        let id = session.groups.resolve(None)?.coordinator_id.clone();
                        session.groups.player(&id).ok_or_else(|| {
                            anyhow!("group coordinator {id} is not a known player")
                        })?
                    }
                };
                envelope["playerId"] = json!(player.id);
                if let Some(ip) = player.ip()
                    && ip != connection.ip()
                {
                    connection = Connection::open(ip).await?;
                }
            }
            RawScope::None => {}
        }

        // Attached before the command is sent: a subscribe can be answered by an
        // event that overtakes the reply, and a receiver created afterwards
        // would miss exactly the thing the probe went to see.
        let mut events = connection.events();

        let (header, body) = connection.command(envelope, options).await?;
        if header.success != Some(true) {
            let err: sonos::proto::ErrorBody =
                serde_json::from_value(body.clone()).unwrap_or_default();
            eprintln!(
                "{namespace} {command}: {}{}",
                err.error_code.as_deref().unwrap_or("refused"),
                err.reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "header": serde_json::to_value(&header)?,
                "body": body,
            }))?
        );

        if let Some(seconds) = watch {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(*seconds);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, events.recv()).await {
                    Err(_) => break,
                    Ok(Err(_)) => break,
                    Ok(Ok(event)) => {
                        if event.kind == sonos::proto::Event::LOST {
                            eprintln!("connection lost");
                            break;
                        }
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "event": event.kind,
                                "namespace": event.namespace,
                                "groupId": event.group_id,
                                "playerId": event.player_id,
                                "body": event.body,
                            }))?
                        );
                    }
                }
            }
        }
        return Ok(());
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
            content::keep(&session, &player, group, name, container).await?
        }
        Command::Bookmark { query, next } => {
            content::bookmark(&session, &player, &target, room, &query, next).await?
        }
        Command::Favorite { query } => {
            content::favorite(&session, &player, &target, group, &query).await?
        }
        Command::Playlist { query } => {
            content::playlist(&session, &player, &target, group, &query).await?
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
            apply_eq(&session, &target, room, want, json).await?
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
            apply_rename(&session, &mut state, &target, room, &name).await?
        }
        Command::Led { mode, json } => apply_led(&session, &target, room, mode, json).await?,
        Command::Buttons { mode, json } => {
            apply_buttons(&session, &target, room, mode, json).await?
        }
        Command::Sleep { duration, json } => {
            apply_sleep(&target, player.ip(), duration, json).await?
        }
        Command::Snooze { duration, json } => {
            apply_snooze(&target, player.ip(), duration, json).await?
        }
        Command::Pause => player.playback(group, "pause").await?,
        Command::Toggle => player.playback(group, "togglePlayPause").await?,
        Command::Next => player.playback(group, "skipToNextTrack").await?,
        Command::Prev => player.playback(group, "skipToPreviousTrack").await?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn only_the_per_room_commands_fan_out() {
        // These act on one room's state, so several --room fan them out.
        assert!(fans_out(&Command::Pause));
        assert!(fans_out(&Command::Toggle));
        assert!(fans_out(&Command::Next));
        assert!(fans_out(&Command::Play { track: None }));
        assert!(fans_out(&Command::Vol {
            change: None,
            player: false,
            each: false,
            ramp: false,
            json: false
        }));
        assert!(fans_out(&Command::Repeat {
            mode: None,
            json: false
        }));
        assert!(fans_out(&Command::Shuffle {
            mode: None,
            json: false
        }));
        assert!(fans_out(&Command::Crossfade {
            mode: None,
            json: false
        }));
        // Playing a specific queue position is per-queue, not a broadcast.
        assert!(!fans_out(&Command::Play { track: Some(3) }));
        // Reads and whole-household commands are not fanned out.
        assert!(!fans_out(&Command::Now { json: false }));
        assert!(!fans_out(&Command::Status {
            json: false,
            full: false
        }));
        assert!(!fans_out(&Command::Rooms { json: false }));
    }

    /// A ramp slides one speaker, so it composes with the fan-outs that are
    /// *over speakers* and not with the one that is over groups. Only `--all`
    /// is refused, and that refusal lives in `run()` rather than in clap
    /// because `--all` is a global flag - parsing must succeed for the message
    /// to be able to name the reason.
    #[test]
    fn ramp_composes_with_the_per_speaker_fan_outs_but_not_with_all() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("x2rock").chain(args.iter().copied()))
        };
        for ok in [
            vec!["-r", "Kitchen", "vol", "30", "--ramp"],
            // Several rooms: the fan-out iterates the names as typed, each
            // resolved to its own speaker.
            vec!["-r", "a", "-r", "b", "vol", "30", "--ramp"],
            // --each rebuilds the command as --player over the group's members,
            // which is the shape a ramp already needs.
            vec!["vol", "30", "--ramp", "--each"],
        ] {
            assert!(parse(&ok).is_ok(), "{ok:?} should parse");
        }
        // Parses, then refused in run() - pinned by the fan-out test below.
        assert!(parse(&["--all", "vol", "30", "--ramp"]).is_ok());
    }

    /// The flag has to survive `per_room`'s rebuild, which is where it was
    /// previously dropped: a ramp that silently became a jump would look like
    /// the command simply ignoring `--ramp`.
    #[test]
    fn the_fan_out_carries_ramp_rather_than_dropping_it() {
        let command = Command::Vol {
            change: Some("30".into()),
            player: false,
            each: false,
            ramp: true,
            json: false,
        };
        match per_room(&command).expect("vol fans out") {
            PerRoom::Vol { ramp, .. } => assert!(ramp, "per_room dropped --ramp"),
            _ => panic!("expected PerRoom::Vol"),
        }
    }
}
