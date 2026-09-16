//! The commands that install x2rock into the machine rather than drive a
//! speaker: the agent skill, the systemd unit and its status, the desktop
//! entry, and shell completions. None of them needs a player on the network.
//! The pure decisions - what a unit should say, whether a file on disk is ours
//! to overwrite - live in `service.rs` and `completions.rs`; this is the glue
//! that reads the flags, writes the files and talks to `systemctl`.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::json;

use crate::cli::{AgentTarget, DesktopAction, ServiceAction};
use crate::{completions, service, store};

/// The agent skill, embedded so it ships with the binary and cannot drift from
/// the CLI it documents. Written to disk, or printed, by `x2rock skill`.
pub const SKILL: &str = include_str!("../../skills/x2rock/SKILL.md");

/// Resolve skill directories for the target assistant(s).
/// Defaults to auto-detecting existing assistant directories (Claude, Antigravity / Gemini)
/// or falling back to Claude for backwards compatibility.
fn agent_skills_dirs(agent: Option<AgentTarget>) -> Result<Vec<PathBuf>> {
    let home = directories::BaseDirs::new()
        .ok_or_else(|| {
            anyhow!("no home directory to find assistant skill directories in; pass --dir")
        })?
        .home_dir()
        .to_path_buf();

    let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(|d| PathBuf::from(d).join("skills"))
        .unwrap_or_else(|| home.join(".claude").join("skills"));

    let antigravity_dir = std::env::var_os("ANTIGRAVITY_CONFIG_DIR")
        .map(|d| PathBuf::from(d).join("skills"))
        .unwrap_or_else(|| home.join(".gemini").join("antigravity-cli").join("skills"));

    match agent {
        Some(AgentTarget::Claude) => Ok(vec![claude_dir]),
        Some(AgentTarget::Antigravity | AgentTarget::Gemini) => Ok(vec![antigravity_dir]),
        Some(AgentTarget::All) => Ok(vec![claude_dir, antigravity_dir]),
        None => {
            let mut detected = Vec::new();
            if home.join(".claude").exists() || std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
                detected.push(claude_dir.clone());
            }
            if home.join(".gemini").join("antigravity-cli").exists()
                || std::env::var_os("ANTIGRAVITY_CONFIG_DIR").is_some()
            {
                detected.push(antigravity_dir);
            }
            if detected.is_empty() {
                detected.push(claude_dir);
            }
            Ok(detected)
        }
    }
}

/// `x2rock skill`: drop the embedded skill into assistant skill directories (or
/// remove or print it). Needs no network - it is a local file operation.
pub fn handle_skill(
    agent: Option<AgentTarget>,
    dir: Option<&std::path::Path>,
    print: bool,
    remove: bool,
) -> Result<()> {
    if print {
        print!("{SKILL}");
        return Ok(());
    }
    let targets = match dir {
        Some(d) => vec![d.to_path_buf()],
        None => agent_skills_dirs(agent)?,
    };
    if remove {
        for base in &targets {
            let target = base.join("x2rock");
            let path = target.join("SKILL.md");
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                println!("Removed {}.", path.display());
                let _ = std::fs::remove_dir(&target);
            } else {
                println!("No skill found at {}.", path.display());
            }
        }
        return Ok(());
    }
    for base in &targets {
        let target = base.join("x2rock");
        std::fs::create_dir_all(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        let path = target.join("SKILL.md");
        std::fs::write(&path, SKILL).with_context(|| format!("writing {}", path.display()))?;
        println!("Wrote the x2rock skill to {}.", path.display());
    }
    println!("An AI assistant on this machine will pick it up for Sonos tasks.");
    Ok(())
}

/// Where the user unit goes: `$XDG_CONFIG_HOME/systemd/user`, which is where
/// `systemctl --user` looks and where the README told people to copy it.
fn user_unit_dir() -> Result<PathBuf> {
    service::user_unit_dir()
}

/// Auto-detect the current shell from $SHELL environment variable.
fn detect_shell() -> Option<clap_complete::Shell> {
    let shell_path = std::env::var("SHELL").ok()?;
    let name = std::path::Path::new(&shell_path).file_name()?.to_str()?;
    match name {
        "bash" => Some(clap_complete::Shell::Bash),
        "zsh" => Some(clap_complete::Shell::Zsh),
        "fish" => Some(clap_complete::Shell::Fish),
        "elvish" => Some(clap_complete::Shell::Elvish),
        "powershell" | "pwsh" => Some(clap_complete::Shell::PowerShell),
        _ => None,
    }
}

/// Whether the running daemon is on a binary other than `exe`: replaced in
/// place, or started from somewhere else. `false` whenever it cannot tell, so
/// an unreadable `/proc` never forces a restart.
fn daemon_runs_stale_binary(exe: &std::path::Path) -> bool {
    let Ok(out) = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "x2rock.service",
            "-p",
            "MainPID",
            "--value",
        ])
        .output()
    else {
        return false;
    };
    let pid: u32 = match String::from_utf8_lossy(&out.stdout).trim().parse() {
        Ok(pid) if pid > 0 => pid,
        _ => return false,
    };
    let Ok(running) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let installed = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    service::runs_stale_binary(&running, &installed)
}

/// Write one generated file. Three cases, judged by [`service::classify`]:
/// identical (say so, do nothing); differing only in comments and the lines
/// this command owns (overwrite - a re-run after a move or an upgrade is
/// exactly this, and so is replacing a unit copied from `systemd/` by hand);
/// a setting a person changed (refuse without `--force`, showing their lines
/// and the replacements).
///
/// Returns whether it wrote, because the caller's next move depends on it: a
/// running daemon is on a stale unit only if the unit actually changed.
fn place_generated(path: &std::path::Path, text: &str, force: bool) -> Result<bool> {
    if let Some(existing) = store::read_optional(path)? {
        match service::classify(&existing, text) {
            service::Existing::Same => {
                println!("{} is already current; left as is.", path.display());
                return Ok(false);
            }
            service::Existing::Generated => {
                store::write_atomically(path, text, store::PLAIN)?;
                println!("Updated {}.", path.display());
                return Ok(true);
            }
            service::Existing::HandEdited { yours, new } if !force => {
                let mut msg = format!(
                    "{} exists with settings edited by hand, so it is not overwritten.\n",
                    path.display()
                );
                for line in &yours {
                    msg.push_str(&format!("  yours:  {line}\n"));
                }
                for line in &new {
                    msg.push_str(&format!("  new:    {line}\n"));
                }
                msg.push_str(
                    "Re-run with --force to overwrite it, or --print to see the whole unit.",
                );
                bail!("{msg}");
            }
            service::Existing::HandEdited { .. } => {}
        }
    }
    store::write_atomically(path, text, store::PLAIN)?;
    println!("Wrote {}.", path.display());
    Ok(true)
}

/// `x2rock service install`: the unit for *this* binary, written where
/// `systemctl --user` reads it. See `service.rs` for why not a fixed path.
fn install_service(
    household: Option<&str>,
    no_household: bool,
    headless: bool,
    enable: bool,
    force: bool,
    print: bool,
) -> Result<()> {
    // The path as invoked, not as resolved: under Nix, Homebrew or stow the
    // stable name on PATH is a symlink into a versioned directory that the
    // next upgrade removes, so resolving it would pin the unit to the thing
    // that breaks. `current_exe()` is the fallback only because it always
    // answers; on Linux it has already resolved the link. See `invoked_path`.
    let exe = std::env::args().next().and_then(|argv0| {
        let cwd = std::env::current_dir().ok()?;
        let path = std::env::var("PATH").ok();
        service::invoked_path(&argv0, &cwd, path.as_deref())
    });
    let exe = match exe {
        Some(p) => p,
        None => std::env::current_exe().context("finding this binary's own path")?,
    };
    // A re-run after a move or an upgrade usually names no household, and on a
    // network with several Sonos systems dropping the one set earlier leaves
    // the daemon asking which household forever. So an existing one is kept
    // unless `--household` replaces it or `--no-household` drops it.
    let dir = user_unit_dir()?;
    let household = match (household, no_household) {
        (Some(_), true) => bail!("--household and --no-household contradict each other"),
        (Some(given), false) => Some(given.to_owned()),
        (None, true) => None,
        (None, false) => {
            let kept = store::read_optional(&dir.join("x2rock.service"))?
                .as_deref()
                .and_then(service::existing_household);
            if let Some(kept) = &kept {
                eprintln!(
                    "Keeping X2ROCK_HOUSEHOLD={kept} from the installed unit; --household \
                     changes it and --no-household drops it."
                );
            }
            kept
        }
    };
    let unit = service::render_unit(&exe, household.as_deref())?;
    if print {
        print!("{unit}");
        if headless {
            print!(
                "\n# --- x2rock.service.d/headless.conf ---\n{}",
                service::render_headless()
            );
        }
        return Ok(());
    }

    // Read before writing: `enable --now` does nothing to a unit that is
    // already active, so a daemon that was running keeps running the *old*
    // binary unless it is restarted - and "Enabled and started" would then be
    // a lie about which build is up.
    let was_active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "x2rock.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    // An upgrade in place leaves the unit identical, so "changed" alone misses
    // the commonest reinstall; ask the running process what it is running.
    let stale = was_active && daemon_runs_stale_binary(&exe);

    let mut changed = place_generated(&dir.join("x2rock.service"), &unit, force)?;
    let dropin = dir.join("x2rock.service.d").join("headless.conf");
    if headless {
        changed |= place_generated(&dropin, &service::render_headless(), force)?;
    } else if dropin.exists() {
        println!(
            "Note: {} is present from an earlier --headless install and was left in place.",
            dropin.display()
        );
    }
    if !headless {
        match service::place_desktop_files(force) {
            Ok(placed) => {
                if placed.desktop_written {
                    println!(
                        "Installed desktop entry to {}.",
                        placed.desktop_path.display()
                    );
                } else if placed.desktop_edited && !force {
                    println!(
                        "Note: {} has been edited and was left in place; use --force to overwrite.",
                        placed.desktop_path.display()
                    );
                }
                if placed.icon_written {
                    println!("Installed icon to {}.", placed.icon_path.display());
                } else if placed.icon_edited && !force {
                    println!(
                        "Note: {} has been edited and was left in place; use --force to overwrite.",
                        placed.icon_path.display()
                    );
                }
            }
            Err(e) => eprintln!("Note: could not install desktop files: {e}"),
        }
    }

    // A reload is what makes systemd read the new file; failing here is worth
    // saying but not worth failing over, since the file is written and a later
    // `daemon-reload` fixes it.
    let reload = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    match reload {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("x2rock: `systemctl --user daemon-reload` exited {status}; run it yourself")
        }
        Err(e) => eprintln!(
            "x2rock: could not run systemctl ({e}); run `systemctl --user daemon-reload` yourself"
        ),
    }

    if enable {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "x2rock.service"])
            .status()
            .context("running systemctl --user enable --now x2rock.service")?;
        ensure!(
            status.success(),
            "`systemctl --user enable --now x2rock.service` exited {status}"
        );
        // A restart only when a running daemon is on an old unit or an old
        // binary; `enable --now` already started one that was not running.
        if was_active && (changed || stale) {
            let status = std::process::Command::new("systemctl")
                .args(["--user", "restart", "x2rock.service"])
                .status()
                .context("running systemctl --user restart x2rock.service")?;
            ensure!(
                status.success(),
                "`systemctl --user restart x2rock.service` exited {status}"
            );
            println!(
                "Restarted x2rock.service so it runs this binary. `journalctl --user -u x2rock` names the binary it came up on."
            );
        } else {
            println!(
                "Enabled and started x2rock.service. `journalctl --user -u x2rock` shows what it is doing."
            );
        }
    } else if was_active && (changed || stale) {
        println!(
            "x2rock.service is still running an older unit or binary; \
             `systemctl --user restart x2rock.service` switches it to this one."
        );
    } else if !was_active {
        println!("Next: systemctl --user enable --now x2rock.service");
    }
    if headless {
        println!(
            "Headless: also run `loginctl enable-linger {}` so the daemon outlives your ssh session.",
            std::env::var("USER").unwrap_or_else(|_| "$USER".into())
        );
    }
    println!(
        "Running from {}. Re-run `x2rock service install` if the binary moves.",
        exe.display()
    );
    Ok(())
}

#[derive(serde::Serialize)]
struct ServiceStatusJson {
    installed: bool,
    unit_path: String,
    active: bool,
    enabled: bool,
    pid: Option<u32>,
    stale: bool,
    exec: Option<String>,
    household: Option<String>,
    headless_installed: bool,
    desktop_installed: bool,
}

fn status_service(json: bool) -> Result<()> {
    let dir = service::user_unit_dir()?;
    let unit_path = dir.join("x2rock.service");
    let installed = unit_path.exists();
    let unit_content = if installed {
        store::read_optional(&unit_path)?
    } else {
        None
    };

    let exec = unit_content.as_deref().and_then(service::existing_exec);
    let household = unit_content
        .as_deref()
        .and_then(service::existing_household);

    let dropin_path = dir.join("x2rock.service.d").join("headless.conf");
    let headless_installed = dropin_path.exists();

    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "x2rock.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let enabled = std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "x2rock.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let pid = if active {
        std::process::Command::new("systemctl")
            .args([
                "--user",
                "show",
                "x2rock.service",
                "-p",
                "MainPID",
                "--value",
            ])
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            })
            .filter(|&p| p > 0)
    } else {
        None
    };

    let (desktop_file_exists, icon_exists) = service::desktop_installed();
    let desktop_installed = desktop_file_exists && icon_exists;

    // Against the unit's own `ExecStart`, not against whichever binary is
    // answering this command. `service status` run from a build tree asks about
    // the installed daemon, and "stale" because those two are different
    // binaries is true of nothing anyone wanted to know.
    let stale = active
        && exec
            .as_deref()
            .map(std::path::Path::new)
            .map(daemon_runs_stale_binary)
            .unwrap_or(false);

    if json {
        let status = ServiceStatusJson {
            installed,
            unit_path: unit_path.display().to_string(),
            active,
            enabled,
            pid,
            stale,
            exec,
            household,
            headless_installed,
            desktop_installed,
        };
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }

    println!(
        "Service unit:   {}",
        if installed {
            format!("installed ({})", unit_path.display())
        } else {
            format!("not installed ({})", unit_path.display())
        }
    );

    if let Some(e) = &exec {
        let exists = std::path::Path::new(e).exists();
        println!(
            "ExecStart:      {}{}",
            e,
            if exists {
                ""
            } else {
                " (binary not found on disk!)"
            }
        );
    }
    if let Some(h) = &household {
        println!("Household:      {h}");
    }
    if headless_installed {
        println!("Drop-in:        installed ({})", dropin_path.display());
    }
    println!(
        "Systemd state:  {}",
        if active {
            if let Some(p) = pid {
                format!("active (running, PID {p})")
            } else {
                "active".to_string()
            }
        } else {
            "inactive".to_string()
        }
    );
    println!("Unit enabled:   {}", if enabled { "yes" } else { "no" });
    if stale {
        println!(
            "Note:           running daemon process is on a stale binary; run `systemctl --user restart x2rock.service`"
        );
    }
    println!(
        "Desktop files:  {}",
        if desktop_installed {
            "installed"
        } else if desktop_file_exists || icon_exists {
            "partially installed"
        } else {
            "not installed"
        }
    );

    Ok(())
}

fn uninstall_service(desktop: bool) -> Result<()> {
    let dir = service::user_unit_dir()?;
    let unit_path = dir.join("x2rock.service");
    let dropin = dir.join("x2rock.service.d").join("headless.conf");
    let dropin_dir = dir.join("x2rock.service.d");

    // Read before the disable, which does not change it, so that a failure can
    // be told from "there was nothing to disable".
    let unit_existed = unit_path.exists();
    let disabled = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "x2rock.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    // A disable that failed while there *was* a unit to disable is worth
    // saying: the file is removed either way, so a daemon still running from it
    // would otherwise be a surprise with nothing left on disk to explain it.
    if unit_existed && !disabled {
        eprintln!(
            "x2rock: `systemctl --user disable --now x2rock.service` did not succeed; \
             if the daemon is still running, stop it with \
             `systemctl --user stop x2rock.service`."
        );
    }

    let mut removed_something = false;
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
        println!("Removed {}.", unit_path.display());
        removed_something = true;
    } else {
        println!("Service unit was not installed at {}.", unit_path.display());
    }

    if dropin.exists() {
        std::fs::remove_file(&dropin).with_context(|| format!("removing {}", dropin.display()))?;
        println!("Removed {}.", dropin.display());
        removed_something = true;
    }

    if dropin_dir.exists() {
        let remaining: Vec<_> = std::fs::read_dir(&dropin_dir)
            .ok()
            .into_iter()
            .flat_map(|entries| {
                entries.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            })
            .collect();
        if remaining.is_empty() {
            let _ = std::fs::remove_dir(&dropin_dir);
        } else {
            println!(
                "Note: custom drop-in(s) left in {}: {}",
                dropin_dir.display(),
                remaining.join(", ")
            );
        }
    }

    if removed_something {
        let reloaded = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if disabled && reloaded {
            println!("Disabled x2rock.service and reloaded systemd user daemon.");
        } else if disabled {
            println!(
                "Disabled x2rock.service (reload systemd user daemon with `systemctl --user daemon-reload`)."
            );
        } else if reloaded {
            println!("Reloaded systemd user daemon.");
        }
    } else if disabled {
        println!("Disabled x2rock.service.");
    }

    if desktop {
        let (d_removed, i_removed) = service::uninstall_desktop_files()?;
        if d_removed || i_removed {
            println!("Removed desktop entry and icon.");
        } else {
            println!("Desktop entry and icon were not installed.");
        }
    } else {
        let (d_exists, i_exists) = service::desktop_installed();
        if d_exists || i_exists {
            println!(
                "Note: desktop entry and icon were left in place; pass --desktop to remove them."
            );
        }
    }

    Ok(())
}

/// `x2rock desktop`: install, remove or report the desktop entry and icon.
/// Local files only; the systemd half is `install_service`.
pub fn desktop(action: Option<DesktopAction>) -> Result<()> {
    match action {
        None | Some(DesktopAction::Install { .. }) => {
            let force = match action {
                Some(DesktopAction::Install { force }) => force,
                _ => false,
            };
            let placed = service::place_desktop_files(force)?;
            if placed.desktop_written {
                println!(
                    "Installed desktop entry to {}.",
                    placed.desktop_path.display()
                );
            } else if placed.desktop_edited && !force {
                println!(
                    "Note: {} has been edited and was left in place; use --force to overwrite.",
                    placed.desktop_path.display()
                );
            }
            if placed.icon_written {
                println!("Installed icon to {}.", placed.icon_path.display());
            } else if placed.icon_edited && !force {
                println!(
                    "Note: {} has been edited and was left in place; use --force to overwrite.",
                    placed.icon_path.display()
                );
            }
            if !placed.desktop_written
                && !placed.icon_written
                && !placed.desktop_edited
                && !placed.icon_edited
            {
                println!("Desktop entry and icon are already up to date.");
            }
        }
        Some(DesktopAction::Uninstall) => {
            let (desktop, icon) = service::uninstall_desktop_files()?;
            if desktop || icon {
                println!("Removed desktop entry and icon.");
            } else {
                println!("Desktop entry and icon were not installed.");
            }
        }
        Some(DesktopAction::Status { json }) => {
            let (desktop_ok, icon_ok) = service::desktop_installed();
            let (desktop, icon) = service::desktop_paths()?;
            if json {
                println!(
                    "{}",
                    json!({
                        "desktop_installed": desktop_ok,
                        "desktop_path": desktop,
                        "icon_installed": icon_ok,
                        "icon_path": icon,
                    })
                );
            } else {
                println!(
                    "Desktop entry: {} ({})",
                    if desktop_ok { "installed" } else { "missing" },
                    desktop.display()
                );
                println!(
                    "Desktop icon:  {} ({})",
                    if icon_ok { "installed" } else { "missing" },
                    icon.display()
                );
            }
        }
    }
    Ok(())
}

/// Whether `--json` means anything for a `service` action.
///
/// The flag is global on `service` so that `x2rock service --json` works
/// without naming `status`, which is what a bare `x2rock service` runs. The
/// other two actions have no JSON form, and a flag that parses and then does
/// nothing is how a caller comes to trust output it never got - so they refuse
/// it rather than ignore it.
fn json_applies(action: &Option<ServiceAction>) -> bool {
    matches!(action, None | Some(ServiceAction::Status))
}

/// `x2rock service`: the daemon's systemd unit - install, status (the
/// default), uninstall. `household` is the global `--household`, kept in
/// the unit so a daemon on a shared network knows which system is meant.
pub fn service(action: Option<ServiceAction>, json: bool, household: Option<&str>) -> Result<()> {
    ensure!(
        !json || json_applies(&action),
        "--json applies to `service status`, which is what `x2rock service` runs on its own; \
         install and uninstall have no JSON form"
    );
    match action {
        Some(ServiceAction::Install {
            headless,
            enable,
            force,
            print,
            no_household,
        }) => install_service(household, no_household, headless, enable, force, print),
        Some(ServiceAction::Status) | None => status_service(json),
        Some(ServiceAction::Uninstall { desktop }) => uninstall_service(desktop),
    }
}

/// `x2rock completions`: print, install or remove the completion script for
/// `shell`, or for `$SHELL` when none is named.
pub fn completions(
    shell: Option<clap_complete::Shell>,
    install: bool,
    uninstall: bool,
) -> Result<()> {
    let target_shell = match shell.or_else(detect_shell) {
        Some(s) => s,
        None => bail!(
            "could not determine shell from $SHELL; specify one of bash, zsh, fish, elvish, powershell"
        ),
    };
    if uninstall {
        completions::uninstall(target_shell)?;
        Ok(())
    } else if install {
        completions::install(target_shell)
    } else {
        completions::generate(target_shell, &mut std::io::stdout())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--json` is global on `service` for the sake of a bare `x2rock service
    /// --json`, so the rule about where it actually means something lives here
    /// rather than in clap, and is held to it.
    #[test]
    fn json_belongs_to_status_and_is_refused_elsewhere() {
        assert!(json_applies(&None), "a bare `service` is status");
        assert!(json_applies(&Some(ServiceAction::Status)));
        assert!(!json_applies(&Some(ServiceAction::Uninstall {
            desktop: false
        })));
        assert!(!json_applies(&Some(ServiceAction::Install {
            headless: false,
            enable: false,
            force: false,
            print: false,
            no_household: false,
        })));
    }

    #[test]
    fn the_embedded_skill_carries_its_frontmatter_and_contracts() {
        // include_str! guarantees the file exists at build time; this guards its
        // shape - the frontmatter a skill needs, and the two contracts the skill
        // exists to teach, so an edit cannot quietly drop them.
        assert!(
            SKILL.starts_with("---\nname: x2rock\n"),
            "needs skill frontmatter"
        );
        assert!(
            SKILL.contains("description:"),
            "needs a description to be discovered"
        );
        assert!(
            SKILL.contains("x2rock status --json"),
            "should teach the status snapshot"
        );
        assert!(
            SKILL.contains("unregistered_network"),
            "should teach the error codes"
        );
    }
}
