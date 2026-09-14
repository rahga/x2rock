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

use std::path::{Path, PathBuf};

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

/// The path the binary was invoked by, kept exactly as invoked.
///
/// Not `current_exe()`, and not canonicalised - both resolve symlinks, and on
/// Linux `current_exe()` reads `/proc/self/exe`, which already has. That is
/// the wrong path to put in a unit: Nix, Homebrew and stow all install a
/// *stable* symlink on `PATH` whose target is a versioned directory that goes
/// away on the next upgrade. The symlink is the durable name; its target is
/// the thing that breaks. So: `argv[0]` if absolute; joined to the working
/// directory if it has a slash; otherwise found on `PATH` the way the shell
/// found it - the first directory holding a file of that name.
///
/// `None` when none of that produces a path, which the caller answers with
/// `current_exe()` as the fallback it always was.
pub fn invoked_path(argv0: &str, cwd: &Path, path_var: Option<&str>) -> Option<PathBuf> {
    let given = Path::new(argv0);
    if given.is_absolute() {
        return Some(given.to_path_buf());
    }
    if argv0.contains('/') {
        return Some(cwd.join(given));
    }
    path_var?
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(argv0))
        .find(|candidate| candidate.is_file())
}

/// What is on disk at the unit's path, judged against what would be written.
#[derive(Debug, PartialEq, Eq)]
pub enum Existing<'a> {
    /// Byte-identical: nothing to do.
    Same,
    /// Ours, and differing only in the lines this command owns - the header,
    /// `ExecStart`, the household line. Safe to overwrite: that is what a
    /// re-run after a move or an upgrade is for.
    Generated,
    /// Either not ours at all (no header - a hand-copied unit) or ours with
    /// edits beyond the lines we own. Refused without `--force`, and these are
    /// the lines to show: theirs that would go, ours that would replace them.
    HandEdited {
        yours: Vec<&'a str>,
        new: Vec<&'a str>,
    },
}

/// The lines this command owns and may rewrite without asking: its own header
/// and the two substitutions. Everything else in the file is the person's.
fn is_owned_line(line: &str) -> bool {
    line.starts_with("# Written by `x2rock service install`")
        || line.starts_with("# Re-run it after moving or reinstalling")
        || line.starts_with("# edits unless told to with --force")
        || line.starts_with("ExecStart=")
        || line.starts_with("Environment=\"X2ROCK_HOUSEHOLD=")
        || line.starts_with("#Environment=X2ROCK_HOUSEHOLD=")
}

/// Judge an existing file. See [`Existing`].
pub fn classify<'a>(existing: &'a str, proposed: &'a str) -> Existing<'a> {
    if existing == proposed {
        return Existing::Same;
    }
    let ours = existing.starts_with("# Written by `x2rock service install`");
    let rest =
        |text: &'a str| -> Vec<&'a str> { text.lines().filter(|l| !is_owned_line(l)).collect() };
    if ours && rest(existing) == rest(proposed) {
        return Existing::Generated;
    }
    let (yours, new) = changed_lines(existing, proposed);
    // The header lines are ours to change and not worth showing as a "diff".
    let not_header = |l: &&str| {
        !l.starts_with("# Written by")
            && !l.starts_with("# Re-run it")
            && !l.starts_with("# edits unless")
    };
    Existing::HandEdited {
        yours: yours.into_iter().filter(not_header).collect(),
        new: new.into_iter().filter(not_header).collect(),
    }
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

/// Whether a running daemon is on a different binary from the one installed.
///
/// `running` is `/proc/<pid>/exe` as `read_link` gives it. Linux appends
/// ` (deleted)` once the file the process started from has been replaced, which
/// is what an upgrade in place does (`cargo install` again, or `install` over
/// the same path) - the unit is then byte-identical, so nothing else would say
/// the daemon is stale. Otherwise the paths are compared resolved, because
/// `/proc` names the binary resolved and `installed` is resolved to match.
pub fn runs_stale_binary(running: &Path, installed: &Path) -> bool {
    running.to_string_lossy().ends_with(" (deleted)") || running != installed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_or_different_binary_is_stale() {
        let installed = Path::new("/home/me/.local/bin/x2rock");
        assert!(!runs_stale_binary(installed, installed));
        assert!(runs_stale_binary(
            Path::new("/home/me/.local/bin/x2rock (deleted)"),
            installed
        ));
        assert!(runs_stale_binary(
            Path::new("/home/me/.cargo/bin/x2rock"),
            installed
        ));
    }
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

    /// The unit must name the path a person's shell used, not where it led.
    #[test]
    fn the_invoked_path_is_kept_as_invoked() {
        let cwd = Path::new("/work");
        assert_eq!(
            invoked_path("/opt/homebrew/bin/x2rock", cwd, None),
            Some(PathBuf::from("/opt/homebrew/bin/x2rock"))
        );
        assert_eq!(
            invoked_path("./target/release/x2rock", cwd, None),
            Some(PathBuf::from("/work/./target/release/x2rock"))
        );
        // A bare name is found the way the shell found it: first PATH entry
        // holding a file of that name. A directory that exists but holds no
        // such file is skipped, and an empty PATH entry is ignored.
        let dir = std::env::temp_dir().join(format!("x2rock-invoked-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("has")).unwrap();
        std::fs::create_dir_all(dir.join("lacks")).unwrap();
        std::fs::write(dir.join("has").join("x2rock"), b"").unwrap();
        let path_var = format!(
            "{}::{}",
            dir.join("lacks").display(),
            dir.join("has").display()
        );
        assert_eq!(
            invoked_path("x2rock", cwd, Some(&path_var)),
            Some(dir.join("has").join("x2rock"))
        );
        assert_eq!(
            invoked_path(
                "x2rock",
                cwd,
                Some(&dir.join("lacks").display().to_string())
            ),
            None
        );
        assert_eq!(invoked_path("x2rock", cwd, None), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The three verdicts, and the boundary between them: a re-run after a move
    /// or an upgrade changes only lines this command owns and must go through;
    /// a person's edit to anything else must not be silently undone.
    #[test]
    fn a_generated_unit_is_overwritten_and_a_hand_edited_one_is_not() {
        let exe_a = PathBuf::from("/home/me/.cargo/bin/x2rock");
        let exe_b = PathBuf::from("/usr/bin/x2rock");
        let a = render_unit(&exe_a, None).unwrap();
        let b = render_unit(&exe_b, None).unwrap();
        let a_hh = render_unit(&exe_a, Some("Studio")).unwrap();

        assert_eq!(classify(&a, &a), Existing::Same);
        // The binary moved: only ExecStart differs. Ours to rewrite.
        assert_eq!(classify(&a, &b), Existing::Generated);
        // A household added or removed: ours too.
        assert_eq!(classify(&a, &a_hh), Existing::Generated);
        assert_eq!(classify(&a_hh, &a), Existing::Generated);
        // An upgrade that changed nothing but the version in the header: ours,
        // and this used to be refused with an empty diff, since the only
        // differing line was a comment the listing hid.
        let bumped = a.replacen("(x2rock 0.1.0)", "(x2rock 9.9.9)", 1);
        assert_ne!(bumped, a, "the fixture must actually differ");
        assert_eq!(classify(&bumped, &a), Existing::Generated);

        // A person changed the restart policy: not ours. Their line and its
        // replacement are what gets shown; no header noise.
        let edited = a.replace("RestartSec=5", "RestartSec=30");
        match classify(&edited, &a) {
            Existing::HandEdited { yours, new } => {
                assert_eq!(yours, ["RestartSec=30"]);
                assert_eq!(new, ["RestartSec=5"]);
            }
            other => panic!("expected HandEdited, got {other:?}"),
        }
        // A unit copied from systemd/ by hand has no header: not ours, even
        // though it differs only in ExecStart. Refused, and the diff says so.
        match classify(UNIT_TEMPLATE, &a) {
            Existing::HandEdited { yours, new } => {
                assert_eq!(yours, [EXEC_MARKER]);
                assert!(
                    new.iter().any(|l| l.starts_with("ExecStart=\"/home/me")),
                    "{new:?}"
                );
            }
            other => panic!("expected HandEdited, got {other:?}"),
        }
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
