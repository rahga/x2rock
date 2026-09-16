//! One module per family of commands. `run` in `main.rs` parses, resolves the
//! room and hands off here; nothing in this tree parses arguments and nothing
//! in `main.rs` talks to a speaker. Each file is named for what the person is
//! doing - installing, playing, adjusting a speaker - not for a Sonos API.

pub mod admin;
pub mod content;
pub mod services;
pub mod stream;

use anyhow::{Result, bail};

use crate::sonos::upnp;

/// An exact id wins, then a case-insensitive substring of the name; among
/// several of those, a whole-name match settles it, and anything else is
/// ambiguous and says so - naming `hint` as the command that lists them.
pub fn find_named<'a, T>(
    items: &'a [T],
    query: &str,
    id: impl Fn(&T) -> &str,
    name: impl Fn(&T) -> &str,
    what: &str,
    hint: &str,
) -> Result<&'a T> {
    if let Some(exact) = items.iter().find(|i| id(i) == query) {
        return Ok(exact);
    }
    let needle = query.to_lowercase();
    let matches: Vec<_> = items
        .iter()
        .filter(|i| name(i).to_lowercase().contains(&needle))
        .collect();

    match matches.as_slice() {
        [] => bail!("no {what} matches {query:?}. `{hint}` lists them."),
        [only] => Ok(only),
        several => {
            // An exact name wins over the substrings around it - but only when
            // it is unique. Two favorites *named the same* (the household ages
            // into these) cannot be told apart by name, so name the ids rather
            // than silently pick the first.
            let exact: Vec<_> = several
                .iter()
                .filter(|i| name(i).to_lowercase() == needle)
                .collect();
            match exact.as_slice() {
                [whole] => return Ok(whole),
                [_, ..] => {
                    let shown: Vec<_> = exact
                        .iter()
                        .map(|i| format!("{} (id {})", name(i), id(i)))
                        .collect();
                    bail!(
                        "{} {what}s are named {query:?}: {}. Give an id to pick one.",
                        exact.len(),
                        shown.join(", ")
                    );
                }
                [] => {}
            }
            let shown: Vec<_> = several.iter().take(8).map(|i| name(i)).collect();
            bail!(
                "{} {what}s match {query:?}: {}{}",
                several.len(),
                shown.join(", "),
                if several.len() > shown.len() {
                    ", ..."
                } else {
                    ""
                }
            )
        }
    }
}

/// "old → " when a command changed something, nothing when it only reported.
pub fn transition(before: &str, after: &str) -> String {
    if before == after {
        String::new()
    } else {
        format!("{before} → ")
    }
}

pub fn mmss(duration: Option<std::time::Duration>) -> String {
    match duration {
        Some(d) => format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60),
        None => String::new(),
    }
}

/// Whether an error is the *player* declining, as opposed to not being reached.
///
/// The distinction the `Fault` type exists to draw, asked in three places: the
/// two enqueue fallbacks below and `raw upnp`. A refusal means "this is not
/// queue material", which is a reason to try the stream session instead; a
/// timeout or a dead socket means nothing of the kind, and falling back on one
/// spends a second round trip to fail the same way while printing a sentence
/// that blames the content.
pub fn is_refusal(e: &anyhow::Error) -> bool {
    upnp::Fault::of(e).is_some()
}

/// `on`/`off`, for every flag and argument that takes those two words.
///
/// Hoisted out of `apply_eq`'s closure once `remote`, `led`, `shuffle` and
/// `crossfade` all wanted the same three lines and the same message.
pub fn on_off(what: &str, text: Option<&str>) -> Result<Option<bool>> {
    match text {
        None => Ok(None),
        Some(word @ ("on" | "off")) => Ok(Some(word == "on")),
        Some(_) => bail!("{what} takes on or off"),
    }
}

/// The word for a boolean, for the read-back lines.
pub fn on_word(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn find_named_disambiguates_two_of_the_same_name() {
        fn id(i: &(String, String)) -> &str {
            i.0.as_str()
        }
        fn name(i: &(String, String)) -> &str {
            i.1.as_str()
        }
        let items = [
            ("fv1".to_string(), "That Christmas Channel".to_string()),
            ("fv2".to_string(), "That Christmas Channel".to_string()),
            ("fv7".to_string(), "Jazz24".to_string()),
        ];
        // A unique name resolves; an exact id always resolves.
        assert_eq!(
            find_named(&items, "jazz24", id, name, "f", "h").unwrap().0,
            "fv7"
        );
        assert_eq!(
            find_named(&items, "fv2", id, name, "f", "h").unwrap().0,
            "fv2"
        );
        // Two favorites sharing a name are not silently reduced to the first -
        // the error names both ids so a caller can pick one.
        let err = find_named(&items, "That Christmas Channel", id, name, "favorite", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fv1") && err.contains("fv2"), "{err}");
        assert!(err.contains("Give an id"), "{err}");
    }

    /// The enqueue fallbacks turn on this one question, so it has to survive a
    /// `.context()` layer: a refusal wrapped in explanation is still a refusal,
    /// and reading it as a transport failure would silently retire the stream
    /// fallback that `play_item` and `bookmark` depend on.
    #[test]
    fn only_a_player_refusal_counts_as_a_refusal() {
        let fault = anyhow!(upnp::Fault {
            action: "AddURIToQueue".into(),
            kind: upnp::FaultKind::Action("800".into()),
            detail: String::new(),
        });
        assert!(is_refusal(&fault));
        // A per-action refusal is not the transport being off - checked before
        // `.context()` below consumes `fault`.
        assert!(upnp::Fault::of(&fault).is_some_and(upnp::Fault::is_per_action));
        assert!(
            is_refusal(&fault.context("enqueuing the track")),
            "a refusal must stay recognisable under added context"
        );

        // The cases that must NOT take the fallback: the speaker was never
        // reached, so the stream session cannot help and would fail the same
        // way a round trip later.
        // HTTP 403 - UPnP switched off in the Sonos app - is the player
        // declining too, and the one case where the fallback matters most:
        // `stream_item` is pure Control API, so it still works on a household
        // where every UPnP call is refused.
        let forbidden = anyhow!(upnp::Fault {
            action: "AddURIToQueue".into(),
            kind: upnp::FaultKind::UpnpDisabled,
            detail: "UPnP is turned off".into(),
        });
        assert!(is_refusal(&forbidden));
        // ...but it is the transport being off, not one action refused, which
        // is the distinction `raw upnp` draws and the fallbacks do not.
        assert!(upnp::Fault::of(&forbidden).is_some_and(|f| !f.is_per_action()));

        assert!(!is_refusal(&anyhow!("connection refused")));
        assert!(!is_refusal(
            &anyhow!("timed out after 8s").context("reaching Kitchen")
        ));
    }
}
