//! Machine-actionable errors: a stable `code` and, where one exists, the
//! `fix` command, carried alongside the human message.
//!
//! The prose an error prints is written for a person; an agent driving x2rock
//! would rather read a field than parse a sentence. So an error raised at a
//! site where the remedy is known carries a [`Hint`], and `x2rock <cmd> --json`
//! renders a failure as `{error, code, fix}`. An ordinary error carries no hint
//! and falls back to code `"unknown"` with a null `fix` - still structured, just
//! without a suggested command.
//!
//! A `Hint` is a normal `std::error::Error`, so it flows through `anyhow` like
//! any other and its `Display` is just the message - the daemon and the plain
//! CLI keep printing the same prose they always did. The code and fix are read
//! back at the top level by downcasting the error chain.

use std::fmt;

use anyhow::Error;
use serde_json::{Value, json};

/// An error that knows its own machine code and, when there is one, the command
/// that resolves it.
#[derive(Debug, Clone)]
pub struct Hint {
    pub message: String,
    /// A stable, snake_case identifier for the *kind* of failure - the thing an
    /// agent branches on. Stable across wording changes to the message.
    pub code: &'static str,
    /// The command that fixes it, verbatim and runnable, when one exists.
    pub fix: Option<String>,
    /// Extra machine-readable detail, merged into the `--json` error object - so
    /// an error can hand back what a caller would otherwise re-fetch. Must be a
    /// JSON object; its keys join `error`/`code`/`fix` (which it cannot shadow).
    pub data: Option<Value>,
}

impl Hint {
    pub fn new(message: impl Into<String>, code: &'static str, fix: Option<String>) -> Self {
        Self {
            message: message.into(),
            code,
            fix,
            data: None,
        }
    }

    /// Attach structured detail rendered into the JSON error alongside the
    /// standard fields - `unknown_room` uses it for `did_you_mean` and `rooms`.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl fmt::Display for Hint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Hint {}

/// One shell argument, quoted only when it needs to be.
///
/// A `fix` is promised **verbatim and runnable**, and most music service names
/// have a space in them: `x2rock link Classical Archives` is two positionals
/// and dies at argument parsing - `unexpected argument 'Archives'` - before
/// anything is contacted. An agent following "when `fix` is non-null, run it
/// and retry" gets a usage error rather than a link flow, which is the one
/// thing this field exists to prevent.
///
/// Single quotes rather than double, so nothing inside is expanded whatever a
/// vendor puts in a name, with the usual `'\''` dance for a name containing
/// one. Left bare when the whole argument is safe, because `x2rock link Deezer`
/// is what a person would type and quoting it would only add noise.
pub fn shell_arg(value: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '+' | ',')
    };
    if !value.is_empty() && value.chars().all(safe) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The `(code, fix)` an error carries, reading the first [`Hint`] in its chain.
/// A plain error - most of them - is `("unknown", None)`.
pub fn of(error: &Error) -> (&'static str, Option<String>) {
    error
        .downcast_ref::<Hint>()
        .map(|h| (h.code, h.fix.clone()))
        .unwrap_or(("unknown", None))
}

/// The `--json` error object for a failure: `{error, code, fix}`, plus any
/// `data` a [`Hint`] carried, merged in (and unable to shadow the three
/// standard keys). One place, so the CLI's error shape stays consistent.
pub fn error_json(error: &Error) -> Value {
    let hint = error.downcast_ref::<Hint>();
    let mut obj = serde_json::Map::new();
    obj.insert("error".into(), json!(format!("{error:#}")));
    obj.insert("code".into(), json!(hint.map_or("unknown", |h| h.code)));
    obj.insert("fix".into(), json!(hint.and_then(|h| h.fix.clone())));
    if let Some(Value::Object(extra)) = hint.and_then(|h| h.data.as_ref()) {
        for (key, value) in extra {
            obj.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    Value::Object(obj)
}

/// Wrap a failure to reach a player as a `no_player` error - unless the inner
/// error already carried a sharper *connection-layer* code, which describes
/// this same "get a player onto the network" problem more precisely. Only those
/// codes are inherited (with their fix): any other would describe a different
/// problem than this wrapper's message, since the message is overwritten here.
/// The fallback carries no fix: at this remove nothing knows whether the
/// current network is even one that may be scanned, and `x2rock discover` is
/// exactly the command [`unregistered_network`] exists to keep out of an
/// agent's "run the fix" reflex. Used where only a borrowed error is in hand
/// and it cannot be kept as a source.
pub fn no_player(inner: &Error, message: impl Into<String>) -> Error {
    match of(inner) {
        (code @ ("unregistered_network" | "no_player"), fix) => {
            Hint::new(message, code, fix).into()
        }
        _ => Hint::new(message, "no_player", None).into(),
    }
}

/// The error for a command run on a network x2rock has never discovered on.
///
/// The `fix` is deliberately null. `x2rock discover` scans the local network,
/// which must not be auto-run on an unfamiliar one (a cafe, a client site)
/// just because an agent follows a "run the fix" rule - the exact behaviour
/// the road-warrior design avoids. Discovery is offered, not run. The message
/// says how; the field withholds the command.
pub fn unregistered_network(fingerprint: &str) -> Error {
    Hint::new(
        format!(
            "unregistered network (gateway {fingerprint}): no speakers are known here. This is \
             normal away from home. `x2rock discover` will scan this network for speakers - \
             offer it rather than run it unasked - or pass `--ip <speaker>`."
        ),
        "unregistered_network",
        None,
    )
    .into()
}

/// A rescan found devices on the Sonos port, but none would complete a session:
/// players mid-reboot, or a Boost, which listens there and is not a player.
/// The same code as [`no_players_answered`], since to the caller it is the
/// same state - no player to talk to - with the same remedy.
pub fn none_completed_a_session(found: usize, last: &Error) -> Error {
    Hint::new(
        format!(
            "{found} device(s) answer on the Sonos port but none completed a session (last: \
             {last:#}); they may be mid-reboot, or not players at all. `x2rock discover` \
             re-checks once they should be back."
        ),
        "no_player",
        None,
    )
    .into()
}

/// The error for a known network where the remembered players did not answer
/// and the rescan `connect` just ran found nothing either.
///
/// No `fix`: `x2rock discover` runs the exact same scan that just came back
/// empty, so handing it out buys a fix-following agent a confident
/// discover/retry loop, not a resolution. The speakers are most likely powered
/// off; the message says so, and names `discover` only as the later,
/// deliberate re-check.
///
/// **Not for a rescan that found players, just not the ones asked for** - that
/// is [`household_unreachable`], which has a different cause and a fix that
/// works.
pub fn no_players_answered(previously: &[&str]) -> Error {
    Hint::new(
        format!(
            "no players found on this network (previously: {}): a rescan just ran and found \
             nothing, so they are most likely powered off. `x2rock discover` re-checks once \
             they should be back.",
            previously.join(", ")
        ),
        "no_player",
        None,
    )
    .into()
}

/// The error for a play or enqueue path that could not reach a player. Shared by
/// the search and browse `--play` sites so the two cannot word it differently.
pub fn no_player_to_play(inner: &Error) -> Error {
    no_player(inner, format!("no player to play it on: {inner:#}"))
}

/// The household errors share one shape: a message, a code, `x2rock
/// households` as the fix, and `data.households` as `(id, rooms)` per household
/// so `--json` gives an agent enough to build `--household` without a second
/// call - the same shape `unknown_room` uses for `did_you_mean`/`rooms`. One
/// core, so a fourth household error is one more two-line constructor rather
/// than a fourth copy of the fix string and the data shape.
fn household_hint(
    message: impl Into<String>,
    code: &'static str,
    households: &[(String, Vec<String>)],
) -> Error {
    let households: Vec<_> = households
        .iter()
        .map(|(id, rooms)| json!({ "id": id, "rooms": rooms }))
        .collect();
    Hint::new(message.into(), code, Some("x2rock households".into()))
        .with_data(json!({ "households": households }))
        .into()
}

pub fn multiple_households(households: &[(String, Vec<String>)]) -> Error {
    let summary: Vec<String> = households
        .iter()
        .map(|(id, rooms)| format!("{} ({id})", rooms.join(", ")))
        .collect();
    ambiguous_household(
        format!(
            "{} Sonos households are reachable on this network: {}. Pass --household <room> \
             (or, if that name is in more than one, --household <id>) to choose; `x2rock \
             households` lists them.",
            households.len(),
            summary.join("; "),
        ),
        households,
    )
}

/// The error for a `--household` selector that narrowed things down but not
/// all the way - a room name that is itself in more than one household, or an
/// id prefix that is not unique. Same code as [`multiple_households`], since
/// to the caller the state is the same one - still ambiguous, look at
/// `data.households` and try again with something more specific - just
/// reached with different wording depending on what was tried.
pub fn ambiguous_household(
    message: impl Into<String>,
    households: &[(String, Vec<String>)],
) -> Error {
    household_hint(message, "multiple_households", households)
}

/// The error for a household that did not answer a rescan **that other
/// households did answer**.
///
/// Distinct from [`no_players_answered`] on the evidence, and that distinction
/// is the whole point: a rescan that comes back empty means the speakers are
/// off, and `discover` would only repeat it. A rescan that comes back full
/// means the network is fine and *this* household is the thing that is gone -
/// powered off, moved to another network, or replaced. So this one carries a
/// fix that works and `data.households` naming what did answer, where the
/// other deliberately carries neither.
///
/// It had been reporting `no_player` with "a rescan just ran and found
/// nothing", which was false in exactly this branch and sent a fix-following
/// agent into a discover loop.
pub fn household_unreachable(
    wanted: &str,
    previously: &[&str],
    answered: &[(String, Vec<String>)],
) -> Error {
    let summary: Vec<String> = answered
        .iter()
        .map(|(id, rooms)| format!("{} ({id})", rooms.join(", ")))
        .collect();
    household_hint(
        format!(
            "the household holding {} did not answer, but {} other household(s) on this \
             network did: {}. It is most likely powered off, or has moved to another network. \
             (Looking for {wanted:?}.)",
            previously.join(", "),
            answered.len(),
            summary.join("; "),
        ),
        "household_unreachable",
        answered,
    )
}

/// The error for a `--household` that matched none of the households actually
/// reachable - a stale id, a room name that moved, a typo. Distinct from
/// [`multiple_households`]/[`ambiguous_household`], which fire when something
/// still needs narrowing; this fires when what was passed helped not at all.
pub fn unknown_household(selector: &str, households: &[(String, Vec<String>)]) -> Error {
    household_hint(
        format!(
            "no household on this network has a room or an id matching {selector:?} (from \
             --room or --household); `x2rock households` lists what is actually reachable"
        ),
        "unknown_household",
        households,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn a_shell_argument_is_quoted_only_when_it_has_to_be() {
        // The whole point: a name with a space is one argument, not two.
        assert_eq!(shell_arg("Classical Archives"), "'Classical Archives'");
        assert_eq!(shell_arg("TuneIn (New)"), "'TuneIn (New)'");
        assert_eq!(
            shell_arg("80s80s - REAL 80s Radio"),
            "'80s80s - REAL 80s Radio'"
        );

        // And a plain one is left as a person would type it.
        assert_eq!(shell_arg("Bandcamp"), "Bandcamp");
        assert_eq!(shell_arg("Piraten.FM"), "Piraten.FM");
        assert_eq!(shell_arg("90s90s"), "90s90s");

        // Single quotes, so nothing inside is ever expanded by the shell.
        assert_eq!(shell_arg("a $HOME `b`"), "'a $HOME `b`'");
        // The awkward one: closing, escaping, reopening.
        assert_eq!(shell_arg("Rock'n'Roll"), r"'Rock'\''n'\''Roll'");
        // Empty is quoted, so it stays an argument rather than vanishing.
        assert_eq!(shell_arg(""), "''");
    }

    #[test]
    fn a_hinted_error_yields_its_code_and_fix() {
        let e: Error = Hint::new(
            "deezer needs an account",
            "needs_link",
            Some("x2rock link deezer".into()),
        )
        .into();
        assert_eq!(of(&e), ("needs_link", Some("x2rock link deezer".into())));
        // Display stays the plain message, so prose output is unchanged.
        assert_eq!(format!("{e}"), "deezer needs an account");
    }

    #[test]
    fn a_hint_is_found_through_added_context() {
        // A hint keeps its code even when a caller wraps it with more context.
        let e: Error = Err::<(), _>(Hint::new(
            "no such room",
            "unknown_room",
            Some("x2rock rooms".into()),
        ))
        .context("resolving the target")
        .unwrap_err();
        assert_eq!(of(&e).0, "unknown_room");
    }

    /// The two "could not reach it" errors differ on evidence, and that
    /// difference is the whole reason the second one exists: an empty rescan
    /// means the speakers are off and `discover` would only repeat it, while a
    /// rescan that found *other* households means the network is fine and this
    /// household is what is missing - which has a fix, and a list worth
    /// carrying. Reporting the second as the first sent agents into a discover
    /// loop on a claim ("a rescan found nothing") that was false.
    #[test]
    fn an_unreachable_household_is_not_an_empty_network() {
        let empty = no_players_answered(&["Kitchen", "Bedroom"]);
        assert_eq!(of(&empty), ("no_player", None));
        assert!(format!("{empty:#}").contains("found nothing"));

        let answered = vec![("hh:OTHER".to_string(), vec!["Studio".to_string()])];
        let gone = household_unreachable("hh:MINE", &["Kitchen", "Bedroom"], &answered);
        let (code, fix) = of(&gone);
        assert_eq!(code, "household_unreachable");
        assert_eq!(fix.as_deref(), Some("x2rock households"));

        // It must not repeat the empty-network claim, and must name what did
        // answer so a caller can see its own household is absent.
        let text = format!("{gone:#}");
        assert!(!text.contains("found nothing"), "{text}");
        assert!(text.contains("Kitchen, Bedroom"), "{text}");
        assert!(text.contains("Studio"), "{text}");

        // `data.households` is the same shape the other household errors use.
        let v = error_json(&gone);
        assert_eq!(
            v["households"],
            json!([{ "id": "hh:OTHER", "rooms": ["Studio"] }])
        );
    }

    #[test]
    fn a_plain_error_falls_back_to_a_generic_code() {
        let e = anyhow!("something went wrong");
        assert_eq!(of(&e), ("unknown", None));
    }

    #[test]
    fn error_json_merges_hint_data_but_cannot_shadow_the_standard_fields() {
        let e: Error = Hint::new(
            "no room named \"x\"",
            "unknown_room",
            Some("x2rock rooms".into()),
        )
        // Includes a rogue "code" key that must NOT override the real one.
        .with_data(json!({ "did_you_mean": ["Bedroom"], "code": "hijacked" }))
        .into();
        let v = error_json(&e);
        assert_eq!(v["code"], "unknown_room", "data must not shadow code");
        assert_eq!(v["fix"], "x2rock rooms");
        assert_eq!(v["did_you_mean"], json!(["Bedroom"]));
        assert!(v["error"].as_str().unwrap().contains("no room named"));

        // A plain error still renders the three fields, with a null fix.
        let plain = error_json(&anyhow!("boom"));
        assert_eq!(plain["code"], "unknown");
        assert!(plain["fix"].is_null());
    }

    #[test]
    fn no_player_defaults_but_keeps_a_sharper_inner_code() {
        // A plain connection failure becomes no_player - with no fix: nothing
        // at this remove knows whether this network is one that may be scanned,
        // so a runnable `discover` must not be minted here (a stale --ip on a
        // cafe network reaches exactly this arm).
        let plain = no_player(&anyhow!("timed out"), "no player to play it on");
        assert_eq!(of(&plain), ("no_player", None));

        // But an unregistered network keeps its own, better diagnosis - the
        // more specific code, and its deliberately null fix.
        let inner: Error = unregistered_network("192.168.1.1");
        let wrapped = no_player(&inner, "no player to play it on");
        assert_eq!(of(&wrapped), ("unregistered_network", None));
    }

    #[test]
    fn no_player_does_not_inherit_a_non_connection_code() {
        // A code from a different layer must not ride on this wrapper's message:
        // an "unknown_room" fix (`x2rock rooms`) would not match "no player to
        // play it on". It falls back to no_player rather than being adopted.
        let inner: Error =
            Hint::new("no such room", "unknown_room", Some("x2rock rooms".into())).into();
        let wrapped = no_player(&inner, "no player to play it on");
        assert_eq!(of(&wrapped), ("no_player", None));
    }

    #[test]
    fn the_network_errors_never_carry_a_runnable_scan() {
        // The safety property the road-warrior design rests on: no network
        // error may hand an agent `x2rock discover` in `fix`, because the
        // "when fix is non-null, run it and retry" contract would then scan an
        // unfamiliar network (unregistered_network) or loop the scan that just
        // came back empty (no_player). The message offers the command; the
        // field withholds it. These are the constructors the production sites
        // in session.rs use, so a regression there fails here.
        let unregistered = unregistered_network("192.168.86.1");
        assert_eq!(of(&unregistered), ("unregistered_network", None));
        assert!(
            format!("{unregistered}").contains("`x2rock discover`"),
            "discovery is offered in prose"
        );

        let silent = no_players_answered(&["Kitchen", "Bedroom"]);
        assert_eq!(of(&silent), ("no_player", None));
        assert!(format!("{silent}").contains("previously: Kitchen, Bedroom"));
    }

    #[test]
    fn multiple_households_names_each_ones_rooms_and_carries_the_ids() {
        // Unlike the unregistered/no_player cases above, `households` is safe
        // to hand out as a fix: this fires only once we already know real
        // Sonos systems are on *this*, already-scanned network - it is not
        // the unprompted-scan-of-a-stranger's-network hazard those guard
        // against.
        let e = multiple_households(&[
            ("hh:1".into(), vec!["Media Room".into(), "Kitchen".into()]),
            ("hh:2".into(), vec!["Kitchen".into(), "Office".into()]),
        ]);
        assert_eq!(
            of(&e),
            ("multiple_households", Some("x2rock households".into()))
        );
        let msg = format!("{e}");
        assert!(msg.contains("2 Sonos households"));
        assert!(msg.contains("--household <room>"));

        let v = error_json(&e);
        assert_eq!(
            v["households"],
            json!([
                { "id": "hh:1", "rooms": ["Media Room", "Kitchen"] },
                { "id": "hh:2", "rooms": ["Kitchen", "Office"] },
            ])
        );
    }
}
