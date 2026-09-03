//! Machine-actionable errors: a stable `code` and, where one exists, the
//! `fix` command, carried alongside the human message.
//!
//! The prose an error prints is written for a person; an agent driving x2rock
//! would rather read a field than parse a sentence. So an error raised at a
//! site where the remedy is known carries a [`Hint`], and `x2rock <cmd> --json`
//! renders a failure as `{error, code, fix}`. An ordinary error carries no hint
//! and falls back to code `"error"` with a null `fix` - still structured, just
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

/// The `(code, fix)` an error carries, reading the first [`Hint`] in its chain.
/// A plain error - most of them - is `("error", None)`.
pub fn of(error: &Error) -> (&'static str, Option<String>) {
    error
        .downcast_ref::<Hint>()
        .map(|h| (h.code, h.fix.clone()))
        .unwrap_or(("error", None))
}

/// The `--json` error object for a failure: `{error, code, fix}`, plus any
/// `data` a [`Hint`] carried, merged in (and unable to shadow the three
/// standard keys). One place, so the CLI's error shape stays consistent.
pub fn error_json(error: &Error) -> Value {
    let hint = error.downcast_ref::<Hint>();
    let mut obj = serde_json::Map::new();
    obj.insert("error".into(), json!(format!("{error:#}")));
    obj.insert("code".into(), json!(hint.map_or("error", |h| h.code)));
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
/// this same "get a player onto the network" problem more precisely and whose
/// fix is the same `x2rock discover`. Only those codes are inherited: any other
/// would describe a different problem than this wrapper's message, since the
/// message is overwritten here. Used where only a borrowed error is in hand and
/// it cannot be kept as a source.
pub fn no_player(inner: &Error, message: impl Into<String>) -> Error {
    match of(inner) {
        (code @ ("unregistered_network" | "no_player"), fix) => Hint::new(message, code, fix).into(),
        _ => Hint::new(message, "no_player", Some("x2rock discover".into())).into(),
    }
}

/// The error for a play or enqueue path that could not reach a player. Shared by
/// the search and browse `--play` sites so the two cannot word it differently.
pub fn no_player_to_play(inner: &Error) -> Error {
    no_player(inner, format!("no player to play it on: {inner:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    #[test]
    fn a_hinted_error_yields_its_code_and_fix() {
        let e: Error = Hint::new("unregistered network", "unregistered_network", Some("x2rock discover".into())).into();
        assert_eq!(of(&e), ("unregistered_network", Some("x2rock discover".into())));
        // Display stays the plain message, so prose output is unchanged.
        assert_eq!(format!("{e}"), "unregistered network");
    }

    #[test]
    fn a_hint_is_found_through_added_context() {
        // A hint keeps its code even when a caller wraps it with more context.
        let e: Error = Err::<(), _>(Hint::new("no such room", "unknown_room", Some("x2rock rooms".into())))
            .context("resolving the target")
            .unwrap_err();
        assert_eq!(of(&e).0, "unknown_room");
    }

    #[test]
    fn a_plain_error_falls_back_to_a_generic_code() {
        let e = anyhow!("something went wrong");
        assert_eq!(of(&e), ("error", None));
    }

    #[test]
    fn error_json_merges_hint_data_but_cannot_shadow_the_standard_fields() {
        let e: Error = Hint::new("no room named \"x\"", "unknown_room", Some("x2rock rooms".into()))
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
        assert_eq!(plain["code"], "error");
        assert!(plain["fix"].is_null());
    }

    #[test]
    fn no_player_defaults_but_keeps_a_sharper_inner_code() {
        // A plain connection failure becomes no_player, fix discover.
        let plain = no_player(&anyhow!("timed out"), "no player to play it on");
        assert_eq!(of(&plain), ("no_player", Some("x2rock discover".into())));

        // But an unregistered network keeps its own, better diagnosis - the fix
        // is the same command, and the code is more specific.
        let inner: Error =
            Hint::new("unregistered network", "unregistered_network", Some("x2rock discover".into())).into();
        let wrapped = no_player(&inner, "no player to play it on");
        assert_eq!(of(&wrapped).0, "unregistered_network");
    }

    #[test]
    fn no_player_does_not_inherit_a_non_connection_code() {
        // A code from a different layer must not ride on this wrapper's message:
        // an "unknown_room" fix (`x2rock rooms`) would not match "no player to
        // play it on". It falls back to no_player rather than being adopted.
        let inner: Error =
            Hint::new("no such room", "unknown_room", Some("x2rock rooms".into())).into();
        let wrapped = no_player(&inner, "no player to play it on");
        assert_eq!(of(&wrapped), ("no_player", Some("x2rock discover".into())));
    }
}
