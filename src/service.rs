//! `x2rock service install`: write the systemd user unit for the daemon,
//! pointing at the binary that is actually running.
//!
//! The shipped unit hardcodes `%h/.local/bin/x2rock`, which is right for a
//! clone-and-`install` and wrong for `cargo install` (`~/.cargo/bin`) and for a
//! distro package (`/usr/bin`). A bare `ExecStart=x2rock` is worse: a unit's
//! executable is resolved against the *user manager's* `PATH`, not the shell's,
//! and even on a desktop that imports one `~/.cargo/bin` is not in it - on a
//! headless box nothing is. So the unit is rendered from the shipped file with
//! `ExecStart` set to `std::env::current_exe()`, which resolves through
//! symlinks to whatever was installed by whichever route.
//!
//! Everything here that decides *what* to write is pure and tested; the file
//! write and the `systemctl` calls live in `main.rs` with the other commands.

use std::path::Path;

use anyhow::{Result, bail};

/// The unit as shipped in `systemd/`, embedded so this and the copy a person
/// might install by hand cannot say different things.
pub const UNIT_TEMPLATE: &str = include_str!("../systemd/x2rock.service");
/// The drop-in for a machine with no graphical session, likewise.
pub const HEADLESS_TEMPLATE: &str = include_str!("../systemd/x2rock.service.d/headless.conf");

/// The line in the shipped unit that names the binary. Replaced, never
/// appended to, so a second `ExecStart` cannot sneak in; a test holds the
/// shipped file to containing it exactly once.
const EXEC_MARKER: &str = "ExecStart=%h/.local/bin/x2rock daemon";
/// The commented household line in the shipped unit, uncommented and filled in
/// when a household is given.
const HOUSEHOLD_MARKER: &str = "#Environment=X2ROCK_HOUSEHOLD=Studio";

/// The first lines of anything this writes, so a later run - or a person - can
/// tell a generated file from a hand-copied one, and knows how to refresh it.
fn header() -> String {
    format!(
        "# Written by `x2rock service install` (x2rock {}).\n\
         # Re-run it after moving or reinstalling the binary; it refuses to overwrite\n\
         # edits unless told to with --force.\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// A value as systemd's unit-file parser wants it inside double quotes.
///
/// Two things bite here. Backslash and double-quote need escaping inside a
/// quoted word. And `%` is a *specifier* in unit files - `%h` is the home
/// directory - so a `%` in a path or a room name must be doubled or systemd
/// expands it. A room called "100% Jazz" is not far-fetched.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The unit to install, from the shipped template.
///
/// `exe` is the binary to run - the caller passes `current_exe()`. `household`
/// fills in and uncomments the `X2ROCK_HOUSEHOLD` line; `None` leaves it as the
/// commented explanation it is in the shipped file.
pub fn render_unit(exe: &Path, household: Option<&str>) -> Result<String> {
    let exe = exe.to_str().ok_or_else(|| {
        anyhow::anyhow!("the binary's path is not valid UTF-8, so it cannot go in a unit file")
    })?;
    if UNIT_TEMPLATE.matches(EXEC_MARKER).count() != 1 {
        bail!("the shipped unit no longer carries exactly one `{EXEC_MARKER}` line");
    }
    let mut unit = UNIT_TEMPLATE.replace(EXEC_MARKER, &format!("ExecStart={} daemon", quoted(exe)));
    if let Some(selector) = household {
        if !unit.contains(HOUSEHOLD_MARKER) {
            bail!("the shipped unit no longer carries the `{HOUSEHOLD_MARKER}` line");
        }
        unit = unit.replace(
            HOUSEHOLD_MARKER,
            &format!(
                "Environment={}",
                quoted(&format!("X2ROCK_HOUSEHOLD={selector}"))
            ),
        );
    }
    Ok(header() + &unit)
}

/// The headless drop-in to install, from the shipped template.
pub fn render_headless() -> String {
    header() + HEADLESS_TEMPLATE
}

/// The lines that differ between what is on disk and what would be written:
/// those only in the existing file, and those only in the new one.
///
/// A set difference by line, not a real diff - enough to show a person which
/// of their edits would be lost, without a dependency for it. Order is kept.
pub fn changed_lines<'a>(existing: &'a str, proposed: &'a str) -> (Vec<&'a str>, Vec<&'a str>) {
    let only_in = |a: &'a str, b: &'a str| -> Vec<&'a str> {
        let b_lines: Vec<&str> = b.lines().collect();
        a.lines().filter(|l| !b_lines.contains(l)).collect()
    };
    (only_in(existing, proposed), only_in(proposed, existing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The substitution is a string replacement on the shipped file, so the
    /// file has to keep carrying the exact lines it replaces - and only once
    /// each, or a second `ExecStart` would appear.
    #[test]
    fn the_shipped_unit_still_carries_the_lines_this_rewrites() {
        assert_eq!(UNIT_TEMPLATE.matches(EXEC_MARKER).count(), 1);
        assert_eq!(UNIT_TEMPLATE.matches(HOUSEHOLD_MARKER).count(), 1);
        assert!(HEADLESS_TEMPLATE.contains("WantedBy=default.target"));
    }

    #[test]
    fn exec_start_names_the_running_binary_and_nothing_else_moves() {
        let unit = render_unit(&PathBuf::from("/home/me/.cargo/bin/x2rock"), None).unwrap();
        assert!(unit.contains("ExecStart=\"/home/me/.cargo/bin/x2rock\" daemon\n"));
        assert!(
            !unit.contains(EXEC_MARKER),
            "the template line must be replaced, not kept"
        );
        assert_eq!(unit.matches("ExecStart=").count(), 1);
        // The rest of the shipped unit survives untouched.
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=graphical-session.target"));
        // Without a household the explanatory comment stays a comment.
        assert!(unit.contains(HOUSEHOLD_MARKER));
        assert!(unit.starts_with("# Written by `x2rock service install`"));
    }

    /// The household line is where the daemon's operator will actually set the
    /// selector, so it has to come out as systemd will read it - quoted,
    /// because a room name has spaces.
    #[test]
    fn a_household_uncomments_the_line_quoted() {
        let unit = render_unit(&PathBuf::from("/usr/bin/x2rock"), Some("Living Room")).unwrap();
        assert!(
            unit.contains("\nEnvironment=\"X2ROCK_HOUSEHOLD=Living Room\"\n"),
            "{unit}"
        );
        assert!(!unit.contains(HOUSEHOLD_MARKER));
        assert_eq!(unit.matches("Environment=").count(), 1);
    }

    /// `%` is a unit-file specifier and `"` ends a quoted word, so both must
    /// be escaped or systemd reads something other than what was typed.
    #[test]
    fn specifiers_and_quotes_are_escaped_for_systemd() {
        let unit = render_unit(
            &PathBuf::from("/opt/100% tools/x2rock"),
            Some("Jazz \"Room\" 50%"),
        )
        .unwrap();
        assert!(
            unit.contains("ExecStart=\"/opt/100%% tools/x2rock\" daemon"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"X2ROCK_HOUSEHOLD=Jazz \\\"Room\\\" 50%%\""),
            "{unit}"
        );
    }

    #[test]
    fn the_headless_dropin_is_the_shipped_one_with_a_header() {
        let dropin = render_headless();
        assert!(dropin.starts_with("# Written by"));
        assert!(dropin.ends_with(HEADLESS_TEMPLATE));
    }

    /// What the refusal shows: the person's lines that would be lost, and ours
    /// that would replace them. Identical files show nothing.
    #[test]
    fn changed_lines_shows_each_sides_own_lines() {
        let (gone, added) = changed_lines("a\nkeep\nb\n", "keep\nc\n");
        assert_eq!(gone, ["a", "b"]);
        assert_eq!(added, ["c"]);
        let (gone, added) = changed_lines("same\n", "same\n");
        assert!(gone.is_empty() && added.is_empty());
    }
}
