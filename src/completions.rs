//! Shell completions generation and dynamic completion helper for x2rock.
//!
//! Generates completion scripts for Bash, Zsh, Fish, Elvish, and PowerShell
//! via `clap_complete`, enhanced with dynamic completion for `--room` / `-r`
//! (resolving remembered rooms instantaneously from local state), bookmarks,
//! and music services.

use std::io::Write;

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::Shell;

use crate::Cli;
use crate::bookmarks::Bookmarks;
use crate::catalogue::Catalogue;
use crate::netid;
use crate::state::State;

/// Output completion script for the requested shell.
pub fn generate(shell: Shell, out: &mut impl Write) -> Result<()> {
    let mut cmd = Cli::command();
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut cmd, "x2rock", &mut buf);
    let raw = String::from_utf8(buf)?;

    let enhanced = match shell {
        Shell::Bash => enhance_bash(&raw),
        Shell::Zsh => enhance_zsh(&raw),
        Shell::Fish => enhance_fish(&raw),
        _ => raw,
    };

    out.write_all(enhanced.as_bytes())?;
    Ok(())
}

/// The default user directory path where completions for `shell` should be installed.
pub fn install_path(shell: Shell) -> Result<std::path::PathBuf> {
    use anyhow::anyhow;
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("no home directory found to install completions"))?;
    let path = match shell {
        Shell::Bash => base
            .data_local_dir()
            .join("bash-completion/completions/x2rock"),
        Shell::Fish => base.config_dir().join("fish/completions/x2rock.fish"),
        Shell::Zsh => base.data_local_dir().join("zsh/site-functions/_x2rock"),
        Shell::Elvish => base.config_dir().join("elvish/lib/x2rock.elv"),
        Shell::PowerShell => base.config_dir().join("powershell/completions/x2rock.ps1"),
        _ => anyhow::bail!("unsupported shell for automatic installation"),
    };
    Ok(path)
}

/// Install completion script directly to the shell's user completion directory.
pub fn install(shell: Shell) -> Result<()> {
    use anyhow::Context;
    let path = install_path(shell)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let mut file =
        std::fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?;
    generate(shell, &mut file)?;
    println!("Installed {shell} completions to {}.", path.display());
    Ok(())
}

/// Dynamic completion helper for shell scripts.
///
/// Reads purely from local state / caches under `$XDG_STATE_HOME/x2rock/` so
/// suggestions are instantaneous and never block on network timeouts. With a
/// `prefix`, only the names starting with it (case-insensitively) are printed,
/// so the shell has nothing left to filter - see [`matching`] for why.
pub fn complete(what: &str, prefix: Option<&str>, out: &mut impl Write) -> Result<()> {
    let names: Vec<String> = match what {
        "rooms" => {
            let state = State::load().unwrap_or_default();
            let fp = netid::network_fingerprint();
            state.room_names(fp.as_deref())
        }
        "bookmarks" => Bookmarks::load()
            .map(|bms| bms.items.into_iter().map(|i| i.name).collect())
            .unwrap_or_default(),
        "services" => Catalogue::load()
            .services()
            .iter()
            .map(|s| s.name.clone())
            .collect(),
        _ => Vec::new(),
    };
    for name in matching(names, prefix) {
        writeln!(out, "{name}")?;
    }
    Ok(())
}

/// The names that start with `prefix`, sorted and deduplicated. Case-insensitive,
/// since a room is typed as "media" as often as "Media".
///
/// Done here, not by the shell. bash's `compgen -W` splits its word list and
/// then *re-parses each word with shell quoting*, and a list holding
/// `Intervallo (from Veruschka) (II)` lost every entry after it for any
/// non-empty prefix - "Me" found none of Medicine, Melana, Meri while "Bo"
/// worked. `mapfile` reads the lines this prints as they are.
fn matching(mut names: Vec<String>, prefix: Option<&str>) -> Vec<String> {
    let prefix = prefix.unwrap_or_default().to_lowercase();
    names.retain(|n| n.to_lowercase().starts_with(&prefix));
    names.sort();
    names.dedup();
    names
}

/// The dynamic hooks for bash, spliced into the script clap_complete wrote.
///
/// Anchored on the script's *structure* - a `case "${prev}"` arm for the flag,
/// the `if [[ ${cur} == -* || ${COMP_CWORD} -eq N ]]` line of a subcommand's
/// block - and never on the text inside it. The first version matched whole
/// blocks verbatim, `opts="-h --help"` included, and stopped matching the
/// moment the global flags propagated into `bookmarks remove`; the hook
/// silently went missing and the test, which only looked for the hook string
/// somewhere in the script, stayed green. Everything here is checked by what
/// it *changed*, in the tests below.
fn enhance_bash(script: &str) -> String {
    let lines: Vec<&str> = script.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 64);
    // Which subcommand block the cursor is in, by clap's case label.
    let mut block: Option<&str> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if let Some(label) = trimmed
            .strip_suffix(')')
            .filter(|l| l.starts_with("x2rock__"))
        {
            block = Some(label);
        }

        // `--room)` / `-r)` and `--service)` / `-s)` arms: the value that
        // follows is a room or a service, not a file.
        let list = match trimmed {
            "--room)" | "-r)" => Some("rooms"),
            "--service)" | "-s)" => Some("services"),
            _ => None,
        };
        if let Some(list) = list
            && lines.get(i + 1).map(|l| l.trim()) == Some(r#"COMPREPLY=($(compgen -f "${cur}"))"#)
        {
            out.push(line.to_string());
            out.extend(dynamic_reply(list, indent(lines[i + 1])));
            i += 2;
            continue;
        }

        // The positional of `bookmark` and of `bookmarks remove` is a bookmark
        // name. clap offers the flags there; split its `if` so a `-` prefix
        // still gets the flags and anything else gets the names.
        let positional = match block {
            Some("x2rock__subcmd__bookmark") => Some(2),
            Some("x2rock__subcmd__bookmarks__subcmd__remove") => Some(3),
            _ => None,
        };
        if let Some(n) = positional
            && trimmed == format!(r#"if [[ ${{cur}} == -* || ${{COMP_CWORD}} -eq {n} ]] ; then"#)
            && lines.get(i + 3).map(|l| l.trim()) == Some("fi")
        {
            let pad = indent(line);
            let inner = indent(lines[i + 1]);
            out.push(format!(r#"{pad}if [[ ${{cur}} == -* ]] ; then"#));
            out.push(lines[i + 1].to_string());
            out.push(lines[i + 2].to_string());
            out.push(format!(r#"{pad}elif [[ ${{COMP_CWORD}} -eq {n} ]] ; then"#));
            out.extend(dynamic_reply("bookmarks", inner));
            out.push(format!("{inner}return 0"));
            out.push(lines[i + 3].to_string());
            i += 4;
            continue;
        }

        out.push(line.to_string());
        i += 1;
    }
    let mut joined = out.join("\n");
    if script.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// The leading whitespace of a line.
fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// The bash lines that fill `COMPREPLY` from `x2rock __complete <list> <cur>`.
/// `mapfile` takes each line as one entry, so "Media Room" stays one word and
/// a `(` or `*` in a name is never re-parsed or globbed - which `compgen -W`
/// and an unquoted `$(...)` both do; `-o filenames` makes bash quote the entry
/// on insertion. The binary has already filtered by `cur`.
fn dynamic_reply(list: &str, pad: &str) -> [String; 2] {
    [
        format!("{pad}compopt -o filenames 2>/dev/null"),
        format!(
            r#"{pad}mapfile -t COMPREPLY < <("${{COMP_WORDS[0]}}" __complete {list} "${{cur}}" 2>/dev/null)"#
        ),
    ]
}

fn enhance_fish(script: &str) -> String {
    let mut s = script.to_string();
    s.push_str("\n# Dynamic completions for rooms, services, and bookmarks\n");
    s.push_str("complete -c x2rock -s r -l room -x -a '(x2rock __complete rooms 2>/dev/null)'\n");
    s.push_str(
        "complete -c x2rock -s s -l service -x -a '(x2rock __complete services 2>/dev/null)'\n",
    );
    s.push_str("complete -c x2rock -n '__fish_seen_subcommand_from bookmark' -x -a '(x2rock __complete bookmarks 2>/dev/null)'\n");
    s.push_str("complete -c x2rock -n '__fish_seen_subcommand_from bookmarks; and __fish_seen_subcommand_from remove' -x -a '(x2rock __complete bookmarks 2>/dev/null)'\n");
    s
}

/// The dynamic hooks for zsh. clap_complete writes every value spec as
/// `:NAME:_default'`, so the action is swapped by its suffix - the one part of
/// the line that does not move when a flag's help text is edited. The first
/// version rewrote a line shape (`'(-r --room)'{-r,--room}'[Room to control]…`)
/// that clap_complete has never emitted, so its helpers were defined and never
/// called, and the test that looked for the definition stayed green.
fn enhance_zsh(script: &str) -> String {
    // `compadd`, not `_describe`: the latter reads `name:description` pairs,
    // so "Book One: Dune" would offer "Book One" described as "Dune".
    let helper_defs = r#"
_x2rock_list() {
    local -a items
    items=("${(@f)$(x2rock __complete "$1" 2>/dev/null)}")
    compadd -- "${items[@]}"
}
_x2rock_rooms() { _x2rock_list rooms }
_x2rock_services() { _x2rock_list services }
_x2rock_bookmarks() { _x2rock_list bookmarks }
"#;
    let mut out: Vec<String> = Vec::new();
    // The nearest `(name)` case label above, indented or not: the top-level
    // `(bookmark)` and the `(remove)` nested under `bookmarks` both carry a
    // `':query:_default'` positional that is a bookmark name. No other
    // `(remove)` has a `query` positional - alarm's has none, queue's is a
    // `range` - so the label and the positional together are the key.
    let mut label: Option<&str> = None;
    for line in script.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix('(').and_then(|l| l.strip_suffix(')'))
            && !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            label = Some(name);
        }
        let mut line = line
            .replace(":ROOM:_default'", ":ROOM:_x2rock_rooms'")
            .replace(":SERVICE:_default'", ":SERVICE:_x2rock_services'");
        if matches!(label, Some("bookmark" | "remove")) && trimmed == r"':query:_default' \" {
            line = line.replace(":query:_default'", ":query:_x2rock_bookmarks'");
        }
        out.push(line);
    }
    let mut s = out.join("\n");
    if script.ends_with('\n') {
        s.push('\n');
    }
    // The helpers go before the entry point, where zsh's autoload has already
    // read them by the time `_x2rock` runs.
    match s.find("_x2rock \"$@\"") {
        Some(pos) => s.insert_str(pos, helper_defs),
        None => s.push_str(helper_defs),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(shell: Shell) -> String {
        let mut out = Vec::new();
        generate(shell, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn generate_produces_non_empty_scripts_for_all_shells() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let s = script(shell);
            assert!(
                !s.is_empty(),
                "completion for {shell:?} should not be empty"
            );
            assert!(
                s.contains("x2rock"),
                "completion for {shell:?} should name the binary"
            );
        }
    }

    /// Checked by effect: no room or service arm still completes filenames, and
    /// both bookmark positionals got their hook. Counting the hook strings alone
    /// is what let `bookmarks remove` lose its hook unnoticed.
    #[test]
    fn bash_rewires_every_room_and_service_arm_and_both_bookmark_positionals() {
        let s = script(Shell::Bash);
        let lines: Vec<&str> = s.lines().collect();
        let mut room_arms = 0;
        let mut service_arms = 0;
        for (i, line) in lines.iter().enumerate() {
            let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
            match line.trim() {
                "--room)" | "-r)" => {
                    room_arms += 1;
                    assert!(next.starts_with("compopt"), "unhooked room arm at line {i}");
                }
                "--service)" | "-s)" => {
                    service_arms += 1;
                    assert!(
                        next.starts_with("compopt"),
                        "unhooked service arm at line {i}"
                    );
                }
                _ => {}
            }
            assert_ne!(
                (line.trim(), next),
                ("--room)", r#"COMPREPLY=($(compgen -f "${cur}"))"#),
                "a room arm still completes filenames"
            );
        }
        assert!(
            room_arms > 10,
            "every subcommand has a room arm; saw {room_arms}"
        );
        assert!(
            service_arms >= 4,
            "search, browse, play-item, queue-item; saw {service_arms}"
        );
        assert_eq!(
            s.matches("__complete bookmarks").count(),
            2,
            "the `bookmark` positional and the `bookmarks remove` positional"
        );
        // And specifically the block that used to be missed.
        let remove = s
            .find("x2rock__subcmd__bookmarks__subcmd__remove)")
            .expect("a bookmarks remove block");
        let block_end = s[remove..].find(";;").map(|e| remove + e).unwrap();
        assert!(
            s[remove..block_end].contains("__complete bookmarks"),
            "bookmarks remove completes bookmark names"
        );
        assert!(s.contains("__complete rooms") && s.contains("__complete services"));
    }

    #[test]
    fn fish_appends_the_three_dynamic_hooks() {
        let s = script(Shell::Fish);
        for list in ["rooms", "services", "bookmarks"] {
            assert!(
                s.contains(&format!("__complete {list}")),
                "fish lacks the {list} hook"
            );
        }
    }

    /// Checked by effect: the helpers are *called*, not merely defined. The
    /// first version's helpers were dead code, and a test that looked for
    /// their names could not tell.
    #[test]
    fn zsh_swaps_every_room_and_service_action_and_the_bookmark_positionals() {
        let s = script(Shell::Zsh);
        assert_eq!(
            s.matches(":ROOM:_default'").count(),
            0,
            "a room value still uses _default"
        );
        assert_eq!(s.matches(":SERVICE:_default'").count(), 0);
        assert!(s.matches(":ROOM:_x2rock_rooms'").count() > 10);
        assert!(s.matches(":SERVICE:_x2rock_services'").count() >= 4);
        assert_eq!(
            s.matches(":query:_x2rock_bookmarks'").count(),
            2,
            "`bookmark` and `bookmarks remove`"
        );
        // Other `query` positionals - favorite, search, the bookmarks list - are
        // not bookmark names and keep _default.
        assert!(s.matches(":query:_default'").count() >= 3);
        // Defined once each, before the entry point that autoload reads first.
        for helper in [
            "_x2rock_rooms()",
            "_x2rock_services()",
            "_x2rock_bookmarks()",
        ] {
            assert_eq!(s.matches(helper).count(), 1, "{helper}");
            assert!(s.find(helper).unwrap() < s.find("_x2rock \"$@\"").unwrap());
        }
        // The helpers add names with compadd: `_describe` reads `name:description`
        // pairs and would split "Book One: Dune". (clap's own subcommand lists
        // use _describe, rightly - those have descriptions.)
        assert!(s.contains(r#"compadd -- "${items[@]}""#));
        let helpers = &s[s.find("_x2rock_list()").unwrap()..s.find("_x2rock \"$@\"").unwrap()];
        assert!(
            !helpers.contains("_describe"),
            "a ':' in a name is _describe's separator"
        );
    }

    #[test]
    fn the_shell_gets_names_already_filtered_case_insensitively() {
        let names = || {
            vec![
                "Intervallo (from Veruschka) (II)".to_string(),
                "Medicine Cabinet".to_string(),
                "Melana".to_string(),
                "Book One: Dune".to_string(),
                "Melana".to_string(),
            ]
        };
        // The case that compgen -W got wrong: a prefix past the parenthesised
        // entry, and typed in lower case.
        assert_eq!(
            matching(names(), Some("me")),
            ["Medicine Cabinet", "Melana"]
        );
        assert_eq!(matching(names(), Some("Book")), ["Book One: Dune"]);
        assert_eq!(matching(names(), Some("zzz")), Vec::<String>::new());
        // No prefix is everything, sorted, once.
        assert_eq!(matching(names(), None).len(), 4);
        // The bash hook reads lines with mapfile, never through compgen -W.
        let bash = script(Shell::Bash);
        assert!(
            !bash.contains("compgen -W \"$("),
            "a __complete list went through compgen -W"
        );
        assert!(
            bash.contains(
                r#"mapfile -t COMPREPLY < <("${COMP_WORDS[0]}" __complete rooms "${cur}""#
            )
        );
    }

    #[test]
    fn complete_rooms_runs_without_error() {
        let mut out = Vec::new();
        complete("rooms", None, &mut out).unwrap();
        // Just verify it doesn't crash or error on this machine:
        let _ = String::from_utf8(out).unwrap();
    }

    #[test]
    fn install_path_resolves_for_all_supported_shells() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let path = install_path(shell).unwrap();
            assert!(
                path.to_string_lossy().contains("x2rock"),
                "install path for {shell} should name x2rock"
            );
        }
    }
}
