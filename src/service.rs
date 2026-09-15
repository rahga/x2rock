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
//! A re-run judges the unit already on disk by its *directives* - the lines
//! systemd reads - and not by its comments: the shipped explanation changes
//! between versions, and a unit copied from `systemd/` before the household
//! block existed must not be refused over prose. Only a directive this command
//! does not own, changed by a person, is worth stopping for.
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
/// The desktop entry file as shipped in `desktop/`.
pub const DESKTOP_ENTRY: &str = include_str!("../desktop/x2rock.desktop");
/// The application SVG icon as shipped in `desktop/`.
pub const DESKTOP_ICON: &str = include_str!("../desktop/x2rock.svg");

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

/// The default user paths for the desktop entry and icon.
pub fn desktop_paths() -> Result<(PathBuf, PathBuf)> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow::anyhow!("no home directory found to install desktop files"))?;
    let data = base.data_local_dir();
    let desktop = data.join("applications").join("x2rock.desktop");
    let icon = data
        .join("icons")
        .join("hicolor")
        .join("scalable")
        .join("apps")
        .join("x2rock.svg");
    Ok((desktop, icon))
}

/// Install the desktop entry and icon for MPRIS application identity.
pub fn install_desktop_files() -> Result<(PathBuf, PathBuf)> {
    use anyhow::Context;
    let (desktop, icon) = desktop_paths()?;
    if let Some(parent) = desktop.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(&desktop, DESKTOP_ENTRY)
        .with_context(|| format!("writing {}", desktop.display()))?;

    if let Some(parent) = icon.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(&icon, DESKTOP_ICON).with_context(|| format!("writing {}", icon.display()))?;

    Ok((desktop, icon))
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
    /// Differing only in comments and in the directives this command owns -
    /// `ExecStart`, the household line. Safe to overwrite: that is what a
    /// re-run after a move or an upgrade is for. A unit copied from `systemd/`
    /// by hand lands here too, of this version or an earlier one: it carries
    /// nothing a person wrote, so there is nothing to lose.
    Generated,
    /// A directive we do not own differs: a person changed a setting. Refused
    /// without `--force`, and these are the lines to show: theirs that would
    /// go, ours that would replace them. Comments are neither compared nor
    /// shown - they are not what the refusal is about.
    HandEdited {
        yours: Vec<&'a str>,
        new: Vec<&'a str>,
    },
}

/// The directives this command owns and may rewrite without asking: the two
/// substitutions, in the shape it writes them or the shape the shipped file
/// carries them. Every other directive is the person's.
///
/// The shipped `ExecStart=%h/.local/bin/x2rock daemon` is owned as that exact
/// line only: a `%h` pointing anywhere else, or quoted, is a person's edit (see
/// [`is_generated_exec`]).
fn is_owned_line(line: &str) -> bool {
    line == EXEC_MARKER
        || is_generated_exec(line)
        || is_generated_household(line)
        || line.starts_with("#Environment=X2ROCK_HOUSEHOLD=")
}

/// The lines systemd acts on: everything but blanks and comments. Unit files
/// take `#` and `;` comments, at the start of a line only.
fn directives(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|line| {
        let line = line.trim_start();
        !line.is_empty() && !line.starts_with('#') && !line.starts_with(';')
    })
}

/// The inverse of [`quoted`]: one whole double-quoted word, unescaped. `None`
/// for anything `quoted` could not have written.
fn unquoted(word: &str) -> Option<String> {
    let inner = word.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().filter(|n| matches!(n, '\\' | '"'))?),
            '%' => {
                chars.next().filter(|n| *n == '%')?;
                out.push('%');
            }
            '"' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// Whether a household line is exactly the shape [`render_unit`] writes,
/// the same rule [`is_generated_exec`] applies to `ExecStart`.
fn is_generated_household(line: &str) -> bool {
    line.strip_prefix("Environment=")
        .and_then(unquoted)
        .and_then(|a| a.strip_prefix("X2ROCK_HOUSEHOLD=").map(str::to_owned))
        .is_some_and(|value| !value.is_empty())
}

/// The household an installed unit already names, so a re-run can keep it.
///
/// Reads the generated quoted form and the unquoted one a person gets by
/// uncommenting the shipped `#Environment=X2ROCK_HOUSEHOLD=` line by hand.
pub fn existing_household(unit: &str) -> Option<String> {
    unit.lines().find_map(|line| {
        let rest = line.strip_prefix("Environment=")?;
        let assignment = if rest.starts_with('"') {
            unquoted(rest)?
        } else {
            rest.to_owned()
        };
        assignment
            .strip_prefix("X2ROCK_HOUSEHOLD=")
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

/// Whether an `ExecStart` line is exactly the shape [`render_unit`] writes: one
/// path, quoted and escaped by [`quoted`], then ` daemon` and nothing else.
///
/// Only that shape is ours. A flag added after `daemon`, an unquoted path, or a
/// bare `%` specifier such as `%h` is a person's edit, and rewriting it on a
/// re-run would silently undo them. A different path *in* the generated shape
/// is indistinguishable from a moved binary, which is what a re-run is for.
fn is_generated_exec(line: &str) -> bool {
    let Some(inner) = line
        .strip_prefix("ExecStart=\"")
        .and_then(|rest| rest.strip_suffix("\" daemon"))
    else {
        return false;
    };
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !matches!(chars.next(), Some('\\' | '"')) => return false,
            '%' if chars.next() != Some('%') => return false,
            '"' => return false,
            _ => {}
        }
    }
    !inner.is_empty()
}

/// Judge an existing file. See [`Existing`].
///
/// Directives only, in order, with the owned ones set aside: the header and
/// the shipped explanation are comments and never decide this, and neither
/// does a comment a person added - it is rewritten with the rest, which the
/// header on every generated file says will happen.
pub fn classify<'a>(existing: &'a str, proposed: &'a str) -> Existing<'a> {
    if existing == proposed {
        return Existing::Same;
    }
    let theirs: Vec<&str> = directives(existing).collect();
    let ours: Vec<&str> = directives(proposed).collect();
    let unowned = |lines: &[&'a str]| -> Vec<&'a str> {
        lines
            .iter()
            .copied()
            .filter(|l| !is_owned_line(l))
            .collect()
    };
    if unowned(&theirs) == unowned(&ours) {
        return Existing::Generated;
    }
    let (yours, new) = changed_lines(&theirs, &ours);
    Existing::HandEdited { yours, new }
}

/// The lines that differ between what is on disk and what would be written:
/// those only in the existing file, and those only in the new one.
///
/// A set difference by line, not a real diff - enough to show a person which
/// of their edits would be lost, without a dependency for it. Order is kept.
pub fn changed_lines<'a>(
    existing: &[&'a str],
    proposed: &[&'a str],
) -> (Vec<&'a str>, Vec<&'a str>) {
    let only_in = |a: &[&'a str], b: &[&'a str]| -> Vec<&'a str> {
        a.iter().copied().filter(|l| !b.contains(l)).collect()
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
        // A unit copied from systemd/ by hand has no header and the shipped
        // `%h` ExecStart, and nothing a person wrote: ours to replace. This
        // used to be refused over exactly that line.
        assert_eq!(classify(UNIT_TEMPLATE, &a), Existing::Generated);
        // The same unit as shipped before the household block existed: fewer
        // comments, no `#Environment=` line, otherwise identical. Ours - the
        // shipped prose is not the person's, and refusing over it is what a
        // real re-run hit (2026-09-15). Comments are never compared, so the
        // copy keeps that verdict with a comment of the person's added too.
        let older: String = UNIT_TEMPLATE
            .lines()
            .filter(|l| !l.starts_with("# Only for") && !l.starts_with("# a guest"))
            .filter(|l| !l.starts_with("# out which") && !l.starts_with("# `multiple"))
            .filter(|l| !l.starts_with("# names, or") && !l.starts_with("# is nearly"))
            .filter(|l| !l.starts_with("#Environment="))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(
            !older.contains("X2ROCK_HOUSEHOLD"),
            "the fixture must drop the block"
        );
        assert_eq!(classify(&older, &a), Existing::Generated);
        let annotated = older.replace("Restart=on-failure", "# mine\nRestart=on-failure");
        assert_eq!(classify(&annotated, &a), Existing::Generated);
        // But a copy with a setting changed is the person's, header or not.
        // The refusal shows every directive the rewrite would change - their
        // setting and the ExecStart swap that would come with it - and no
        // comment lines, since comments are not what it is refusing over.
        let copied_edited = older.replace("RestartSec=5", "RestartSec=30");
        match classify(&copied_edited, &a) {
            Existing::HandEdited { yours, new } => {
                assert_eq!(yours, [EXEC_MARKER, "RestartSec=30"]);
                assert_eq!(new.len(), 2, "{new:?}");
                assert!(new[0].starts_with("ExecStart=\"/home/me"), "{new:?}");
                assert_eq!(new[1], "RestartSec=5");
                assert!(yours.iter().chain(&new).all(|l| !l.starts_with('#')));
            }
            other => panic!("expected HandEdited, got {other:?}"),
        }
    }

    /// `ExecStart` is ours only in the exact shape we write. A person's flags,
    /// an unquoted path or a `%h` specifier must survive a re-run.
    #[test]
    fn a_hand_edited_exec_start_is_not_ours() {
        let a = render_unit(Path::new("/home/me/.cargo/bin/x2rock"), None).unwrap();
        let exec = a.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        for edit in [
            format!("{exec} --verbose"),
            "ExecStart=/home/me/.cargo/bin/x2rock daemon".to_owned(),
            "ExecStart=\"%h/.cargo/bin/x2rock\" daemon".to_owned(),
            "ExecStart=%h/.cargo/bin/x2rock daemon".to_owned(),
            format!("-{exec}"),
        ] {
            let edited = a.replace(exec, &edit);
            match classify(&edited, &a) {
                Existing::HandEdited { yours, new } => {
                    assert_eq!(yours, [edit.as_str()]);
                    assert_eq!(new, [exec]);
                }
                other => panic!("{edit:?}: expected HandEdited, got {other:?}"),
            }
        }
        // A path needing every escape still reads as generated, so a moved
        // binary with an awkward name goes through rather than being refused.
        let odd = render_unit(Path::new("/opt/100% \"x\\y\"/x2rock"), None).unwrap();
        assert_eq!(classify(&odd, &a), Existing::Generated);
        assert_eq!(classify(&a, &odd), Existing::Generated);
    }

    /// A household set earlier is found in either form, and survives the
    /// round trip through [`render_unit`]'s escaping.
    #[test]
    fn an_installed_household_is_read_back() {
        let exe = Path::new("/usr/bin/x2rock");
        for selector in ["Studio", "Jazz \"Room\" 50%", "back\\slash"] {
            let unit = render_unit(exe, Some(selector)).unwrap();
            assert_eq!(existing_household(&unit).as_deref(), Some(selector));
        }
        let by_hand =
            UNIT_TEMPLATE.replace(HOUSEHOLD_MARKER, "Environment=X2ROCK_HOUSEHOLD=Office");
        assert_eq!(existing_household(&by_hand).as_deref(), Some("Office"));
        assert_eq!(existing_household(UNIT_TEMPLATE), None);
        assert_eq!(existing_household(&render_unit(exe, None).unwrap()), None);

        // Ours only in the generated shape: a hand-written line is the person's.
        let ours = render_unit(exe, Some("Studio")).unwrap();
        let edited = ours.replace(
            "Environment=\"X2ROCK_HOUSEHOLD=Studio\"",
            "Environment=X2ROCK_HOUSEHOLD=Studio",
        );
        assert!(matches!(
            classify(&edited, &ours),
            Existing::HandEdited { .. }
        ));
    }

    /// What the refusal shows: the person's lines that would be lost, and ours
    /// that would replace them. Identical files show nothing.
    #[test]
    fn changed_lines_shows_each_sides_own_lines() {
        let (gone, added) = changed_lines(&["a", "keep", "b"], &["keep", "c"]);
        assert_eq!(gone, ["a", "b"]);
        assert_eq!(added, ["c"]);
        let (gone, added) = changed_lines(&["same"], &["same"]);
        assert!(gone.is_empty() && added.is_empty());
    }

    /// What counts as a directive: blanks and `#`/`;` comments are not, and
    /// leading whitespace does not hide a comment.
    #[test]
    fn directives_skip_blanks_and_comments() {
        let got: Vec<&str> =
            directives("[Unit]\n\n# c\n  ; also c\nRestart=on-failure\n  Indented=1\n").collect();
        assert_eq!(got, ["[Unit]", "Restart=on-failure", "  Indented=1"]);
    }

    #[test]
    fn embedded_desktop_files_are_valid() {
        assert!(DESKTOP_ENTRY.contains("[Desktop Entry]"));
        assert!(DESKTOP_ENTRY.contains("Icon=x2rock"));
        assert!(DESKTOP_ICON.contains("<svg"));
        let (desktop, icon) = desktop_paths().unwrap();
        assert!(desktop.ends_with("applications/x2rock.desktop"));
        assert!(icon.ends_with("icons/hicolor/scalable/apps/x2rock.svg"));
    }
}
