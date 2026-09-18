//! The music services: linking an account and forgetting it, what this
//! machine holds, and the two ways into a service's catalogue - `search` and
//! `browse` - plus `rate`, which is a SMAPI extension per service rather than
//! a Sonos feature. The token dance around a search that comes back with a
//! refreshed credential lives here too, since search and browse are the two
//! callers. Playing a hit is `content::play_item`.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;

use super::content::{play_item, queueable};
use crate::cli::RateDirection;
use crate::session;
use crate::sonos::local::Connection;
use crate::sonos::proto::Player;
use crate::sonos::upnp::{self, Upnp};
use crate::state::State;
use crate::{catalogue, credentials, hint, sonos};

/// Persist a token a service handed back inside a `tokenRefreshRequired`
/// fault, so the next command does not pay for the same refresh again.
///
/// Takes an already-loaded store rather than loading its own - every caller
/// either already has one in scope from resolving the account in the first
/// place, or is about to load one fresh for this alone, and hiding a second
/// load inside this function was paying for the same file twice in the one
/// command this feature exists to make cheaper.
///
/// A blank `private_key` is not trusted the way [`sonos::smapi::parse_device_auth`]
/// trusts one on a fresh link (there, an honest empty key is the fair test of
/// whether the service meant it). Here it would silently overwrite a key that
/// has been working, for a service that may simply not have sent one this
/// time - so the existing key is kept instead when the refresh's is empty.
///
/// Best-effort and silent otherwise: the refreshed token already did its job
/// for this call, in memory, whether or not it reaches disk, and a link that
/// predates this feature (or was somehow removed mid-command) is nothing to
/// report - there is no account left to attach the refresh to.
pub fn save_refreshed_token(
    creds: &mut credentials::Credentials,
    service_id: &str,
    refreshed: sonos::smapi::RefreshedToken,
) {
    let Some(existing) = creds.get(service_id).cloned() else {
        return;
    };
    let private_key = if refreshed.private_key.is_empty() {
        existing.private_key.clone()
    } else {
        refreshed.private_key
    };
    creds.remember(
        service_id,
        credentials::Account {
            auth_token: refreshed.auth_token,
            private_key,
            user_id_hash_code: refreshed.user_id_hash_code,
            ..existing
        },
    );
    let _ = creds.save();
}

/// The token to use for whatever comes right after a call that may have
/// refreshed it - the refreshed one, persisted along the way, or the original
/// unchanged. Without this, a `browse`/`search --play` that had to refresh
/// mid-call handed the very next call (`play_item`) the token that call had
/// just proven stale.
///
/// Loads its own store: callers of this one are past the point of having a
/// `Credentials` already open for another reason, so there is nothing to
/// avoid reloading. `save_refreshed_token` is the one that matters for reuse.
fn use_refreshed_token(
    service_id: &str,
    token: Option<sonos::smapi::Token>,
    refreshed: Option<sonos::smapi::RefreshedToken>,
) -> Option<sonos::smapi::Token> {
    let Some(new_token) = refreshed else {
        return token;
    };
    let key = if new_token.private_key.is_empty() {
        token.as_ref().map(|t| t.key.clone()).unwrap_or_default()
    } else {
        new_token.private_key.clone()
    };
    let household = token.and_then(|t| t.household);
    if let Ok(mut creds) = credentials::Credentials::load() {
        save_refreshed_token(&mut creds, service_id, new_token.clone());
    }
    Some(sonos::smapi::Token {
        token: new_token.auth_token,
        key,
        household,
    })
}

/// A rough age, for a list where the exact second has never mattered.
fn ago(then: u64) -> String {
    let now = credentials::now();
    let seconds = now.saturating_sub(then);
    match seconds {
        0..=90 => "just now".to_string(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// What the household should call this machine's account, when nothing was given.
///
/// The hostname, because a household can have several machines linked to the
/// same service account and the Sonos app shows this string.
fn default_nickname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| format!("x2rock on {h}"))
        .unwrap_or_else(|| "x2rock".to_string())
}

/// Hand a URL to whatever browser the person already uses.
///
/// Spawned and not waited on: `xdg-open` stays attached to some handlers for as
/// long as the browser lives, and the polling loop below is what should be
/// running, not a wait.
fn open_in_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("running xdg-open")?;
    Ok(())
}

/// Put a link page in front of the person, or print it when asked to.
///
/// A failure to open is not a failure to link: the URL is right there, and
/// printing it is the one path that matters over ssh.
fn announce_link_page(name: &str, url: &str, no_open: bool) {
    if no_open {
        println!("Open this and log in to {name}:\n\n  {url}\n");
        return;
    }
    match open_in_browser(url) {
        Ok(()) => println!("Opened {name} in your browser."),
        Err(e) => println!("Could not open a browser ({e:#}). Open this yourself:\n\n  {url}\n"),
    }
}

/// `x2rock rate`: rate the currently playing track up or down.
///
/// Only Pandora-shaped radio features offer this at all, and only on a track
/// that has one - **not** a Live broadcast, whose current track carries no
/// per-track id of any kind (verified against a real household, 2026-09-12:
/// `currentItem.track.id` is simply absent, not merely unratable). That
/// absence is the whole gate; nothing here special-cases "is this Live".
///
/// The id is read once, up front, and the two calls that follow
/// (`extended_metadata`, `rate_item`) both use it even if the track changes
/// mid-command. Accepted rather than fixed: a rating sent for the track that
/// was playing when the command was run is the track the person meant to
/// rate, and re-checking would only mean rating (or silently not rating)
/// whatever replaced it - a stranger outcome than the one this leaves in
/// place.
pub async fn run_rate(
    player: &Connection,
    group: &str,
    room_name: &str,
    direction: RateDirection,
    refresh: bool,
    json: bool,
) -> Result<()> {
    // Independent of each other - one is the Control API over the socket that
    // is already open, the other a UPnP `ListAvailableServices` to the same
    // player - so they overlap rather than queue. `refresh` is a no-op when
    // the player's service-list version has not moved.
    let mut catalogue = catalogue::Catalogue::load();
    let upnp = Upnp::new(player.ip());
    let (meta, dirty) =
        tokio::try_join!(player.metadata(group), catalogue.refresh(&upnp, refresh))?;
    let track_id = meta
        .current_item
        .as_ref()
        .and_then(|i| i.track.as_ref())
        .and_then(|t| t.id.as_ref())
        .filter(|id| id.is_real())
        .ok_or_else(|| {
            anyhow!(
                "nothing rateable is playing in {room_name} (a Live broadcast's track carries \
                 no id to rate)"
            )
        })?;
    let service_id = track_id
        .service_id
        .as_deref()
        .ok_or_else(|| anyhow!("the current track names no service, so it cannot be rated"))?;

    let service = catalogue
        .by_id(service_id)
        .ok_or_else(|| anyhow!("service {service_id} is not in this player's service list"))?
        .clone();

    // Asked before the call for the same reason `search` asks before
    // `categories_for`: a freshly learned *empty* list is exactly what is
    // worth writing, and `is_empty()` afterwards cannot tell that from a hit.
    let learned = !catalogue.ratings_cached(&service.id);
    let ratings = catalogue.ratings_for(&service).await?;
    if dirty || learned {
        catalogue.save()?;
    }
    ensure!(
        !ratings.is_empty(),
        "{} publishes no ratings, so nothing here can be rated.{}",
        service.name,
        // Only worth suggesting when the answer came from cache: a reply just
        // fetched says what the service currently says, and re-fetching it is
        // a round trip that cannot change the outcome.
        if learned {
            ""
        } else {
            " (Cached; `x2rock rate --refresh` re-reads it if the service has since added them.)"
        }
    );

    let linked = credentials::Credentials::load()?;
    let token = linked.token_for(&service.id);

    let mut refreshed = None;
    let properties = sonos::smapi::extended_metadata(
        &service,
        token.as_ref(),
        &track_id.object_id,
        &mut refreshed,
    )
    .await?;
    let token = use_refreshed_token(&service.id, token, refreshed);

    let up = direction == RateDirection::Up;
    let chosen = properties
        .iter()
        .find_map(|(propname, value)| {
            sonos::smapi::RatingsMatch::find(&ratings, propname, value, up)
        })
        .ok_or_else(|| {
            anyhow!(
                "{} did not report a rating state for the current track, so it cannot be rated \
                 right now.",
                service.name
            )
        })?;

    let mut refreshed = None;
    let result = sonos::smapi::rate_item(
        &service,
        token.as_ref(),
        &track_id.object_id,
        &chosen.id,
        &mut refreshed,
    )
    .await?;
    // Nothing follows that needs the token; the call is for its save.
    let _ = use_refreshed_token(&service.id, token, refreshed);

    // The rating already landed - a failure here must not read as the rating
    // itself having failed, which `?` would do (and which could send a caller
    // that retries on error back to rate the same track twice). Warn and move
    // on; the room simply keeps playing what it was.
    let skipped = result.should_skip == Some(true)
        && match player.playback(group, "skipToNextTrack").await {
            Ok(()) => true,
            Err(e) => {
                eprintln!("x2rock: rated successfully, but the requested skip failed: {e:#}");
                false
            }
        };

    let word = if up { "up" } else { "down" };
    if json {
        println!(
            "{}",
            json!({
                "service": service.name,
                "rating": word,
                "should_skip": result.should_skip,
                "skipped": skipped,
                "message": result.message_string_id,
            })
        );
    } else {
        let skip_note = if skipped {
            " — skipping"
        } else if result.should_skip == Some(true) {
            " (skip requested but failed; see stderr)"
        } else {
            ""
        };
        println!("Rated {word} on {}{skip_note}", service.name);
    }
    Ok(())
}

/// `x2rock link`: the device-link flow, end to end.
///
/// A player is required, unlike search: the link is minted *for a household*,
/// and its id is in both SMAPI calls. The browser step is the person's own
/// browser, and the whole interaction for a service like Bandcamp is: open a
/// link, log in, done.
pub async fn run_link(
    ip: Option<IpAddr>,
    household: Option<&str>,
    service: Option<&String>,
    no_open: bool,
    nickname: Option<&String>,
    no_match: bool,
    from_player: bool,
) -> Result<()> {
    let mut linked = credentials::Credentials::load()?;
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state, household, None).await?;
    let mut catalogue = catalogue::Catalogue::load();
    if catalogue
        .refresh(&Upnp::new(session.connection.ip()), false)
        .await?
    {
        catalogue.save()?;
    }

    let Some(query) = service else {
        let linkable = catalogue.linkable();
        println!("{} services can be linked:", linkable.len());
        for s in &linkable {
            let mark = match linked.get(&s.id) {
                Some(a) => format!("  (linked {})", ago(a.linked)),
                None => String::new(),
            };
            println!("  {}{mark}", s.name);
        }
        println!("\nLink one with: x2rock link <service>");
        println!(
            "App-link services are not listed, but naming one still asks it \
             for a browser page - some hand one over, and a refusal costs \
             nothing."
        );
        return Ok(());
    };

    let chosen = catalogue.find_any(query)?.clone();
    ensure!(
        chosen.auth != sonos::smapi::Auth::Anonymous,
        "{} needs no account at all - search it as it is.",
        chosen.name
    );

    let household = session.connection.household_id().await?;

    // Plex first: its SMAPI link half is dead in both flavours - `getAppLink`
    // answers `Server.ServiceUnknownError` and `getDeviceLinkCode` answers
    // `Client.AuthTokenExpired`, whatever the credentials header carries - and
    // the token its content half wants is a plain Plex account token, which
    // Plex's own published PIN flow mints for any client. See sonos/plex.rs.
    // From out here it is the same flow as every other link: open a page,
    // wait, store.
    let (auth, link_code) = if from_player {
        ensure!(
            chosen.id == sonos::plex::SERVICE_ID,
            "--from-player is a Plex-only path: only Plex puts a usable token \
             in the players' art URLs"
        );
        // The same move as `keep`: read what the player itself built. Every
        // Plex art URL a player hands out carries the household integration's
        // token, so any room currently on Plex is a source. Metadata is
        // group-scoped, so each group is asked through its own coordinator -
        // the session's socket only answers for the group it coordinates.
        let mut token = None;
        for group in &session.groups.groups {
            let target = session::Target {
                group_id: group.id.clone(),
                name: group.name.clone(),
                coordinator_id: group.coordinator_id.clone(),
                coordinator_ip: session
                    .groups
                    .player(&group.coordinator_id)
                    .and_then(|p| p.ip()),
            };
            let Ok(connection) = session::coordinator(&session, &target).await else {
                continue;
            };
            let Ok(status) = connection.metadata(&group.id).await else {
                continue;
            };
            let urls = [
                status
                    .container
                    .as_ref()
                    .and_then(|c| c.image_url.as_deref()),
                status
                    .current_item
                    .as_ref()
                    .and_then(|i| i.track.as_ref())
                    .and_then(|t| t.image_url.as_deref()),
            ];
            token = urls.into_iter().flatten().find_map(sonos::plex::token_in);
            if token.is_some() {
                break;
            }
        }
        let Some(token) = token else {
            bail!(
                "no room is showing Plex art right now, so there is no token \
                 to read. Play something from Plex in any room, then run this \
                 again - or use `x2rock link plex` for the browser flow."
            );
        };
        let auth = sonos::smapi::DeviceAuth {
            auth_token: token,
            private_key: String::new(),
            user_id_hash_code: None,
        };
        (auth, None)
    } else if chosen.id == sonos::plex::SERVICE_ID {
        let (pin, url) = sonos::plex::pin().await?;
        announce_link_page(&chosen.name, &url, no_open);
        let deadline = tokio::time::Instant::now() + sonos::smapi::LINK_DEADLINE;
        eprint!("Waiting for you to finish");
        let token = loop {
            match sonos::plex::poll(&pin).await {
                Ok(Some(token)) => {
                    eprintln!();
                    break token;
                }
                Ok(None) => {
                    use std::io::Write;
                    eprint!(".");
                    let _ = std::io::stderr().flush();
                }
                Err(e) => {
                    eprintln!();
                    return Err(e);
                }
            }
            if tokio::time::Instant::now() + sonos::smapi::LINK_POLL >= deadline {
                eprintln!();
                bail!(
                    "{} never confirmed the link. Run `x2rock link {}` again to start over.",
                    chosen.name,
                    chosen.name
                );
            }
            tokio::time::sleep(sonos::smapi::LINK_POLL).await;
        };
        let auth = sonos::smapi::DeviceAuth {
            auth_token: token,
            private_key: String::new(),
            user_id_hash_code: None,
        };
        (auth, None)
    } else {
        // Two flavours of the same browser flow. App link is named for handing
        // off to the service's own app, but that is the controller's choice -
        // Sonos's desktop controller links without one - so the reply nests the
        // identical regUrl/linkCode pair, and a service may fill it in with a
        // real page. Asking is the only way to know, and a refusal arrives
        // immediately with the service's own words in it. The catch-all arm
        // also takes whatever parse_services could not classify, which is why
        // the advice it prints does not name a mechanism.
        let code = match chosen.auth {
            sonos::smapi::Auth::DeviceLink => {
                sonos::smapi::device_link_code(&chosen, &household).await?
            }
            _ => sonos::smapi::app_link_code(&chosen, &household).await?,
        };
        announce_link_page(&chosen.name, &code.reg_url, no_open);
        if code.show_link_code {
            println!("Enter this code when asked:\n\n  {}\n", code.link_code);
        }

        let deadline = tokio::time::Instant::now() + sonos::smapi::LINK_DEADLINE;
        eprint!("Waiting for you to finish");
        let auth = loop {
            match sonos::smapi::device_auth_token(
                &chosen,
                &household,
                &code.link_code,
                code.link_device_id.as_deref(),
            )
            .await
            {
                Ok(Some(auth)) => {
                    eprintln!();
                    break auth;
                }
                Ok(None) => {
                    use std::io::Write;
                    eprint!(".");
                    let _ = std::io::stderr().flush();
                }
                Err(e) => {
                    eprintln!();
                    return Err(e);
                }
            }
            if tokio::time::Instant::now() + sonos::smapi::LINK_POLL >= deadline {
                eprintln!();
                bail!(
                    "{} never confirmed the link. Run `x2rock link {}` again to start over.",
                    chosen.name,
                    chosen.name
                );
            }
            tokio::time::sleep(sonos::smapi::LINK_POLL).await;
        };
        (auth, Some(code.link_code))
    };

    let nickname = nickname.cloned().unwrap_or_else(default_nickname);
    let hash = auth.user_id_hash_code.clone();
    let (id, account) = credentials::from_device_auth(
        &chosen.id,
        &chosen.name,
        Some(&household),
        Some(&nickname),
        auth,
    );
    // Stored before anything else is attempted. A link code is single-use, so
    // losing the token to a later failure would mean walking back through the
    // browser to fix something that already worked.
    linked.remember(&id, account);
    linked.save()?;
    println!(
        "Linked {}. Search it with: x2rock search -s {}",
        chosen.name, chosen.name
    );

    if no_match {
        return Ok(());
    }
    let Some(hash) = hash else {
        // Quiet when there is no link code either (Plex): registering on the
        // household was never part of that flow, and the household's own
        // registration - made from the Sonos app - is what playback rides on.
        if link_code.is_some() {
            println!(
                "{} sent no userIdHashCode, so the household was not asked about the \
                 account. Search and browse work. On-demand tracks play only if the \
                 household already has a {} account, added in the Sonos app; anything \
                 the service hands back as a playable stream plays either way.",
                chosen.name, chosen.name
            );
        }
        return Ok(());
    };
    match session
        .connection
        .match_music_service_account(
            &household,
            &chosen.id,
            &hash,
            &nickname,
            link_code.as_deref(),
        )
        .await
    {
        Ok(account_id) => {
            if let Some(entry) = linked.services.get_mut(&id) {
                entry.account_id = account_id.clone();
            }
            linked.save()?;
            match account_id {
                Some(id) => println!("The household knows this account as {id}."),
                None => println!("The household accepted the account."),
            }
        }
        // The token is already on disk and already useful, so this is a warning
        // and not an error: failing the command here would suggest the whole
        // flow needs repeating, and it does not.
        //
        // `match` has only ever succeeded for an account the household already
        // held (Spotify, after the Sonos app added it). A refusal does not prove
        // the household has none, though: iHeartRadio refused while the household
        // held two, and on-demand tracks played from its own.
        Err(e) => println!(
            "The household did not match the account ({e:#}). The token is stored, \
             and search and browse work. On-demand tracks play only if the household \
             has its own {} account, added in the Sonos app; anything the service hands \
             back as a playable stream plays either way.",
            chosen.name
        ),
    }
    Ok(())
}

/// `x2rock browse`: a music service's own containers, walked one level at a time.
///
/// The half of a linked service that `search` cannot reach. "Play something from
/// my playlists" is not a search - it names a place, not a word - and every
/// service puts those places behind `getMetadata` starting at `root`.
///
/// A player is wanted but not required, on the same terms as `search`: listing
/// what can be browsed comes from the on-disk catalogue, and only `--play` and a
/// first run with nothing cached genuinely need one.
#[allow(clippy::too_many_arguments)]
pub async fn run_browse(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    service: Option<&String>,
    container: Option<&str>,
    count: u32,
    index: u32,
    play: Option<usize>,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let mut state = State::load()?;
    let reached = session::connect(ip, &mut state, household, room).await;
    let live =
        || -> Result<&session::Session> { reached.as_ref().map_err(hint::no_player_to_play) };

    let mut catalogue = catalogue::Catalogue::load();
    match &reached {
        Ok(session) => {
            if catalogue
                .refresh(&Upnp::new(session.connection.ip()), refresh)
                .await?
            {
                catalogue.save()?;
            }
        }
        Err(e) if catalogue.services().is_empty() => return Err(anyhow!("{e:#}")),
        Err(e) => eprintln!("x2rock: no player reached, using the cached catalogue ({e:#})"),
    }

    let linked = credentials::Credentials::load()?;
    // Everything reachable, which is wider than what `search` offers. Browsing
    // needs an endpoint and, for a linked service, a token; searching needs a
    // published search category on top of that. This comment used to say the
    // two sets were the same - Radio Paloma is the counterexample, browse-only,
    // and filtering here would have removed the one route that works for it.
    let usable = catalogue.usable(&linked);

    let Some(query) = service else {
        let mut names: Vec<_> = usable.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable_by_key(|n| n.to_lowercase());
        if json {
            println!("{}", serde_json::to_string_pretty(&names)?);
        } else {
            println!("{} services can be browsed:", usable.len());
            for name in names {
                println!("  {name}");
            }
            println!("\nOpen one with: x2rock browse -s <service>");
        }
        return Ok(());
    };

    let chosen = catalogue::Catalogue::find(&usable, query)?.clone();
    let token = linked.token_for(&chosen.id);
    // `root` is where every service starts, and no service documents it - it is
    // simply what the players ask for.
    let at = container.unwrap_or("root");
    let mut refreshed = None;
    let (items, total) =
        sonos::smapi::metadata(&chosen, token.as_ref(), at, index, count, &mut refreshed).await?;
    // Feeds whatever comes next, below - not just persisted for later. A
    // token that just proved stale must not be handed straight to `play_item`.
    let token = use_refreshed_token(&chosen.id, token, refreshed);

    if let Some(nth) = play {
        let item = items
            .get(nth.checked_sub(1).unwrap_or(usize::MAX))
            .ok_or_else(|| anyhow!("no row {nth}; {at} has {}", items.len()))?;
        // A container is a place, and refusing here is kinder than letting
        // getMediaURI refuse it with a grammar error about ids.
        //
        // The second sentence is the one that matters: playing a container is
        // not something x2rock has yet to implement, it is not reachable over
        // the LAN at all - four routes were tried and all four fail, see "A
        // service container cannot be played over the LAN at all" in
        // docs/architecture.md. Saving it in the Sonos app is genuinely the way
        // through, because `favorite` hands the player an id and lets it
        // resolve the container itself.
        ensure!(
            !item.container,
            "{:?} is a container. Open it with: x2rock browse -s {} {}\n\
             To play the whole thing, save it as a favorite in the Sonos app - \
             then: x2rock favorite {:?}",
            item.title,
            chosen.name,
            item.id,
            item.title
        );
        return play_item(
            live()?,
            room,
            &chosen,
            token.as_ref(),
            Some(item.item_type.as_str()),
            &item.id,
            &item.title,
        )
        .await;
    }

    if json {
        let rows: Vec<_> = items
            .iter()
            .map(|i| {
                // The field names `favorites`, `search` and `bookmarks` already
                // use, plus the one thing only browsing has: whether a row is a
                // place or a thing.
                json!({
                    "id": i.id,
                    "name": i.title,
                    "type": i.item_type,
                    "description": i.summary,
                    "service": chosen.name,
                    "art_url": i.art_url,
                    "container": i.container,
                    "queueable": queueable(i, &chosen),
                })
            })
            .collect();
        // An envelope, not a bare array. `total` is the whole point: a caller
        // that got `count` rows has no way to tell a full container from a
        // truncated one, and `--json` used to drop the number the plain-text
        // line already prints. `index` echoes what was asked so a pager can
        // step without tracking it. Read `items`; there is more when
        // `index + items.len() < total`.
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": total,
                "index": index,
                "items": rows,
            }))?
        );
        return Ok(());
    }
    if items.is_empty() {
        println!("{} is empty on {}.", at, chosen.name);
        return Ok(());
    }
    for item in &items {
        let summary = item
            .summary
            .as_deref()
            .map(|s| format!("  {s}"))
            .unwrap_or_default();
        // A trailing slash for somewhere to go, the way a directory listing
        // marks one. Cheaper to read than a column.
        let name = if item.container {
            format!("{}/", item.title)
        } else {
            item.title.clone()
        };
        println!("{:<40} {:<10} {name}{summary}", item.id, item.item_type);
    }
    if total > items.len() as u32 {
        println!("\n{} of {total} in {at}.", items.len());
    }
    Ok(())
}

/// `x2rock search`: the CLI talking to a music service. One of three commands
/// that leave the LAN - `browse` and `link` are the others - and like them it is
/// CLI-only and unreachable from the daemon. See "Rule: talking to a service
/// never enters the daemon".
///
/// A player is wanted but not required. Listing what can be searched, and a
/// service's categories, both come from the on-disk catalogue and must keep
/// working when the household is unreachable - a cache that fails whenever the
/// thing it caches is unavailable is not doing its job. Only `--play`, and a
/// first run with nothing cached, genuinely need a player.
#[allow(clippy::too_many_arguments)]
/// How long one service gets to answer in a merged search.
///
/// Shorter than the single-service budget on purpose: thirty-five services are
/// asked at once and the slowest decides when results appear, so a service
/// having a bad day costs everyone. Its absence is reported rather than hidden.
const FAN_OUT_TIMEOUT: Duration = Duration::from_secs(12);

/// Results per service when no `--count` is given and no one service was named.
///
/// Twenty is right for one service and wrong for thirty-five: the merged list is
/// read top to bottom, and seven hundred rows is not a list.
const FAN_OUT_COUNT: u32 = 5;

#[allow(clippy::too_many_arguments)]
pub async fn run_search(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    term: Option<&String>,
    service: Option<&String>,
    category: Option<&String>,
    only_linked: bool,
    count: Option<u32>,
    index: u32,
    play: Option<usize>,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let mut state = State::load()?;
    let reached = session::connect(ip, &mut state, household, room).await;
    let live =
        || -> Result<&session::Session> { reached.as_ref().map_err(hint::no_player_to_play) };

    let mut catalogue = catalogue::Catalogue::load();
    let mut dirty = false;
    match &reached {
        Ok(session) => {
            dirty = catalogue
                .refresh(&Upnp::new(session.connection.ip()), refresh)
                .await?;
        }
        Err(e) if catalogue.services().is_empty() => {
            // Nothing cached and nothing to ask: this is the one case with no
            // useful answer, so give the connection's own error rather than a
            // second-hand one about an empty catalogue.
            return Err(anyhow!("{e:#}"));
        }
        Err(e) => eprintln!("x2rock: no player reached, using the cached catalogue ({e:#})"),
    }

    let linked = credentials::Credentials::load()?;

    // A term with no service is the merged search. Checked before the listing
    // below, which is what a bare term used to fall into: it printed the
    // service list and silently dropped the word the person typed.
    if service.is_none()
        && let Some(term) = term
    {
        let candidates: Vec<sonos::smapi::Service> = catalogue
            .searchable(&linked)
            .into_iter()
            .filter(|s| !only_linked || linked.get(&s.id).is_some())
            .cloned()
            .collect();
        if dirty {
            catalogue.save()?;
        }
        return search_everywhere(
            &mut catalogue,
            &linked,
            &reached,
            room,
            candidates,
            term,
            category,
            count.unwrap_or(FAN_OUT_COUNT),
            index,
            play,
            json,
        )
        .await;
    }
    let count = count.unwrap_or(20);
    let usable = catalogue.searchable(&linked);

    let Some(query) = service else {
        let mut names: Vec<_> = usable.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable_by_key(|n| n.to_lowercase());
        if json {
            println!("{}", serde_json::to_string_pretty(&names)?);
        } else {
            // "More" means it: what could be linked and is not yet, so the
            // count stays honest once some of the linkable set is searchable.
            let linkable = catalogue
                .linkable()
                .iter()
                .filter(|s| linked.get(&s.id).is_none())
                .count();
            println!(
                "{} of {} services can be searched:",
                usable.len(),
                catalogue.services().len()
            );
            for name in names {
                let mark = if linked.services.values().any(|a| a.service_name == name) {
                    "  (linked)"
                } else {
                    ""
                };
                println!("  {name}{mark}");
            }
            println!("\nSearch one with: x2rock search -s <service> <term>");
            if linkable > 0 {
                println!("{linkable} more can be linked: x2rock link");
            }
        }
        if dirty {
            catalogue.save()?;
        }
        return Ok(());
    };

    // Naming a real service that simply needs an account is a different
    // mistake from naming one that does not exist, and the difference is
    // worth the extra lookup. Only for a service that needs one: an anonymous
    // service is always searchable, so if `find` still refused it the refusal
    // is about the query - ambiguity - and saying anything about accounts
    // would answer a question nobody asked.
    let chosen = catalogue::Catalogue::find(&usable, query)
        .map_err(|e| {
            match catalogue
                .services()
                .iter()
                .find(|s| s.name.to_lowercase() == query.to_lowercase())
            {
                Some(s) if s.auth != sonos::smapi::Auth::Anonymous => s.needs_link_hint().into(),
                // A real service that `searchable` has since dropped for
                // publishing no categories. Without this arm `find` calls it
                // unmatched, which reads as a typo rather than as the fact it
                // is - and buys a `x2rock search` retry that will not help.
                Some(s) if catalogue.publishes_no_categories(&s.id) => {
                    s.no_search_categories_hint().into()
                }
                _ => e,
            }
        })?
        .clone();

    // Asked before the call, because a freshly learned *empty* list is the
    // whole reason to write here and `is_empty()` afterwards cannot tell it
    // from a cache hit. Persisting the negative is what stops the next
    // `x2rock search` listing this service as searchable again.
    let learned = !catalogue.categories_cached(&chosen.id);
    let categories = catalogue.categories_for(&chosen).await?;
    dirty |= learned;
    if dirty {
        catalogue.save()?;
    }
    let chosen = &chosen;
    // The cold-cache path to the same refusal the `find` arm above gives: on a
    // first encounter nothing had been asked, so the service was still listed
    // and `find` had no reason to object. Same hint, so the two cannot drift.
    if categories.is_empty() {
        return Err(chosen.no_search_categories_hint().into());
    }
    let picked = match category {
        Some(want) => {
            let want = want.to_lowercase();
            categories
                .iter()
                .find(|c| c.id.to_lowercase() == want)
                .ok_or_else(|| {
                    let known: Vec<_> = categories.iter().map(|c| c.id.as_str()).collect();
                    anyhow!(
                        "{} has no category {want:?}. It has: {}",
                        chosen.name,
                        known.join(", ")
                    )
                })?
        }
        None => categories
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case("all"))
            .unwrap_or(&categories[0]),
    };

    let Some(term) = term else {
        let known: Vec<_> = categories.iter().map(|c| c.id.as_str()).collect();
        println!("{} can search: {}", chosen.name, known.join(", "));
        println!("Default is {}. Give a term to search.", picked.id);
        return Ok(());
    };

    let token = linked.token_for(&chosen.id);
    let mut refreshed = None;
    let (items, total) = sonos::smapi::search(
        chosen,
        token.as_ref(),
        &picked.mapped_id,
        term,
        index,
        count,
        &mut refreshed,
    )
    .await?;
    // Feeds whatever comes next, below - not just persisted for later. A
    // token that just proved stale must not be handed straight to `play_item`.
    let token = use_refreshed_token(&chosen.id, token, refreshed);

    if let Some(nth) = play {
        let item = items
            .get(nth.checked_sub(1).unwrap_or(usize::MAX))
            .ok_or_else(|| anyhow!("no result {nth}; the search returned {}", items.len()))?;
        // A search can return places rather than things: every Mixcloud hit is a
        // `tag:` collection, not a track. Refusing here beats letting
        // getMediaURI refuse it with a grammar error about ids.
        ensure!(
            !item.container,
            "{:?} is a container, not a track. Open it with: x2rock browse -s {} {}\n\
             To play the whole thing, save it as a favorite in the Sonos app - \
             then: x2rock favorite {:?}",
            item.title,
            chosen.name,
            item.id,
            item.title
        );
        return play_item(
            live()?,
            room,
            chosen,
            token.as_ref(),
            Some(item.item_type.as_str()),
            &item.id,
            &item.title,
        )
        .await;
    }
    if json {
        let rows: Vec<_> = items
            .iter()
            .map(|i| {
                // Deliberately the same field names `favorites --json` uses.
                // The bar widget merges the two lists into one picker, and
                // matching shapes keep that a concatenation rather than a
                // translation layer.
                json!({
                    "id": i.id,
                    "name": i.title,
                    "type": i.item_type,
                    "description": i.summary,
                    "service": chosen.name,
                    "art_url": i.art_url,
                    // A hit is not always a thing to play. Mixcloud searches
                    // tags and answers with collections, so a caller that
                    // assumed otherwise would hand a container to `play-item`.
                    "container": i.container,
                    "queueable": queueable(i, chosen),
                })
            })
            .collect();
        // An envelope, not a bare array. `total` is the whole point: a caller
        // that got `count` rows has no way to tell a full container from a
        // truncated one, and `--json` used to drop the number the plain-text
        // line already prints. `index` echoes what was asked so a pager can
        // step without tracking it. Read `items`; there is more when
        // `index + items.len() < total`.
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": total,
                "index": index,
                "items": rows,
            }))?
        );
        return Ok(());
    }
    if items.is_empty() {
        // Name the category that was searched: a service with no `all` was
        // searched in one category only (Plex defaults to artists), and
        // "nothing" without that context reads as "the service has it not"
        // when the truth may be "you searched the wrong shelf".
        let others: Vec<_> = categories
            .iter()
            .map(|c| c.id.as_str())
            .filter(|id| *id != picked.id)
            .collect();
        match others.is_empty() {
            true => println!("Nothing on {} for {term:?}.", chosen.name),
            false => println!(
                "Nothing on {} for {term:?} in {}. Also searchable: {}.",
                chosen.name,
                picked.id,
                others.join(", ")
            ),
        }
        return Ok(());
    }
    for item in &items {
        let summary = item
            .summary
            .as_deref()
            .map(|s| format!("  {s}"))
            .unwrap_or_default();
        // The same trailing slash `browse` uses, and for the same reason: some
        // services answer a search entirely in collections.
        let name = if item.container {
            format!("{}/", item.title)
        } else {
            item.title.clone()
        };
        println!("{:<14} {:<10} {name}{summary}", item.id, item.item_type);
    }
    if total > items.len() as u32 {
        println!("\n{} of {total} on {}.", items.len(), chosen.name);
    }
    Ok(())
}

/// Which category of one service a merged search should ask.
///
/// Named, and the service must have it by that name - a miss is skipped rather
/// than substituted, because `-c albums` answered with a radio service's station
/// list would be a wrong answer wearing the right label. Unnamed, `all` where the
/// service offers one, else whatever it lists first, which is the same rule the
/// single-service path follows.
fn pick_category<'a>(
    categories: &'a [sonos::smapi::Category],
    want: Option<&str>,
) -> Option<&'a sonos::smapi::Category> {
    match want {
        Some(want) => categories.iter().find(|c| c.id.eq_ignore_ascii_case(want)),
        None => categories
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case("all"))
            .or_else(|| categories.first()),
    }
}

/// `x2rock search <term>` with no `--service`: ask everything at once.
///
/// Three passes, because the middle one cannot be folded into the others.
/// Categories are warmed concurrently (`categories_for` takes `&mut self`, so
/// only one of those can be in flight); the plan is then built from the cache;
/// then the searches themselves fan out.
///
/// **Linked services sort first, and that is a judgement about quality rather
/// than speed.** A linked service is the tier with real albums, metadata the
/// service itself vouches for, and content a player will queue. The anonymous
/// tier is stations and aggregators: Hype Machine indexes music blogs, so it
/// carries no albums at all by construction, its titles come from the blog post
/// rather than the file, and its links rot. Worth showing, not worth showing
/// first.
#[allow(clippy::too_many_arguments)]
async fn search_everywhere(
    catalogue: &mut catalogue::Catalogue,
    linked: &credentials::Credentials,
    reached: &Result<session::Session>,
    room: Option<&str>,
    candidates: Vec<sonos::smapi::Service>,
    term: &str,
    category: Option<&String>,
    count: u32,
    index: u32,
    play: Option<usize>,
    json: bool,
) -> Result<()> {
    ensure!(
        !candidates.is_empty(),
        "no service can be searched yet. Link one with: x2rock link"
    );

    // Pass one: learn the categories of anything never asked. A failure is left
    // unrecorded on purpose - `remember_categories` would write "asked, and it
    // has none", which is what drops a service out of `searchable` for good.
    let cold: Vec<&sonos::smapi::Service> = candidates
        .iter()
        .filter(|s| !catalogue.categories_cached(&s.id))
        .collect();
    if !cold.is_empty() {
        let warmed = futures_util::future::join_all(cold.iter().map(|s| async move {
            (
                s.id.clone(),
                tokio::time::timeout(FAN_OUT_TIMEOUT, sonos::smapi::categories(s)).await,
            )
        }))
        .await;
        let mut learned = 0;
        for (id, got) in warmed {
            if let Ok(Ok(categories)) = got {
                catalogue.remember_categories(&id, categories);
                learned += 1;
            }
        }
        if learned > 0 {
            catalogue.save()?;
        }
    }

    // Pass two: who can answer, and in which category. A service that has no
    // category by the requested name is skipped rather than searched in the
    // wrong one - `-c albums` against a radio service would otherwise return
    // its station list and call it albums.
    let mut plan: Vec<(&sonos::smapi::Service, String)> = Vec::new();
    for service in &candidates {
        let Some(categories) = catalogue.cached_categories(&service.id) else {
            continue;
        };
        if let Some(picked) = pick_category(categories, category.map(String::as_str)) {
            plan.push((service, picked.mapped_id.clone()));
        }
    }
    if plan.is_empty() {
        match category {
            Some(want) => bail!("no searchable service has a category {want:?}"),
            None => bail!("no service published a category to search"),
        }
    }

    // Pass three: the searches. Every service gets the same term and the same
    // budget, and one that overruns it is named on stderr rather than passed off
    // as having found nothing.
    let answers = futures_util::future::join_all(plan.iter().map(|(service, mapped)| {
        let token = linked.token_for(&service.id);
        async move {
            let mut refreshed = None;
            let got = tokio::time::timeout(
                FAN_OUT_TIMEOUT,
                sonos::smapi::search(
                    service,
                    token.as_ref(),
                    mapped,
                    term,
                    index,
                    count,
                    &mut refreshed,
                ),
            )
            .await;
            (*service, got, refreshed)
        }
    }))
    .await;

    struct Row<'a> {
        service: &'a sonos::smapi::Service,
        item: sonos::smapi::Item,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut total = 0u32;
    let mut slow: Vec<&str> = Vec::new();
    let mut refused: Vec<(&str, String)> = Vec::new();
    for (service, got, refreshed) in answers {
        // Sequentially, after the fan-out: this writes the credentials file, and
        // several tasks racing to rewrite it is a good way to lose a token.
        if refreshed.is_some() {
            let _ = use_refreshed_token(&service.id, linked.token_for(&service.id), refreshed);
        }
        match got {
            Err(_) => slow.push(&service.name),
            Ok(Err(e)) => refused.push((&service.name, format!("{e:#}"))),
            Ok(Ok((items, found))) => {
                total += found;
                rows.extend(items.into_iter().map(|item| Row { service, item }));
            }
        }
    }

    // Stable, so each service keeps the order it answered in - services rank
    // their own hits and reordering within one would discard that.
    rows.sort_by_key(|r| {
        (
            linked.get(&r.service.id).is_none(),
            r.service.name.to_lowercase(),
        )
    });

    if let Some(nth) = play {
        let row = rows
            .get(nth.checked_sub(1).unwrap_or(usize::MAX))
            .ok_or_else(|| anyhow!("no result {nth}; the search returned {}", rows.len()))?;
        ensure!(
            !row.item.container,
            "{:?} is a container, not a track. Open it with: x2rock browse -s {} {}",
            row.item.title,
            row.service.name,
            row.item.id
        );
        let session = reached.as_ref().map_err(hint::no_player_to_play)?;
        let token = linked.token_for(&row.service.id);
        return play_item(
            session,
            room,
            row.service,
            token.as_ref(),
            Some(row.item.item_type.as_str()),
            &row.item.id,
            &row.item.title,
        )
        .await;
    }

    if json {
        // The same field names one service's `--json` emits, so the widget can
        // concatenate the two rather than translate between them. `service` was
        // always there; here it is the column that matters.
        let items: Vec<_> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.item.id,
                    "name": r.item.title,
                    "type": r.item.item_type,
                    "description": r.item.summary,
                    "service": r.service.name,
                    "art_url": r.item.art_url,
                    "container": r.item.container,
                    "queueable": queueable(&r.item, r.service),
                    "linked": linked.get(&r.service.id).is_some(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": total,
                "index": index,
                "asked": plan.len(),
                "items": items,
            }))?
        );
        return Ok(());
    }

    for name in &slow {
        eprintln!("x2rock: {name} did not answer within {FAN_OUT_TIMEOUT:?}");
    }
    for (name, why) in &refused {
        eprintln!("x2rock: {name} refused the search ({why})");
    }
    if rows.is_empty() {
        println!(
            "Nothing for {term:?} on any of the {} services asked.",
            plan.len()
        );
        return Ok(());
    }
    // Rendered first, then measured. Measuring the raw title instead leaves a
    // container's trailing slash hanging past the column, which is how this was
    // wrong the first time.
    const NAME_MAX: usize = 44;
    let names: Vec<String> = rows
        .iter()
        .map(|r| {
            let name = match r.item.container {
                true => format!("{}/", r.item.title),
                false => r.item.title.clone(),
            };
            match name.chars().count() > NAME_MAX {
                true => name.chars().take(NAME_MAX - 1).chain("…".chars()).collect(),
                false => name,
            }
        })
        .collect();
    let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(20);
    // Ids are never truncated - they are what `play-item` is given, and half an
    // id is worse than a ragged column. The width is the widest *ordinary* one,
    // so a service with monstrous ids (NRK Radio's are 58 characters) pushes its
    // own rows out rather than every other row.
    const ID_MAX: usize = 30;
    let id_width = rows
        .iter()
        .map(|r| r.item.id.chars().count())
        .filter(|n| *n <= ID_MAX)
        .max()
        .unwrap_or(18);
    // The artist, where the service gave one. Four rows reading "Moon River" and
    // nothing else are not four results a person can choose between, and this is
    // the column that tells Frank Ocean from Frank Sinatra. Omitted entirely
    // when nothing has one, rather than printing a column of blanks: a search
    // that answers in stations has no artists and should not imply it does.
    const BY_MAX: usize = 24;
    let artists: Vec<String> = rows
        .iter()
        .map(|r| {
            let by = r.item.summary.clone().unwrap_or_default();
            match by.chars().count() > BY_MAX {
                true => by.chars().take(BY_MAX - 1).chain("…".chars()).collect(),
                false => by,
            }
        })
        .collect();
    let by_width = artists.iter().map(|a| a.chars().count()).max().unwrap_or(0);
    for (n, ((row, name), by)) in rows.iter().zip(&names).zip(&artists).enumerate() {
        // Padded by hand: `{:<width$}` pads to a byte count through Display, so
        // one accented character in a title shifts the column.
        let pad = |s: &str, w: usize| " ".repeat(w.saturating_sub(s.chars().count()));
        let by = match by_width {
            0 => String::new(),
            _ => format!("{by}{}  ", pad(by, by_width)),
        };
        println!(
            "{:>3}. {}{} {:<9} {name}{}  {by}{}",
            n + 1,
            row.item.id,
            pad(&row.item.id, id_width),
            row.item.item_type,
            pad(name, width),
            row.service.name
        );
    }
    println!(
        "\n{} from {} of {} services asked. Play one with: x2rock search {term:?} --play N",
        rows.len(),
        plan.len() - slow.len() - refused.len(),
        plan.len()
    );
    Ok(())
}

/// `x2rock unlink`: forget a linked account, by id, name or unique prefix.
/// Local only - the token stays valid at the service.
pub fn unlink(service: &str) -> Result<()> {
    let mut linked = credentials::Credentials::load()?;
    let (id, _) = linked
        .find_service(service)
        .map(|(id, a)| (id.to_string(), a))?;
    let dropped = linked.forget(&id);
    linked.save()?;
    if let Some(account) = dropped {
        println!(
            "Forgot the {} token. It is still valid at the service - \
             revoke it there if that matters.",
            account.service_name
        );
    }
    Ok(())
}

/// `x2rock accounts`: the accounts this machine holds a token for, and with
/// `content` the account serials the household's favorites and queue name -
/// which is evidence about content, not the household's account list.
pub async fn accounts(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    content: bool,
    json: bool,
) -> Result<()> {
    let linked = credentials::Credentials::load()?;
    // Only `--content` reaches the network, so the default keeps the
    // promise made above: this command reads a file on this machine.
    let serials = if content {
        let mut state = State::load()?;
        let session = session::connect(ip, &mut state, household, room).await?;
        // Favorites are household-wide, so any player answers for the
        // half that matters, and demanding --room to read them would be
        // a question with no bearing on the answer. A room is honoured
        // when given - it picks whose queue is read - and otherwise the
        // first reachable player serves.
        let ip = match room {
            Some(_) => {
                let target = session::target(&session.groups, room)?;
                target
                    .coordinator_ip
                    .ok_or_else(|| anyhow!("no address for {}", target.name))?
            }
            None => session
                .groups
                .players
                .iter()
                .find_map(Player::ip)
                .ok_or_else(|| anyhow!("no player with a known address"))?,
        };
        let upnp = Upnp::new(ip);
        let mut found = std::collections::BTreeSet::new();
        // Favorites are household-wide; the queue is this coordinator's.
        // Neither is the account list - see `serials_in`.
        for object in ["FV:2", "Q:0"] {
            found.extend(upnp::serials_in(&upnp.browse_content(object).await?));
        }
        Some(found)
    } else {
        None
    };
    if json {
        let rows: Vec<_> = linked
            .services
            .iter()
            .map(|(id, a)| {
                // Never the token or the key: this is printed to a
                // terminal, into a widget's stdout, and into whatever
                // logs those end up in.
                json!({
                    "service_id": id,
                    "service": a.service_name,
                    "nickname": a.nickname,
                    "household": a.household,
                    "account_id": a.account_id,
                    "linked": a.linked,
                })
            })
            .collect();
        let out = match &serials {
            Some(found) => json!({
                "linked": rows,
                // Named exactly what it is. A consumer that reads this
                // as the household's accounts will be wrong in both
                // directions - see `upnp::serials_in`.
                "serials_named_by_content": found
                    .iter()
                    .map(|(sid, sn)| json!({ "service_id": sid, "account": sn }))
                    .collect::<Vec<_>>(),
            }),
            None => json!(rows),
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if linked.services.is_empty() {
            println!("No accounts linked. Run `x2rock link` to see what can be.");
        } else {
            for (id, a) in &linked.services {
                // `account_id` is set only when *this machine's* `match`
                // succeeded, which has never happened. Saying "not
                // registered on the household" read as a fact about the
                // household, which this file cannot know: the household
                // may hold several accounts for the service already.
                let registered = match &a.account_id {
                    Some(account) => format!("registered from here as {account}"),
                    None => "no registration from this machine".to_string(),
                };
                println!(
                    "{:<20} {:<10} {:<12} {registered}",
                    a.service_name,
                    id,
                    ago(a.linked)
                );
            }
        }
        if let Some(found) = &serials {
            let catalogue = catalogue::Catalogue::load();
            println!();
            if found.is_empty() {
                println!("No serials named by this household's favorites or queue.");
            } else {
                println!("Serials named by this household's favorites and queue:");
                // By serial, which is the order they were created in.
                // Sorting by service id puts "6" after "333".
                let mut rows: Vec<_> = found.iter().collect();
                rows.sort_by_key(|(_, sn)| sn.parse::<u64>().unwrap_or(u64::MAX));
                for (sid, sn) in rows {
                    let name = catalogue
                        .services()
                        .iter()
                        .find(|s| &s.id == sid)
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| "not in the catalogue".to_string());
                    println!("  sn_{sn:<4} {name:<24} sid {sid}");
                }
            }
            println!();
            println!("Not the household's account list. A serial stays here after its account");
            println!("is removed, and an account that has only played a station never appears.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sonos::smapi::Category;

    fn cats(ids: &[&str]) -> Vec<Category> {
        ids.iter()
            .map(|id| Category {
                id: (*id).to_string(),
                mapped_id: format!("search:{id}"),
            })
            .collect()
    }

    #[test]
    fn an_unnamed_category_prefers_all_and_falls_back_to_the_first() {
        let with_all = cats(&["artists", "all", "tracks"]);
        assert_eq!(pick_category(&with_all, None).unwrap().id, "all");

        // iHeartRadio publishes no `all`, so the merged search asks its first -
        // stations - which is why a music term there answers with radio.
        let without = cats(&["stations", "artists", "tracks"]);
        assert_eq!(pick_category(&without, None).unwrap().id, "stations");

        assert!(pick_category(&[], None).is_none(), "nothing to pick");
    }

    #[test]
    fn a_named_category_is_matched_by_name_or_skipped_entirely() {
        let deezer = cats(&["artists", "albums", "tracks"]);
        assert_eq!(
            pick_category(&deezer, Some("albums")).unwrap().mapped_id,
            "search:albums"
        );
        assert_eq!(
            pick_category(&deezer, Some("ALBUMS")).unwrap().id,
            "albums",
            "services disagree about case"
        );
        // The point of the whole function: a station-only service asked for
        // albums is left out, rather than answered with its stations.
        let radio = cats(&["stations", "podcasts"]);
        assert!(
            pick_category(&radio, Some("albums")).is_none(),
            "no substituting a category the caller did not ask for"
        );
    }
}
