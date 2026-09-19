//! `raw`: one message straight to a player and its answer printed, over
//! either wire - a Control API command by namespace and name, or a UPnP SOAP
//! action by service and name. A probe, not a feature: a refusal is a result
//! here and exits 0, because discovering that something is unsupported is
//! what the probe was for.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;

use super::speaker::named_speaker;
use crate::cli::{RawScope, UpnpScope};
use crate::session::{self, Session};
use crate::sonos;
use crate::sonos::local::Connection;
use crate::sonos::upnp::{self, Upnp};

/// `raw upnp`: one SOAP action against one speaker.
///
/// Separate from the Control API path rather than folded into it because
/// almost nothing is shared: a different transport, a different address (a
/// player, never a group id), a different argument shape, and a different
/// answer. What they do share is the contract that **a refusal is a result** -
/// a UPnP fault prints and exits 0, so a probe that discovers an action is
/// unsupported has succeeded at what it was for.
pub async fn raw_upnp(
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

/// `raw api`: send `namespace`/`command` with `options` as the body, addressed
/// by `scope` - or by `session_id`, which is an explicit address and wins -
/// and with `watch` stay on for that many seconds printing the events that
/// follow.
#[allow(clippy::too_many_arguments)]
pub async fn api(
    session: &Session,
    room: Option<&str>,
    namespace: &str,
    command: &str,
    options: Option<&str>,
    scope: RawScope,
    watch: Option<u64>,
    session_id: Option<&str>,
) -> Result<()> {
    let options: serde_json::Value = match options {
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
            connection = session::coordinator(session, &target).await?;
        }
        RawScope::Player => {
            // A player answers player-scoped commands only for itself, so
            // naming one over a socket to another gets ERROR_INVALID_OBJECT_ID
            // - "Incorrect playerId" - for an id that is perfectly correct.
            // The same resolution every per-player command uses, so `--scope
            // player` and `led` cannot drift on which speaker `--room` means.
            let target = session::target(&session.groups, room)?;
            let (player, upnp) = named_speaker(session, &target, room)?;
            envelope["playerId"] = json!(player.id);
            if upnp.ip() != connection.ip() {
                connection = Connection::open(upnp.ip()).await?;
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
        let err: sonos::proto::ErrorBody = serde_json::from_value(body.clone()).unwrap_or_default();
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
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
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
    Ok(())
}
