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
use super::nth;
use crate::cli::RateDirection;
use crate::session;
use crate::sonos::local::Connection;
use crate::sonos::proto::Player;
use crate::sonos::upnp::{self, Upnp};
use crate::state::State;
use crate::{bookmarks, catalogue, credentials, hint, sonos};

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
    household: &str,
    service_id: &str,
    account: Option<&str>,
    refreshed: sonos::smapi::RefreshedToken,
) {
    // The account the token came from, not whichever one this service would
    // resolve to now: with two accounts for one service, writing the refresh to
    // the preferred one would overwrite a token that was never stale and leave
    // the stale one in place.
    let existing = match account {
        Some(key) => creds
            .accounts_for(household, service_id)
            .and_then(|held| held.accounts.get(key))
            .cloned(),
        None => creds.get(household, service_id).cloned(),
    };
    let Some(existing) = existing else {
        return;
    };
    let private_key = if refreshed.private_key.is_empty() {
        existing.private_key.clone()
    } else {
        refreshed.private_key
    };
    creds.remember(
        household,
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
/// Writes into the store the caller already holds - every caller loaded one
/// to look the token up in the first place - so the refresh is saved through
/// the same `Credentials` the command is working from, rather than a second
/// copy loaded here that could disagree with it.
fn use_refreshed_token(
    creds: &mut credentials::Credentials,
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
    let (household, account) = token.map_or((None, None), |t| (t.household, t.account));
    // The refresh can only be persisted against the household the token names;
    // a token without one (there should be none from the store) is used for
    // this call and simply not written back.
    if let Some(hh) = &household {
        save_refreshed_token(creds, hh, service_id, account.as_deref(), new_token.clone());
    }
    Some(sonos::smapi::Token {
        token: new_token.auth_token,
        key,
        household,
        account,
    })
}

/// Which household's tokens apply for this command right now.
///
/// The reached player's household when one is in hand; otherwise, for a command
/// running against a cached catalogue with no player (an offline browse), the
/// household named with `--household`, or the store's sole household if it holds
/// exactly one. Empty when it cannot be told, in which case every service resolves
/// as unlinked: anonymous ones still browse, and that is the honest answer when the
/// account cannot be located.
///
/// **Advisory, never fatal.** An unusable `--household` degrades to "not known"
/// rather than failing the command, because the flag is not always a claim about
/// this command: it is documented as ignored on a single-household network, it is
/// env-settable as `X2ROCK_HOUSEHOLD` and so is often simply *set*, and it accepts
/// a **room name**, which no stored household id will ever match. Failing a browse
/// because a room name could not be matched against a store would break the one
/// case this fallback exists to serve. A caller that genuinely cannot proceed
/// without a household resolves it strictly itself - see [`set_preference`].
async fn current_household(
    session: Option<&session::Session>,
    linked: &credentials::Credentials,
    named_household: Option<&str>,
) -> String {
    if let Some(session) = session
        && let Ok(household) = session.connection.household_id().await
    {
        return household;
    }
    if let Some(named) = named_household
        && let Ok(household) = linked.resolve_household(named)
    {
        return household;
    }
    linked.sole_household().unwrap_or_default()
}

/// Collapse `(name, service id)` pairs to one row per service, keeping the
/// first name each service is known by.
///
/// Sorted by **id** first, not name: `dedup_by` only drops *adjacent*
/// duplicates, and two accounts of one service do not always carry the same
/// name - for a service the catalogue cannot name, the import falls back to
/// each account's own nickname. Sorting by name would then leave them apart and
/// the service would be reported twice, which is the bug this whole report was
/// rewritten to avoid.
fn one_row_per_service<'a>(
    pairs: impl Iterator<Item = (&'a String, &'a String)>,
) -> Vec<(&'a String, &'a String)> {
    let mut rows: Vec<(&String, &String)> = pairs.collect();
    rows.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));
    rows.dedup_by(|a, b| a.1 == b.1);
    rows
}

/// How one account is named in the listing: the service, and its nickname too
/// where the service has more than one account and the nickname is what tells
/// them apart.
fn account_label(
    held: &credentials::ServiceAccounts,
    account: &credentials::Account,
    key: &str,
) -> String {
    if held.accounts.len() < 2 {
        return account.service_name.clone();
    }
    // Where there is a choice to make, every row has to be *nameable* - the
    // listing is where someone reads what to hand `--prefer`. `label_for`
    // settles what that takes: the nickname alone where it distinguishes this
    // account, the key alongside or instead where it does not.
    format!("{} ({})", account.service_name, held.distinguisher(key))
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

    let mut linked = credentials::Credentials::load()?;
    let household = player.household_id().await?;
    let token = linked.token_for(&household, &service.id);

    let mut refreshed = None;
    let properties = sonos::smapi::extended_metadata(
        &service,
        token.as_ref(),
        &track_id.object_id,
        &mut refreshed,
    )
    .await?;
    let token = use_refreshed_token(&mut linked, &service.id, token, refreshed);

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
    let _ = use_refreshed_token(&mut linked, &service.id, token, refreshed);

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
#[allow(clippy::too_many_arguments)]
pub async fn run_link(
    ip: Option<IpAddr>,
    household: Option<&str>,
    service: Option<&String>,
    json: bool,
    no_open: bool,
    nickname: Option<&String>,
    no_match: bool,
    from_player: bool,
    from_household: bool,
    callback_port: u16,
    dry_run: bool,
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
    // Which household these tokens belong to - resolved once, up front, because
    // even the "what is linked?" listing is now a per-household question.
    let household = session.connection.household_id().await?;

    if from_household && dry_run {
        // Not a mode of the import: it keeps nothing, so it shares the capture
        // and none of the rest.
        let (short, encoded) = capture_stored(&session, &household, callback_port).await?;
        return report_stored_accounts(&encoded, &short, &catalogue);
    }
    if from_household {
        return link_from_household(
            &session,
            &catalogue,
            &mut linked,
            &household,
            service,
            nickname,
            callback_port,
        )
        .await;
    }

    let Some(query) = service else {
        let linkable = catalogue.linkable();
        if json {
            // What a caller needs to offer linking: the name to pass back, and
            // whether it would be a link or a re-link. The bar widget lists these
            // beside the services it can already reach, which is the only place
            // an unlinked one is visible at all - every other listing filters to
            // what has a token.
            let rows: Vec<_> = linkable
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "name": s.name,
                        "linked": linked.get(&household, &s.id).is_some(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }
        println!("{} services can be linked:", linkable.len());
        for s in &linkable {
            let mark = match linked.get(&household, &s.id) {
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
            let target = session::target_for(&session.groups, group);
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
        let token = wait_for_link(&chosen.name, || sonos::plex::poll(&pin)).await?;
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

        let auth = wait_for_link(&chosen.name, || {
            sonos::smapi::device_auth_token(
                &chosen,
                &household,
                &code.link_code,
                code.link_device_id.as_deref(),
            )
        })
        .await?;
        (auth, Some(code.link_code))
    };

    let nickname = nickname.cloned().unwrap_or_else(default_nickname);
    let hash = auth.user_id_hash_code.clone();
    let id = chosen.id.as_str();
    let account =
        credentials::from_device_auth(&chosen.name, Some(&household), Some(&nickname), auth);
    // Stored before anything else is attempted. A link code is single-use, so
    // losing the token to a later failure would mean walking back through the
    // browser to fix something that already worked.
    let account_key = linked.remember(&household, id, account);
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
            if let Some(entry) = linked
                .households
                .get_mut(&household)
                .and_then(|s| s.get_mut(id))
                .and_then(|held| held.accounts.get_mut(&account_key))
            {
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

/// Capture the household's encrypted account blob, with the short household id
/// the key derives from.
///
/// The blob is keyed to the *short* form (no `.suffix`), which is what
/// `GetHouseholdID` returns, while the SMAPI header and the credentials record
/// want the long one. The short is the long up to its first dot.
async fn capture_stored(
    session: &session::Session,
    long_household: &str,
    callback_port: u16,
) -> Result<(String, String)> {
    let short = long_household
        .split('.')
        .next()
        .unwrap_or(long_household)
        .to_string();
    let encoded = sonos::stored::capture_envelope(
        session.connection.ip(),
        callback_port,
        HOUSEHOLD_EVENT_TIMEOUT,
    )
    .await?;
    Ok((short, encoded))
}

/// `link --from-household --dry-run`: print what the household stores, keep none
/// of it.
///
/// Every record, every attribute, in the payload's own order - including the
/// ones x2rock does not model, since an attribute nobody prints is an attribute
/// nobody discovers. `NumAccounts` is called out per record because the digit
/// on `Token0`/`SerialNum0` is an index against it, and whether a household
/// ever sets it above 1 is still an open question that only a household can
/// answer.
///
/// **A token never reaches the terminal.** Its length does, which is the one
/// property worth comparing between two reads of the same household - a token
/// that changed length changed, and one that kept it may still have been
/// rotated, as this household's YouTube Music was.
fn report_stored_accounts(
    encoded: &str,
    short_household: &str,
    catalogue: &catalogue::Catalogue,
) -> Result<()> {
    let records = sonos::stored::decrypt_elements(encoded, short_household)?;
    println!(
        "{} account record{} in the household's store. Nothing was kept.\n",
        records.len(),
        if records.len() == 1 { "" } else { "s" }
    );
    for (tag, attrs) in &records {
        let named = |key: &str| {
            attrs
                .iter()
                .find(|(n, _)| n == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_default()
        };
        // The service id is the same arithmetic the modelled parser does, and
        // worth printing because the UDN states it only in encoded form.
        let service = named("UDN")
            .strip_prefix("SA_RINCON")
            .and_then(|rest| rest.split('_').next())
            .and_then(|digits| digits.parse::<u32>().ok())
            .map(|encoded_type| {
                let id = (encoded_type / 256).to_string();
                let name = catalogue.name_of(&id).unwrap_or("not in this catalogue");
                format!("{name} (sid {id}, schema rev {})", encoded_type % 256)
            })
            .unwrap_or_else(|| "unrecognised UDN".to_string());
        println!("<{tag}> {service}");
        for (name, value) in attrs {
            // Secrets by name, not by guess: everything a token or key could be
            // called carries the index suffix, so the prefix is the test.
            let shown = if name.starts_with("Token")
                || name.starts_with("Key")
                || name.starts_with("Password")
            {
                if value.is_empty() {
                    "<empty>".to_string()
                } else {
                    format!("<{} bytes, not shown>", value.len())
                }
            } else {
                format!("{value:?}")
            };
            println!("    {name:<14} {shown}");
        }
        println!();
    }
    let packed: Vec<&str> = records
        .iter()
        .filter(|(_, attrs)| {
            attrs
                .iter()
                .any(|(n, v)| n == "NumAccounts" && v.parse::<u32>().unwrap_or(1) > 1)
        })
        .map(|(_, attrs)| {
            attrs
                .iter()
                .find(|(n, _)| n == "UDN")
                .map(|(_, v)| v.as_str())
                .unwrap_or("?")
        })
        .collect();
    if packed.is_empty() {
        println!(
            "Every record declares NumAccounts=1, so each holds one account and the \
             `0` suffix is the only index present."
        );
    } else {
        println!(
            "NumAccounts is above 1 on {}: {}. The suffixed attributes are indices, \
             and these records carry more than the `0` set.",
            if packed.len() == 1 {
                "a record"
            } else {
                "records"
            },
            packed.join(", ")
        );
    }
    Ok(())
}

/// How long to wait for the player's account event. Generous: it is one round
/// trip, but it depends on the player choosing to open a connection back, and a
/// firewall that is going to drop it will drop it for the whole window.
const HOUSEHOLD_EVENT_TIMEOUT: Duration = Duration::from_secs(12);

/// `x2rock link --from-household`: keep the token the household already stores,
/// with no browser flow.
///
/// The counterpart to the whole device-link dance: instead of minting a token,
/// read the one the Sonos app minted and left on the speaker. Named a service
/// takes just that one; unnamed imports every service the household holds a
/// usable token for. No `match` step - this path never carries a
/// `userIdHashCode`, and playback rides the household's own registration, which
/// adding the service in the Sonos app already made.
async fn link_from_household(
    session: &session::Session,
    catalogue: &catalogue::Catalogue,
    linked: &mut credentials::Credentials,
    long_household: &str,
    service: Option<&String>,
    nickname: Option<&String>,
    callback_port: u16,
) -> Result<()> {
    let (short_household, encoded) = capture_stored(session, long_household, callback_port).await?;
    let accounts = sonos::stored::decrypt_accounts(&encoded, &short_household)?;

    // Only accounts that actually carry a token can be injected; the rest are
    // placeholders. Keyed by service id, which is how the catalogue and the
    // credentials store both file them.
    let usable: Vec<_> = accounts.into_iter().filter(|a| a.has_token()).collect();
    ensure!(
        !usable.is_empty(),
        "the household stores no music-service token this machine can read. \
         Add a service in the Sonos app first, then run this again."
    );

    // With a service named, resolve it to an id and keep only that one; the
    // resolution also gives the catalogue's own name for the record.
    let wanted = match service {
        Some(query) => {
            let chosen = catalogue.find_any(query)?;
            let id = chosen.id.clone();
            // Every account the household holds for it, not the first: naming
            // a service narrows *which service* to import, and a service with
            // two accounts has two either way.
            let matched: Vec<_> = usable
                .into_iter()
                .filter(|a| a.service_id.to_string() == id)
                .map(|a| (chosen.name.clone(), id.clone(), a))
                .collect();
            ensure!(
                !matched.is_empty(),
                "the household stores no token for {} - it has not been added \
                 in the Sonos app on this household",
                chosen.name
            );
            matched
        }
        None => usable
            .into_iter()
            .map(|a| {
                let id = a.service_id.to_string();
                // The catalogue names the service where it knows it; a token for
                // a service not in this household's catalogue is still kept, under
                // its own stored nickname or its id.
                let name = catalogue
                    .name_of(&id)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if a.nickname.is_empty() {
                            format!("service {id}")
                        } else {
                            a.nickname.clone()
                        }
                    });
                (name, id, a)
            })
            .collect(),
    };

    for (name, id, account) in &wanted {
        // The stored nickname is the app's own label ("Qb1"); prefer an explicit
        // --nickname, then that, then this machine's default.
        let nick = nickname
            .map(String::to_string)
            .or_else(|| (!account.nickname.is_empty()).then(|| account.nickname.clone()))
            .unwrap_or_else(default_nickname);
        let auth = sonos::smapi::DeviceAuth {
            auth_token: account.token.clone(),
            private_key: account.key.clone(),
            user_id_hash_code: None,
        };
        let mut record =
            credentials::from_device_auth(name, Some(long_household), Some(&nick), auth);
        // The household's own serial for the account, which only this path ever
        // sees. It is what keys the record, so two accounts of one service stay
        // two records instead of the second landing on the first.
        record.serial = Some(account.serial);
        // The per-account `Username` selector, kept as a within-household
        // identity for the one shape the serial cannot cover: a second account
        // served with no distinct serial. `0`/empty name no account, so they
        // are dropped rather than stored as a key that identifies nothing.
        record.account_key = match account.account_key.as_str() {
            "" | "0" => None,
            k => Some(k.to_string()),
        };
        linked.remember(long_household, id, record);
    }
    linked.save()?;

    // Reported per *service*, not per account read, because the two differ:
    // this household holds two iHeartRadio accounts, and a "Kept" line each
    // said twice what the store would then show once. Counting what is held
    // after the writes is the only report that cannot drift from them.
    let services = one_row_per_service(wanted.iter().map(|(n, id, _)| (n, id)));
    for (name, id) in services {
        // A multi-word name has to be quoted or the shell splits it, so the hint
        // is copy-pasteable rather than subtly wrong.
        let quoted = if name.contains(char::is_whitespace) {
            format!("\"{name}\"")
        } else {
            name.clone()
        };
        let Some(held) = linked.accounts_for(long_household, id) else {
            continue;
        };
        match held.accounts.len() {
            0 => continue,
            1 => println!(
                "Kept the household's {name} token. Search it with: x2rock search -s {quoted}"
            ),
            n => {
                let chosen = held
                    .chosen()
                    .map(|(key, _)| held.label_for(key))
                    .unwrap_or_default();
                let all: Vec<String> = held
                    .accounts
                    .keys()
                    .map(|key| held.label_for(key))
                    .collect();
                println!(
                    "Kept {n} {name} accounts ({}); searching with {chosen:?}. \
                     Change that with: x2rock accounts --prefer {quoted} \"<nickname>\"",
                    all.join(", ")
                );
            }
        }
    }
    println!(
        "\nNo household match was needed: playback rides the registration the Sonos app \
         already made. Search and browse work now; on-demand tracks play for any service \
         whose account the household still holds."
    );
    Ok(())
}

/// Ask `poll` every `LINK_POLL` until the person has finished in the browser -
/// the shared wait of the Plex PIN flow and the SMAPI device-link flow, which
/// differ only in what they poll. `Ok(None)` is "not yet" and earns a dot on
/// stderr; the first `Some` is the answer; an error ends the wait, as does the
/// deadline, with a fresh `link` named as the way to start over - a link code
/// is single-use, so nothing here can retry.
async fn wait_for_link<T, F, Fut>(service_name: &str, mut poll: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let deadline = tokio::time::Instant::now() + sonos::smapi::LINK_DEADLINE;
    eprint!("Waiting for you to finish");
    // Kept so the give-up message can say what kept going wrong, rather than
    // reporting a plain timeout over a service that was answering with an
    // error every time.
    let mut last_err: Option<String> = None;
    /// How many polls in a row may fail to reach the service before the link is
    /// abandoned. Five at `LINK_POLL` is about fifteen seconds of patience -
    /// long enough for a wifi hiccup or a resume from suspend, short enough
    /// that a service which is simply down still says so promptly.
    const GIVE_UP_AFTER: u32 = 5;
    let mut consecutive = 0u32;
    loop {
        match poll().await {
            Ok(Some(got)) => {
                eprintln!();
                return Ok(got);
            }
            Ok(None) => {
                use std::io::Write;
                eprint!(".");
                let _ = std::io::stderr().flush();
                consecutive = 0;
            }
            // **A failed poll is evidence about the poll, not about the link.**
            // The same reasoning `stream_url` uses. One six-second socket
            // timeout to `www.saavn.com` used to end the whole flow *after* the
            // person had already finished logging in, throwing away a link code
            // that is single-use - so the browser trip had to be made again for
            // a blip that would have cleared on the next poll two seconds
            // later. Verified against Saavn, 2026-09-21.
            //
            // A refusal is different: the service answered, and said no. That
            // still stops immediately, because asking it again cannot help.
            Err(e) if hint::of(&e).0 == hint::Code::LinkRefused => {
                eprintln!();
                return Err(e);
            }
            // **Bounded, or a dead endpoint becomes a seven-minute wait.**
            // Surviving a blip means tolerating a *few* consecutive failures,
            // not every failure until the deadline: a service that is down, or
            // whose name no longer resolves, used to say so in seconds and must
            // still. The count resets on any answer, so a flaky link that keeps
            // making progress is never cut off.
            Err(e) => {
                use std::io::Write;
                eprint!("?");
                let _ = std::io::stderr().flush();
                consecutive += 1;
                last_err = Some(format!("{e:#}"));
                if consecutive >= GIVE_UP_AFTER {
                    eprintln!();
                    bail!(
                        "{service_name} could not be reached {consecutive} times running, so \
                         the link was abandoned. The last attempt failed with: {}",
                        last_err.unwrap_or_default()
                    );
                }
            }
        }
        if tokio::time::Instant::now() + sonos::smapi::LINK_POLL >= deadline {
            eprintln!();
            match last_err {
                Some(why) => bail!(
                    "{service_name} never confirmed the link, and the last attempt to ask \
                     failed ({why}). Run `x2rock link {service_name}` again to start over."
                ),
                None => bail!(
                    "{service_name} never confirmed the link. Run `x2rock link {service_name}` \
                     again to start over."
                ),
            }
        }
        tokio::time::sleep(sonos::smapi::LINK_POLL).await;
    }
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
        // Re-wrapped so the message is this command's, but with the code kept:
        // a `--json` caller branches on `no_player`, and a plain string would
        // degrade it to `unknown`.
        Err(e) if catalogue.services().is_empty() => {
            return Err(hint::no_player(e, format!("{e:#}")));
        }
        Err(e) => eprintln!("x2rock: no player reached, using the cached catalogue ({e:#})"),
    }

    let mut linked = credentials::Credentials::load()?;
    let household = current_household(reached.as_ref().ok(), &linked, household).await;
    // Everything reachable, which is wider than what `search` offers. Browsing
    // needs an endpoint and, for a linked service, a token; searching needs a
    // published search category on top of that. This comment used to say the
    // two sets were the same - Radio Paloma is the counterexample, browse-only,
    // and filtering here would have removed the one route that works for it.
    let usable = catalogue.usable(&linked, &household);

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
    let token = linked.token_for(&household, &chosen.id);
    // `root` is where every service starts, and no service documents it - it is
    // simply what the players ask for.
    let at = container.unwrap_or("root");
    let mut refreshed = None;
    let (items, total) =
        sonos::smapi::metadata(&chosen, token.as_ref(), at, index, count, &mut refreshed).await?;
    // Feeds whatever comes next, below - not just persisted for later. A
    // token that just proved stale must not be handed straight to `play_item`.
    let token = use_refreshed_token(&mut linked, &chosen.id, token, refreshed);

    if let Some(n) = play {
        let item = nth(&items, n).ok_or_else(|| anyhow!("no row {n}; {at} has {}", items.len()))?;
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

/// How long one service gets to answer in a merged search.
///
/// Shorter than the single-service budget on purpose: thirty-five services are
/// asked at once and the slowest decides when results appear, so a service
/// having a bad day costs everyone. Its absence is reported rather than hidden.
const FAN_OUT_TIMEOUT: Duration = Duration::from_secs(12);

/// Results per service *per category* when no `--count` is given and no one
/// service was named.
///
/// Twenty is right for one service and wrong for thirty-five: the merged list is
/// read top to bottom, and seven hundred rows is not a list.
const FAN_OUT_COUNT: u32 = 5;

/// Rows one service contributes to a merged search when `--per-service` is not
/// given, after its categories are interleaved.
///
/// Three, which is what Sonos's own mobile app shows beneath a service heading
/// before you ask it for more. It is also about as many as a person reads per
/// service when twenty of them answered.
const FAN_OUT_PER_SERVICE: usize = 3;

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
pub async fn run_search(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    term: Option<&String>,
    service: Option<&String>,
    category: Option<&String>,
    all_categories: bool,
    only_linked: bool,
    per_service: Option<usize>,
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
            // second-hand one about an empty catalogue - with its code kept, so
            // `--json` still says `no_player`.
            return Err(hint::no_player(e, format!("{e:#}")));
        }
        Err(e) => eprintln!("x2rock: no player reached, using the cached catalogue ({e:#})"),
    }

    let mut linked = credentials::Credentials::load()?;
    let household = current_household(reached.as_ref().ok(), &linked, household).await;

    // A term with no service is the merged search. Checked before the listing
    // below, which is what a bare term used to fall into: it printed the
    // service list and silently dropped the word the person typed.
    if service.is_none()
        && let Some(term) = term
    {
        let candidates: Vec<sonos::smapi::Service> = catalogue
            .searchable(&linked, &household)
            .into_iter()
            .filter(|s| !only_linked || linked.get(&household, &s.id).is_some())
            .cloned()
            .collect();
        if dirty {
            catalogue.save()?;
        }
        return search_everywhere(
            &mut catalogue,
            &mut linked,
            &household,
            &reached,
            room,
            candidates,
            term,
            category,
            all_categories,
            per_service.unwrap_or(FAN_OUT_PER_SERVICE),
            count.unwrap_or(FAN_OUT_COUNT),
            index,
            play,
            json,
        )
        .await;
    }
    let count = count.unwrap_or(20);
    let usable = catalogue.searchable(&linked, &household);

    let Some(query) = service else {
        let mut sorted = usable.clone();
        sorted.sort_unstable_by_key(|s| s.name.to_lowercase());
        if json {
            let names: Vec<_> = sorted.iter().map(|s| s.name.as_str()).collect();
            println!("{}", serde_json::to_string_pretty(&names)?);
        } else {
            // "More" means it: what could be linked and is not yet, so the
            // count stays honest once some of the linkable set is searchable.
            let linkable = catalogue
                .linkable()
                .iter()
                .filter(|s| linked.get(&household, &s.id).is_none())
                .count();
            println!(
                "{} of {} services can be searched:",
                usable.len(),
                catalogue.services().len()
            );
            for s in &sorted {
                // By id, as every other site asks: a name in Sonos's
                // catalogue can change under a stable id, and the store is
                // keyed by the id for exactly that reason.
                let mark = if linked.get(&household, &s.id).is_some() {
                    "  (linked)"
                } else {
                    ""
                };
                println!("  {}{mark}", s.name);
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
    // The cold-cache path to the same refusal the `find` arm above gives: on a
    // first encounter nothing had been asked, so the service was still listed
    // and `find` had no reason to object. Same hint, so the two cannot drift.
    if categories.is_empty() {
        return Err(chosen.no_search_categories_hint().into());
    }

    // **A named service asked for several categories takes the merged path**,
    // scoped to that one service. Everything that makes several categories
    // readable already lives there - interleaving, the per-service cap, the
    // category on every row - and a second implementation of it here would be a
    // second thing to keep in step. One category keeps the older, plainer
    // output, which is what a script piping a single search still expects.
    // **On the shape of what was asked, not on what it resolved to.** Keying this
    // off the number of categories that matched sent a list which happened to
    // match one - `-c tracks,artists,…` against a stations-only service - down
    // to the single-category lookup below, which compares the whole comma string
    // against a category id and refuses it. That is the exact command the
    // picker's drill-in sends, so "More from ..." failed for every
    // stations-only service, which is most of the anonymous tier.
    //
    // A term is required: with none, the listing of what this service can search
    // is still the right answer and is printed below.
    if let Some(term) = term
        && (all_categories || asked_for_several(category.map(String::as_str)))
    {
        return search_everywhere(
            &mut catalogue,
            &mut linked,
            &household,
            &reached,
            room,
            vec![chosen.clone()],
            term,
            category,
            all_categories,
            // Everything by default: the caller named one service and several
            // categories, which is a request to see them rather than a sample.
            per_service.unwrap_or(0),
            count,
            index,
            play,
            json,
        )
        .await;
    }
    let chosen = &chosen;
    let picked = match category {
        // Through `pick_categories`, so one matcher decides what a name means:
        // hand-rolling it here meant `-c " tracks"` resolved in a merged search
        // and was refused in a single-service one, for a leading space.
        Some(want) => pick_categories(&categories, Some(want), false)
            .first()
            .copied()
            .ok_or_else(|| {
                let known: Vec<_> = categories.iter().map(|c| c.id.as_str()).collect();
                anyhow!(
                    "{} has no category {want:?}. It has: {}",
                    chosen.name,
                    known.join(", ")
                )
            })?,
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

    let token = linked.token_for(&household, &chosen.id);
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
    let token = use_refreshed_token(&mut linked, &chosen.id, token, refreshed);

    if let Some(n) = play {
        let item = nth(&items, n)
            .ok_or_else(|| anyhow!("no result {n}; the search returned {}", items.len()))?;
        // A search can return places rather than things: every Mixcloud hit is a
        // `tag:` collection, not a track. Refusing here beats letting
        // getMediaURI refuse it with a grammar error about ids.
        //
        // An album or a playlist is not one of those places: it holds tracks, so
        // the player expands it into the queue. Only a container of *containers*
        // has nothing to play.
        ensure!(
            !item.container || bookmarks::container_holds_tracks(&item.item_type),
            "{:?} is {} {}, which holds other containers rather than tracks. \
             Open it with: x2rock browse -s {} {}",
            item.title,
            super::article(&item.item_type),
            item.item_type,
            chosen.name,
            item.id
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

/// What a merged search asks for when the caller names no category.
///
/// Sonos standardised these names across services, and they are the three a
/// search box is usually about. Order is the priority: where only a few rows per
/// service survive, a track beats an artist beats an album.
const DEFAULT_CATEGORIES: [&str; 3] = ["tracks", "artists", "albums"];

/// Which categories of one service a merged search should ask, in priority order.
///
/// **Named, and the service must have them by those names.** A miss is skipped
/// rather than substituted, because `-c albums` answered with a radio service's
/// station list would be a wrong answer wearing the right label; a service with
/// none of the named categories drops out of the search entirely.
///
/// Unnamed, `all` first - that is not a convenience but the service declaring
/// Universal Search, which Sonos documents as "use `all` as the ID to inform
/// Sonos that this category should be used for search experiences that support
/// it", and one request that already means "anything". Only three services in
/// this household's catalogue of thirty-five declare one, so the fallback does
/// the real work: [`DEFAULT_CATEGORIES`] where the service has them, and failing
/// even that, whatever it lists first. That last arm is what keeps the thirteen
/// stations-only services answering exactly as they did before.
fn pick_categories<'a>(
    categories: &'a [sonos::smapi::Category],
    want: Option<&str>,
    every: bool,
) -> Vec<&'a sonos::smapi::Category> {
    // The only way to reach a category Sonos never standardised: its name is the
    // service's own, so no list written here could name it.
    if every {
        return categories.iter().collect();
    }
    let by_name = |name: &str| categories.iter().find(|c| c.id.eq_ignore_ascii_case(name));
    if let Some(want) = want {
        // The caller's order, not the service's: they said what mattered most.
        return want
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .filter_map(by_name)
            .collect();
    }
    if let Some(all) = by_name("all") {
        return vec![all];
    }
    let preferred: Vec<_> = DEFAULT_CATEGORIES
        .iter()
        .filter_map(|n| by_name(n))
        .collect();
    if !preferred.is_empty() {
        return preferred;
    }
    categories.iter().take(1).collect()
}

/// Whether `--category` named more than one, which decides how a single service
/// is searched.
///
/// **On the shape of what was asked, not on what it resolved to.** Keying this
/// off the number of categories that matched sent a list which happened to match
/// one - `-c tracks,artists,albums,...` against a stations-only service - down
/// to the single-category lookup, which compares the whole comma string against
/// a category id and refuses it. That is the command the bar widget's drill-in
/// sends, so "More from ..." failed for every stations-only service, which is
/// most of the anonymous tier.
fn asked_for_several(category: Option<&str>) -> bool {
    category.is_some_and(|c| c.contains(','))
}

/// One service and the categories it will be asked in.
///
/// Grouped rather than flattened to one entry per search: the searches are per
/// (service, category) but every consumer is per service, so a flat plan meant
/// re-deriving the grouping by hand at four separate sites.
struct Search<'a> {
    service: &'a sonos::smapi::Service,
    /// `(category id, mapped id)` - the first is what a row is labelled with,
    /// the second is what `search` is actually sent.
    asking: Vec<(String, String)>,
}

/// What one service answered, kept per category until it is interleaved.
///
/// Named because the shape is three deep and reads badly inline: for each
/// category the service was asked, its id and the rows it returned.
type ByCategory<'a> = Vec<(&'a str, Vec<sonos::smapi::Item>)>;

/// One service's categories, round-robined into a single list, best first.
///
/// Concatenating instead would make "top three from this service" three tracks,
/// which is not what a person searching a name wants to see; taking one from
/// each category in turn makes it a track, an artist and an album. The order of
/// `per_category` is the priority - `pick_categories` put it there - so when the
/// cap bites, the earlier categories are what survive.
///
/// **Duplicates are dropped by id, first occurrence winning.** The three services
/// that declare `all` also publish the individual categories, and a service may
/// answer with the same track under both; the first is the more specific one.
///
/// `cap` of 0 keeps everything. Nothing here can fail: a category that returned
/// nothing, a service that returned nothing at all, and fewer rows than the cap
/// are all ordinary.
fn interleave(per_category: ByCategory<'_>, cap: usize) -> Vec<(&str, sonos::smapi::Item)> {
    let mut queues: Vec<(&str, std::vec::IntoIter<sonos::smapi::Item>)> = per_category
        .into_iter()
        .map(|(id, items)| (id, items.into_iter()))
        .collect();
    let mut out: Vec<(&str, sonos::smapi::Item)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    // A spent iterator answers `None` for ever, so an exhausted category needs
    // no removing - polling it again is the whole cost, and it is nothing. The
    // rounds stop when one produces nothing at all.
    let mut progressed = true;
    while progressed {
        progressed = false;
        for (id, items) in queues.iter_mut() {
            let Some(item) = items.next() else { continue };
            progressed = true;
            // An item nothing can be done with is not a result. Observed: a
            // service answering with an element carrying no id at all.
            if item.id.is_empty() || seen.contains(&item.id) {
                continue;
            }
            seen.push(item.id.clone());
            out.push((id, item));
            if cap > 0 && out.len() >= cap {
                return out;
            }
        }
    }
    out
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
    linked: &mut credentials::Credentials,
    household: &str,
    reached: &Result<session::Session>,
    room: Option<&str>,
    candidates: Vec<sonos::smapi::Service>,
    term: &str,
    category: Option<&String>,
    all_categories: bool,
    per_service: usize,
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

    // Pass two: who can answer, and in which categories. A service that has no
    // category by a requested name is skipped rather than searched in the wrong
    // one - `-c albums` against a radio service would otherwise return its
    // station list and call it albums.
    //
    // **Kept grouped by service**, not flattened to one entry per search. The
    // searches are per (service, category) but everything downstream is per
    // service - one row group, one failure line, one token write - and a flat
    // plan meant folding the service back together by hand four separate times,
    // with three different dedup idioms for the one idea.
    let mut plan: Vec<Search<'_>> = Vec::new();
    for service in &candidates {
        let Some(categories) = catalogue.cached_categories(&service.id) else {
            continue;
        };
        let asking: Vec<(String, String)> =
            pick_categories(categories, category.map(String::as_str), all_categories)
                .into_iter()
                .map(|c| (c.id.clone(), c.mapped_id.clone()))
                .collect();
        if !asking.is_empty() {
            plan.push(Search { service, asking });
        }
    }
    if plan.is_empty() {
        match category {
            Some(want) => bail!("no searchable service has a category {want:?}"),
            None => bail!("no service published a category to search"),
        }
    }
    let asked_services = plan.len();
    let searches: usize = plan.iter().map(|s| s.asking.len()).sum();

    // Pass three: the searches. One future per service, fanning out over its own
    // categories inside - so every search still starts at once, and a failure is
    // still per category: a service asked for five whose third times out keeps
    // the other four. What the nesting buys is that an answer arrives already
    // belonging to one service, so nothing downstream has to put it back
    // together.
    let answers = futures_util::future::join_all(plan.iter().map(|search| {
        let token = linked.token_for(household, &search.service.id);
        async move {
            let per_category =
                futures_util::future::join_all(search.asking.iter().map(|(id, mapped)| {
                    let token = token.clone();
                    async move {
                        let mut refreshed = None;
                        let got = tokio::time::timeout(
                            FAN_OUT_TIMEOUT,
                            sonos::smapi::search(
                                search.service,
                                token.as_ref(),
                                mapped,
                                term,
                                index,
                                count,
                                &mut refreshed,
                            ),
                        )
                        .await;
                        (id.as_str(), got, refreshed)
                    }
                }))
                .await;
            (search.service, per_category)
        }
    }))
    .await;

    struct Row<'a> {
        service: &'a sonos::smapi::Service,
        category: &'a str,
        item: sonos::smapi::Item,
    }
    let mut grouped: Vec<(&sonos::smapi::Service, ByCategory)> = Vec::new();
    let mut total = 0u32;
    let mut slow: Vec<&str> = Vec::new();
    let mut refused: Vec<(&str, String)> = Vec::new();
    for (service, per_category) in answers {
        let mut answered: ByCategory = Vec::new();
        let mut failed: Option<String> = None;
        let mut timed_out = false;
        let mut refresh = None;
        for (id, got, refreshed) in per_category {
            // The first is kept and the rest dropped: several categories of one
            // service can each come back with the same refreshed token, and
            // writing it repeatedly says the same thing while risking saying it
            // badly. The write itself happens once, below.
            refresh = refresh.or(refreshed);
            match got {
                Err(_) => timed_out = true,
                Ok(Err(e)) => {
                    failed.get_or_insert_with(|| format!("{e:#}"));
                }
                Ok(Ok((items, found))) => {
                    // Summed only over the categories that actually replied, so
                    // the number never quietly omits one that did not.
                    total += found;
                    answered.push((id, items));
                }
            }
        }
        // Sequentially, after the fan-out: this writes the credentials file, and
        // several tasks racing to rewrite it is a good way to lose a token.
        if refresh.is_some() {
            let token = linked.token_for(household, &service.id);
            let _ = use_refreshed_token(linked, &service.id, token, refresh);
        }
        // Named once however many of its categories failed. Five timeout lines
        // for one service hide the other thirty.
        if answered.is_empty() {
            match failed {
                Some(why) => refused.push((&service.name, why)),
                None if timed_out => slow.push(&service.name),
                None => {}
            }
        } else {
            grouped.push((service, answered));
        }
    }
    let answered_services = grouped.len();

    let mut rows: Vec<Row> = Vec::new();
    for (service, per_category) in grouped {
        for (category, item) in interleave(per_category, per_service) {
            rows.push(Row {
                service,
                category,
                item,
            });
        }
    }

    // Stable, so each service keeps the order the interleave gave it - services
    // rank their own hits and reordering within one would discard that.
    rows.sort_by_key(|r| {
        (
            linked.get(household, &r.service.id).is_none(),
            r.service.name.to_lowercase(),
        )
    });

    if let Some(n) = play {
        let row = nth(&rows, n)
            .ok_or_else(|| anyhow!("no result {n}; the search returned {}", rows.len()))?;
        ensure!(
            !row.item.container || bookmarks::container_holds_tracks(&row.item.item_type),
            "{:?} is {} {}, which holds other containers rather than tracks. \
             Open it with: x2rock browse -s {} {}",
            row.item.title,
            super::article(&row.item.item_type),
            row.item.item_type,
            row.service.name,
            row.item.id
        );
        let session = reached.as_ref().map_err(hint::no_player_to_play)?;
        let token = linked.token_for(household, &row.service.id);
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

    // Which services did not answer, and which refused, whichever way the
    // answer is printed: stderr cannot pollute the JSON on stdout, and a caller
    // grouping thirty-five services deserves to know which three are missing.
    for name in &slow {
        eprintln!("x2rock: {name} did not answer within {FAN_OUT_TIMEOUT:?}");
    }
    for (name, why) in &refused {
        eprintln!("x2rock: {name} refused the search ({why})");
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
                    "linked": linked.get(household, &r.service.id).is_some(),
                    // Unconditional, even when only one category was asked: a
                    // caller grouping by it should not have to work out whether
                    // the field exists before it can read it.
                    "category": r.category,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "total": total,
                "index": index,
                // Services, not searches: there are twenty-three of the first
                // and thirty-two of the second for an ordinary merged term.
                "asked": asked_services,
                "searches": searches,
                "answered": answered_services,
                "slow": slow,
                "refused": refused.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                "items": items,
            }))?
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!("Nothing for {term:?} on any of the {asked_services} services asked.");
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
    // The category each row came from, shown only when more than one is in play.
    // With one it is the same word on every line, which is a column of noise.
    // Measured off `rows` directly - unlike `names` and `artists`, which hold
    // rendered strings and so earn their vectors, this is a plain borrow.
    let cat_width = match rows.iter().all(|r| r.category == rows[0].category) {
        true => 0,
        false => rows
            .iter()
            .map(|r| r.category.chars().count())
            .max()
            .unwrap_or(0),
    };
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
    // What the service says about the row, which for a catalogue is the artist:
    // four hits reading "Moon River" and nothing else are not four results a
    // person can choose between, and this is the column that tells Frank Ocean
    // from Frank Sinatra.
    //
    // **It is not always an artist**, and calling it one would be a promise the
    // data does not keep. `parse_items` fills `summary` from whichever of
    // `summary`, `artist`, `genre` or `country` a service populates, so a merged
    // search puts genres and programme blurbs in the same column - 55 of 95 rows
    // for "jazz", including NRK Radio's Norwegian synopses. Restricting it by
    // item type would not sort that out either, since those arrive as `track`.
    // It still earns its place: whatever a service offers here distinguishes one
    // row from the next, which is the whole job of the column. Omitted entirely
    // when no row has one, rather than printing a column of blanks.
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
        // Width 0 means the column was suppressed, so it renders as nothing.
        let column = |s: &str, w: usize| match w {
            0 => String::new(),
            _ => format!("{s}{}  ", pad(s, w)),
        };
        let by = column(by, by_width);
        let cat = column(row.category, cat_width);
        println!(
            "{:>3}. {}{} {:<9} {name}{}  {by}{cat}{}",
            n + 1,
            row.item.id,
            pad(&row.item.id, id_width),
            row.item.item_type,
            pad(name, width),
            row.service.name
        );
    }
    println!(
        "\n{} from {answered_services} of {asked_services} services asked. \
         Play one with: x2rock search {term:?} --play N",
        rows.len()
    );
    Ok(())
}

/// `x2rock unlink`: forget stored tokens. Local only - they stay valid at the
/// service. Scoped by what is given: a service alone forgets that service in
/// every household; `--household` narrows any variant to one; `--all` clears
/// whole households.
///
/// `household` is the global `--household` selector. Here it has no player to
/// resolve against, so it is matched against the *stored* household ids (what
/// `accounts` shows) by exact id or a unique fragment - not against room names.
pub fn unlink(
    service: Option<&str>,
    all: bool,
    account: Option<&str>,
    household: Option<&str>,
) -> Result<()> {
    let mut linked = credentials::Credentials::load()?;
    let scope = household.map(|h| linked.resolve_household(h)).transpose()?;

    if all {
        let (count, where_) = match &scope {
            Some(hh) => (linked.forget_household(hh), format!(" in household {hh}")),
            None => {
                let n = linked.all().count();
                linked.households.clear();
                (n, String::new())
            }
        };
        linked.save()?;
        if count == 0 {
            println!("Nothing was linked{where_}.");
        } else {
            let what = if count == 1 {
                "the one stored token".to_string()
            } else {
                format!("all {count} tokens")
            };
            println!(
                "Forgot {what}{where_}. They stay valid at their services - \
                 revoke them there if that matters. Re-import with `x2rock link --from-household`."
            );
        }
        return Ok(());
    }

    let Some(service) = service else {
        bail!("name a service to unlink, or pass --all to forget every token.");
    };
    let (id, name) = linked.find_service_id(service)?;

    // One account of the service rather than all of them. Resolved per
    // household, because the same nickname can name a different account in each
    // and the key certainly does - so this walks the households in scope and
    // forgets what each one resolves the query to, rather than resolving once
    // and assuming the answer travels.
    if let Some(query) = account {
        let households: Vec<String> = match &scope {
            Some(hh) => vec![hh.clone()],
            None => linked
                .households
                .iter()
                .filter(|(_, s)| s.contains_key(&id))
                .map(|(hh, _)| hh.clone())
                .collect(),
        };
        let mut dropped = Vec::new();
        for hh in &households {
            // A household that holds this service but no account by that name
            // is passed over, not fatal: with several households in scope the
            // nickname naturally names an account in only some of them, and
            // failing here would abandon the removals already made in the
            // others - silently, since nothing had been saved yet. An
            // *ambiguous* name still stops everything, because passing over
            // that would forget an account nobody named.
            let Some(key) = linked.try_resolve_account(hh, &id, query)? else {
                continue;
            };
            if let Some(gone) = linked.forget_account(hh, &id, &key) {
                dropped.push(credentials::account_display(gone.nickname.as_deref(), &key));
            }
        }
        linked.save()?;
        // Named the same way the whole-service path names it, which it did not
        // used to be: this branch returned before `where_` was ever built, so
        // `unlink X --account Y --household Z` reported as though it had swept
        // everywhere.
        let where_ = match &scope {
            Some(hh) => format!(" in household {hh}"),
            None if dropped.len() > 1 => format!(" (in {} households)", dropped.len()),
            None => String::new(),
        };
        // One name per account *name*, not per household: the same nickname in
        // two households is one account as far as a person reading this is
        // concerned, and "Kids, Kids" reads like a bug.
        dropped.sort();
        dropped.dedup();
        match dropped.len() {
            0 => println!("No {name} account matching {query:?} was held{where_}."),
            _ => println!(
                "Forgot the {name} account {}{where_}. It is still valid at the service - \
                 revoke it there if that matters.",
                dropped.join(", ")
            ),
        }
        return Ok(());
    }

    // With a household named, forget only there; otherwise from every household
    // that holds it - for a roaming machine, "stop using this service" rather
    // than "on this one network".
    let (dropped, where_) = match &scope {
        Some(hh) => (linked.forget(hh, &id), format!(" in household {hh}")),
        None => {
            // Counted before the removal, and counted in *accounts*: the arm
            // above drops one household's accounts, so if this one reported
            // households the same number would mean two different things.
            let households: Vec<String> = linked
                .households
                .iter()
                .filter(|(_, s)| s.contains_key(&id))
                .map(|(hh, _)| hh.clone())
                .collect();
            let accounts = households
                .iter()
                .filter_map(|hh| linked.accounts_for(hh, &id))
                .map(|held| held.accounts.len())
                .sum();
            linked.forget_everywhere(&id);
            let w = if households.len() > 1 {
                format!(" (in {} households)", households.len())
            } else {
                String::new()
            };
            (accounts, w)
        }
    };
    linked.save()?;
    if dropped > 0 {
        // Plural where it is: forgetting a service that held two accounts is
        // worth saying out loud, since the other one was not named and went too.
        let what = if dropped > 1 {
            format!("all {dropped} {name} tokens")
        } else {
            format!("the {name} token")
        };
        println!(
            "Forgot {what}{where_}. They stay valid at the service - \
             revoke them there if that matters."
        );
    } else {
        println!("No {name} token was held{where_}.");
    }
    Ok(())
}

/// `x2rock accounts --prefer <service> <account>`: choose which of a household's
/// accounts for a service everything else uses.
///
/// The household is resolved the way `search` and `browse` resolve it - the
/// reached player's, else the store's sole household - because a preference is
/// per household by construction: the office's two Audible accounts and the
/// home system's are different accounts, and a preference stated on one network
/// has no meaning on the other.
///
/// Reaching a player is tried and not required: with one household in the store
/// there is nothing to disambiguate, and a person sitting away from their
/// speakers can still say which account to use.
async fn set_preference(
    ip: Option<IpAddr>,
    household: Option<&str>,
    room: Option<&str>,
    linked: &mut credentials::Credentials,
    pair: &[String],
) -> Result<()> {
    let [service, account] = pair else {
        bail!("--prefer takes a service and an account: --prefer <service> <account>");
    };
    let mut state = State::load()?;
    let session = session::connect(ip, &mut state, household, room).await.ok();
    let mut hh = current_household(session.as_ref(), linked, household).await;
    // Strict where the advisory resolution above is not: this command writes to
    // one household's slot and has nothing sensible to do without knowing which,
    // so a `--household` that matched nothing is named as such rather than
    // falling through to "cannot tell which household", which would be the one
    // message guaranteed to be unhelpful to someone who just named it.
    if hh.is_empty()
        && let Some(named) = household
    {
        hh = linked.resolve_household(named)?;
    }
    ensure!(
        !hh.is_empty(),
        "cannot tell which household this is for. Connect to the household's \
         network, or name it with --household."
    );

    let (id, name) = linked.find_service_id(service)?;
    let key = linked.resolve_account(&hh, &id, account)?;
    let held = linked
        .accounts_for(&hh, &id)
        .ok_or_else(|| anyhow!("no {name} account is held in this household"))?;
    // Saying what it already was is not a failure, but it is worth not
    // claiming a change that did not happen.
    let already = held.is_chosen(&key) && held.preferred.is_some();
    let named = credentials::account_display(
        held.accounts.get(&key).and_then(|a| a.nickname.as_deref()),
        &key,
    );
    let others = held.accounts.len().saturating_sub(1);
    linked.prefer(&hh, &id, &key)?;
    linked.save()?;
    if already {
        println!("{name} already searches with {named:?}.");
    } else if others == 0 {
        // The only account there is. Stated rather than refused: it is a
        // perfectly sensible thing to have typed, and it will still be the
        // preference when a second account arrives.
        println!("{name} will search with {named:?}, the only account held for it here.");
    } else {
        println!("{name} now searches with {named:?}, not the other {others}.");
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
    prefer: Option<&[String]>,
    json: bool,
) -> Result<()> {
    let mut linked = credentials::Credentials::load()?;

    // Setting the preference is a different command wearing this one's name -
    // it writes, and it answers rather than lists - so it returns before any
    // of the listing below.
    if let Some(pair) = prefer {
        return set_preference(ip, household, room, &mut linked, pair).await;
    }
    let linked = linked;
    // Which household to list, when one was named. Matched against the *stored*
    // ids, as `unlink` matches it, since this command reaches no player of its
    // own. With `--content` it does reach one, and a room name - a documented
    // form of this flag - only resolves there, so the failure is deferred and
    // the session's household stands in below.
    let mut scope = match household {
        None => None,
        Some(named) => match linked.resolve_household(named) {
            Ok(hh) => Some(hh),
            Err(e) if !content => return Err(e),
            Err(_) => None,
        },
    };

    // Only `--content` reaches the network, so the default keeps the
    // promise made above: this command reads a file on this machine.
    let serials = if content {
        let mut state = State::load()?;
        let session = session::connect(ip, &mut state, household, room).await?;
        // A `--household` that named a room rather than a stored id resolves
        // here, where a player can say which household that room is in.
        if scope.is_none()
            && household.is_some()
            && let Ok(reached) = session.connection.household_id().await
        {
            scope = Some(reached);
        }
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
        // Still a bare array, and still one row per *account*: a service with
        // two accounts is two rows. The bar widget reduces this to service
        // names and skips one it has already seen, so the extra row costs it
        // nothing - but it does require the top level to stay an array, which
        // is why `--content` wraps and the default never does.
        let rows: Vec<_> = linked
            .all()
            .filter(|(hh, _, _, _)| scope.as_deref().is_none_or(|target| *hh == target))
            .map(|(hh, id, key, a)| {
                // Never the token or the key: this is printed to a
                // terminal, into a widget's stdout, and into whatever
                // logs those end up in.
                json!({
                    "service_id": id,
                    "service": a.service_name,
                    "nickname": a.nickname,
                    "account_key": key,
                    "serial": a.serial,
                    "preferred": linked
                        .accounts_for(hh, id)
                        .is_some_and(|held| held.is_chosen(key)),
                    "household": hh,
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
        if linked.is_empty() {
            println!("No accounts linked. Run `x2rock link` to see what can be.");
            return Ok(());
        }
        let households: Vec<_> = match &scope {
            Some(target) => linked
                .households
                .get_key_value(target)
                .into_iter()
                .collect(),
            None => linked.households.iter().collect(),
        };
        // Scoped to a household this store holds nothing for. Reachable through
        // `--content`, where the scope comes from the player that answered
        // rather than from the store - standing in front of speakers whose
        // household has no token here is exactly the case - and printing
        // nothing at all would read as "the command did not run".
        if households.is_empty() {
            let where_ = scope.as_deref().unwrap_or_default();
            println!(
                "No accounts linked in household {where_}. \
                 Run `x2rock link --from-household` to import what it holds."
            );
            return Ok(());
        }
        // Grouped by household, with a header only when there is more than
        // one - the roaming case - so the ordinary single-household listing
        // reads exactly as it did.
        let multi = scope.is_none() && linked.households.len() > 1;
        for (hh, services) in households {
            if multi {
                println!("Household {hh}:");
            }
            // The marker column exists for this household only when some
            // service in it has a choice to make, and the name column is
            // sized to what it actually has to hold - a nickname pushes a
            // row well past the width a bare service name needs, and a
            // fixed width either truncates it or pads every other line to
            // suit the longest thing that might one day appear.
            let any_choice = services.values().any(|h| h.accounts.len() > 1);
            let width = services
                .values()
                .flat_map(|held| {
                    held.accounts
                        .iter()
                        .map(move |(key, a)| account_label(held, a, key).chars().count())
                })
                .max()
                .unwrap_or(20)
                .max(20);
            for (id, held) in services {
                let several = held.accounts.len() > 1;
                for (key, a) in &held.accounts {
                    // `account_id` is set only when *this machine's* `match`
                    // succeeded, which has never happened. Saying "not
                    // registered on the household" read as a fact about the
                    // household, which this file cannot know: the household
                    // may hold several accounts for the service already.
                    let registered = match &a.account_id {
                        Some(account) => format!("registered from here as {account}"),
                        None => "no registration from this machine".to_string(),
                    };
                    // The marker leads the line rather than trailing the
                    // name, so it lines up whatever the names are doing,
                    // and it is only drawn where there is a choice to
                    // make: a lone account is not "preferred over"
                    // anything, and a column of stars down a
                    // single-account listing would say nothing while
                    // looking like it did.
                    let mark = match (any_choice, several && held.is_chosen(key)) {
                        (false, _) => "",
                        (true, true) => "* ",
                        (true, false) => "  ",
                    };
                    let named = account_label(held, a, key);
                    println!(
                        "{mark}{named:<width$} {:<10} {:<12} {registered}",
                        id,
                        ago(a.linked)
                    );
                }
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

    #[test]
    fn two_accounts_of_one_service_are_reported_once_even_when_named_apart() {
        // The import names each account after the catalogue's service name, but
        // falls back to the account's own nickname for a service the catalogue
        // does not carry - so one service id can arrive under two names. Sorting
        // by name would leave them non-adjacent and `dedup_by` would keep both.
        let (a, b, c) = (
            "Account A".to_string(),
            "Account B".to_string(),
            "Deezer".to_string(),
        );
        let (six, two) = ("6".to_string(), "2".to_string());
        let rows = one_row_per_service([(&a, &six), (&c, &two), (&b, &six)].into_iter());
        assert_eq!(rows.len(), 2, "one row per service id, not per account");
        // Grouped by id, and the first name by sort order stands for the pair.
        assert_eq!(rows[0], (&c, &two));
        assert_eq!(rows[1], (&a, &six));
    }

    #[tokio::test]
    async fn current_household_resolves_named_household_offline() {
        let mut creds = credentials::Credentials::default();
        let auth = sonos::smapi::DeviceAuth {
            auth_token: "tok".into(),
            private_key: "key".into(),
            user_id_hash_code: None,
        };
        creds.remember(
            "Sonos_home.123",
            "2",
            credentials::from_device_auth("Deezer", Some("Sonos_home.123"), None, auth.clone()),
        );
        creds.remember(
            "Sonos_office.456",
            "2",
            credentials::from_device_auth("Deezer", Some("Sonos_office.456"), None, auth),
        );

        // When offline with multiple households and no household named, resolves to empty:
        assert_eq!(current_household(None, &creds, None).await, "");

        // When offline with a household named, resolves it against the store:
        assert_eq!(
            current_household(None, &creds, Some("office")).await,
            "Sonos_office.456"
        );
        assert_eq!(
            current_household(None, &creds, Some("home")).await,
            "Sonos_home.123"
        );

        // A name that matches no stored household degrades to "not known"
        // rather than failing. `--household` takes a **room name** as well as
        // an id, and a room name can only ever be resolved by a player - so
        // refusing here would break `browse`/`search` offline for the
        // documented spelling of the flag, and for anyone who simply has
        // X2ROCK_HOUSEHOLD exported. Commands that cannot proceed without a
        // household resolve it strictly themselves.
        assert_eq!(current_household(None, &creds, Some("Kitchen")).await, "");
        assert_eq!(current_household(None, &creds, Some("beach")).await, "");
    }

    fn cats(ids: &[&str]) -> Vec<Category> {
        ids.iter()
            .map(|id| Category {
                id: (*id).to_string(),
                mapped_id: format!("search:{id}"),
            })
            .collect()
    }

    fn ids<'a>(picked: &[&'a sonos::smapi::Category]) -> Vec<&'a str> {
        picked.iter().map(|c| c.id.as_str()).collect()
    }

    fn item(id: &str) -> sonos::smapi::Item {
        sonos::smapi::Item {
            id: id.to_string(),
            title: id.to_string(),
            item_type: "track".into(),
            summary: None,
            art_url: None,
            container: false,
        }
    }

    fn picked(out: &[(&str, sonos::smapi::Item)]) -> Vec<String> {
        out.iter().map(|(c, i)| format!("{c}:{}", i.id)).collect()
    }

    #[test]
    fn asking_for_every_category_reaches_the_ones_with_no_canonical_name() {
        // Hype Machine's shape: two standard shelves and one of its own. No
        // list written here could name "Blogs", so this is the only way to it.
        let mut hype = cats(&["artists", "tracks"]);
        hype.push(Category {
            id: "Blogs".into(),
            mapped_id: "SBLG".into(),
        });
        assert_eq!(
            ids(&pick_categories(&hype, None, true)),
            ["artists", "tracks", "Blogs"]
        );
        // And it outranks a named list, which could only ever name the standard
        // ones by accident.
        assert_eq!(
            ids(&pick_categories(&hype, Some("tracks"), true)),
            ["artists", "tracks", "Blogs"]
        );
        // Off, the defaults still apply and the custom shelf is not asked for.
        assert_eq!(
            ids(&pick_categories(&hype, None, false)),
            ["tracks", "artists"]
        );
    }

    #[test]
    fn a_list_that_matches_one_category_is_still_a_list() {
        // The regression: a stations-only service asked for every standard
        // category matches exactly one, and routing on that count sent the whole
        // comma string to a lookup that compares it against a category id.
        assert!(asked_for_several(Some("tracks,artists,albums,stations")));
        assert!(!asked_for_several(Some("tracks")));
        assert!(!asked_for_several(None));
        // And the resolution of that same list is what the merged path then
        // searches - one category, not none.
        let radio = cats(&["stations"]);
        assert_eq!(
            ids(&pick_categories(
                &radio,
                Some("tracks,artists,albums,stations"),
                false
            )),
            ["stations"]
        );
    }

    #[test]
    fn interleaving_takes_one_from_each_category_in_turn() {
        // Three rows from a service should be three kinds of thing, not the
        // first three tracks - which is the whole reason for round-robin.
        let out = interleave(
            vec![
                ("tracks", vec![item("t1"), item("t2"), item("t3")]),
                ("artists", vec![item("a1"), item("a2")]),
                ("albums", vec![item("b1")]),
            ],
            3,
        );
        assert_eq!(picked(&out), ["tracks:t1", "artists:a1", "albums:b1"]);
    }

    #[test]
    fn an_exhausted_category_drops_out_and_the_rest_keep_going() {
        // Albums runs dry first; the remaining rows must still come, rather than
        // the round-robin stalling or leaving a gap.
        let out = interleave(
            vec![
                ("tracks", vec![item("t1"), item("t2")]),
                ("albums", vec![item("b1")]),
            ],
            0,
        );
        assert_eq!(picked(&out), ["tracks:t1", "albums:b1", "tracks:t2"]);
    }

    #[test]
    fn the_same_id_in_two_categories_is_kept_once() {
        // The three services that declare `all` also publish the individual
        // categories, so one track can arrive twice. First wins, which given the
        // priority order is the more specific category.
        let out = interleave(
            vec![
                ("tracks", vec![item("same"), item("t2")]),
                ("all", vec![item("same"), item("x9")]),
            ],
            0,
        );
        assert_eq!(picked(&out), ["tracks:same", "tracks:t2", "all:x9"]);
    }

    #[test]
    fn nothing_to_interleave_is_ordinary_rather_than_an_error() {
        assert!(
            interleave(vec![], 3).is_empty(),
            "a service that said nothing"
        );
        assert!(
            interleave(vec![("tracks", vec![])], 3).is_empty(),
            "a category that said nothing"
        );
        // Fewer rows than the cap is the common case and must not pad or panic.
        let out = interleave(vec![("tracks", vec![item("t1")])], 10);
        assert_eq!(picked(&out), ["tracks:t1"]);
    }

    #[test]
    fn an_item_with_no_id_is_dropped_because_nothing_can_be_done_with_it() {
        let out = interleave(vec![("tracks", vec![item(""), item("t1")])], 0);
        assert_eq!(picked(&out), ["tracks:t1"]);
    }

    #[test]
    fn a_cap_of_zero_keeps_everything() {
        let out = interleave(
            vec![("tracks", vec![item("t1"), item("t2"), item("t3")])],
            0,
        );
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn an_unnamed_search_prefers_all_which_is_the_universal_search_declaration() {
        // One request that already means "anything", so nothing else is asked.
        let with_all = cats(&["artists", "all", "tracks"]);
        assert_eq!(ids(&pick_categories(&with_all, None, false)), ["all"]);
        assert!(
            pick_categories(&[], None, false).is_empty(),
            "nothing to pick"
        );
    }

    #[test]
    fn an_unnamed_search_without_all_takes_the_default_three_in_priority_order() {
        // Deezer's real list. The service's own order is artists-first; ours is
        // tracks-first, because that is what survives a small per-service cap.
        let deezer = cats(&["artists", "albums", "tracks", "playlists", "stations"]);
        assert_eq!(
            ids(&pick_categories(&deezer, None, false)),
            ["tracks", "artists", "albums"]
        );
        // Hype Machine has no albums, and is asked only for what it has.
        let partial = cats(&["artists", "tracks"]);
        assert_eq!(
            ids(&pick_categories(&partial, None, false)),
            ["tracks", "artists"]
        );
    }

    #[test]
    fn a_service_with_none_of_the_defaults_still_answers_on_its_first() {
        // Thirteen services here are stations-only. Without this arm every one
        // of them would drop out of a merged search that used to include them.
        let radio = cats(&["stations", "podcasts"]);
        assert_eq!(ids(&pick_categories(&radio, None, false)), ["stations"]);
    }

    #[test]
    fn a_named_list_keeps_the_callers_order_and_skips_what_is_missing() {
        let deezer = cats(&["artists", "albums", "tracks"]);
        assert_eq!(
            ids(&pick_categories(&deezer, Some("albums,tracks"), false)),
            ["albums", "tracks"],
            "the caller said what mattered most, not the service"
        );
        assert_eq!(
            ids(&pick_categories(&deezer, Some(" ALBUMS , ,tracks "), false)),
            ["albums", "tracks"],
            "services disagree about case, and people leave spaces"
        );
        assert_eq!(
            ids(&pick_categories(&deezer, Some("albums,podcasts"), false)),
            ["albums"],
            "a name this service lacks is dropped, not substituted"
        );
        // The point of the whole function: a station-only service asked for
        // albums is left out, rather than answered with its stations.
        let radio = cats(&["stations", "podcasts"]);
        assert!(
            pick_categories(&radio, Some("albums"), false).is_empty(),
            "no substituting a category the caller did not ask for"
        );
    }
}
