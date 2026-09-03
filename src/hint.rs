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
}

impl Hint {
    pub fn new(message: impl Into<String>, code: &'static str, fix: Option<String>) -> Self {
        Self {
            message: message.into(),
            code,
            fix,
        }
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
}
