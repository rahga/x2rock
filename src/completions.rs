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
/// suggestions are instantaneous (<1ms) and never block on network timeouts.
pub fn complete(what: &str, out: &mut impl Write) -> Result<()> {
    match what {
        "rooms" => {
            let state = State::load().unwrap_or_default();
            let fp = netid::network_fingerprint();
            for room in state.room_names(fp.as_deref()) {
                writeln!(out, "{room}")?;
            }
        }
        "bookmarks" => {
            if let Ok(bms) = Bookmarks::load() {
                let mut names: Vec<String> = bms.items.into_iter().map(|i| i.name).collect();
                names.sort();
                names.dedup();
                for name in names {
                    writeln!(out, "{name}")?;
                }
            }
        }
        "services" => {
            let cat = Catalogue::load();
            let mut names: Vec<String> = cat.services().iter().map(|s| s.name.clone()).collect();
            names.sort();
            names.dedup();
            for name in names {
                writeln!(out, "{name}")?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn enhance_bash(script: &str) -> String {
    let room_target = r#"                --room)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -r)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;"#;

    let room_replacement = r#"                --room|-r)
                    local IFS=$'\n'
                    compopt -o filenames 2>/dev/null
                    COMPREPLY=($(compgen -W "$("${COMP_WORDS[0]}" __complete rooms 2>/dev/null)" -- "${cur}"))
                    return 0
                    ;;"#;

    let svc_target = r#"                --service)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -s)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;"#;

    let svc_replacement = r#"                --service|-s)
                    local IFS=$'\n'
                    compopt -o filenames 2>/dev/null
                    COMPREPLY=($(compgen -W "$("${COMP_WORDS[0]}" __complete services 2>/dev/null)" -- "${cur}"))
                    return 0
                    ;;"#;

    let bm_play_target = r#"        x2rock__subcmd__bookmark)
            opts="-r -i -h --next --room --all --ip --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi"#;

    let bm_play_replacement = r#"        x2rock__subcmd__bookmark)
            opts="-r -i -h --next --room --all --ip --help"
            if [[ ${cur} == -* ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            elif [[ ${COMP_CWORD} -eq 2 ]] ; then
                local IFS=$'\n'
                compopt -o filenames 2>/dev/null
                COMPREPLY=($(compgen -W "$("${COMP_WORDS[0]}" __complete bookmarks 2>/dev/null)" -- "${cur}"))
                return 0
            fi"#;

    let bm_rm_target = r#"        x2rock__subcmd__bookmarks__subcmd__remove)
            opts="-h --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi"#;

    let bm_rm_replacement = r#"        x2rock__subcmd__bookmarks__subcmd__remove)
            opts="-h --help"
            if [[ ${cur} == -* ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            elif [[ ${COMP_CWORD} -eq 3 ]] ; then
                local IFS=$'\n'
                compopt -o filenames 2>/dev/null
                COMPREPLY=($(compgen -W "$("${COMP_WORDS[0]}" __complete bookmarks 2>/dev/null)" -- "${cur}"))
                return 0
            fi"#;

    script
        .replace(room_target, room_replacement)
        .replace(svc_target, svc_replacement)
        .replace(bm_play_target, bm_play_replacement)
        .replace(bm_rm_target, bm_rm_replacement)
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

fn enhance_zsh(script: &str) -> String {
    let mut s = script.to_string();

    let helper_defs = r#"
_x2rock_rooms() {
    local -a rooms
    rooms=("${(@f)$(x2rock __complete rooms 2>/dev/null)}")
    _describe -t rooms 'room' rooms
}

_x2rock_services() {
    local -a svcs
    svcs=("${(@f)$(x2rock __complete services 2>/dev/null)}")
    _describe -t services 'service' svcs
}

_x2rock_bookmarks() {
    local -a bms
    bms=("${(@f)$(x2rock __complete bookmarks 2>/dev/null)}")
    _describe -t bookmarks 'bookmark' bms
}
"#;

    // Inject helper functions before the completion entry
    if let Some(pos) = s.find("_x2rock \"$@\"") {
        s.insert_str(pos, helper_defs);
    } else {
        s.push_str(helper_defs);
    }

    // Wire up room action in zsh _arguments
    let room_pattern = "'(-r --room)'{-r,--room}'[Room to control]:ROOM: '";
    let room_action = "'(-r --room)'{-r,--room}'[Room to control]:ROOM:_x2rock_rooms'";
    if s.contains(room_pattern) {
        s = s.replace(room_pattern, room_action);
    }

    // Wire up service action in zsh _arguments
    let svc_pattern = "'(-s --service)'{-s,--service}'[Service to search, by name. Case-insensitive, and a prefix will do.]:SERVICE: '";
    let svc_action = "'(-s --service)'{-s,--service}'[Service to search, by name. Case-insensitive, and a prefix will do.]:SERVICE:_x2rock_services'";
    if s.contains(svc_pattern) {
        s = s.replace(svc_pattern, svc_action);
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_non_empty_scripts_for_all_shells() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let mut out = Vec::new();
            generate(shell, &mut out).unwrap();
            let script = String::from_utf8(out).unwrap();
            assert!(
                !script.is_empty(),
                "completion for {shell:?} should not be empty"
            );
            assert!(
                script.contains("x2rock"),
                "completion for {shell:?} should reference binary name"
            );
        }
    }

    #[test]
    fn bash_completion_contains_dynamic_room_hook() {
        let mut out = Vec::new();
        generate(Shell::Bash, &mut out).unwrap();
        let script = String::from_utf8(out).unwrap();
        assert!(
            script.contains("__complete rooms"),
            "bash completion should hook into dynamic room completion"
        );
        assert!(
            script.contains("__complete bookmarks"),
            "bash completion should hook into dynamic bookmark completion"
        );
        assert!(
            script.contains("__complete services"),
            "bash completion should hook into dynamic service completion"
        );
    }

    #[test]
    fn fish_completion_contains_dynamic_room_hook() {
        let mut out = Vec::new();
        generate(Shell::Fish, &mut out).unwrap();
        let script = String::from_utf8(out).unwrap();
        assert!(
            script.contains("__complete rooms"),
            "fish completion should hook into dynamic room completion"
        );
        assert!(
            script.contains("__complete bookmarks"),
            "fish completion should hook into dynamic bookmark completion"
        );
        assert!(
            script.contains("__complete services"),
            "fish completion should hook into dynamic service completion"
        );
    }

    #[test]
    fn zsh_completion_contains_dynamic_room_hook() {
        let mut out = Vec::new();
        generate(Shell::Zsh, &mut out).unwrap();
        let script = String::from_utf8(out).unwrap();
        assert!(
            script.contains("_x2rock_rooms"),
            "zsh completion should hook into dynamic room completion"
        );
    }

    #[test]
    fn complete_rooms_runs_without_error() {
        let mut out = Vec::new();
        complete("rooms", &mut out).unwrap();
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
