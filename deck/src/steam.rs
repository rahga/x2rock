//! The Steam library's artwork for x2rock, which a non-Steam game has none of.
//!
//! Steam launches a non-Steam shortcut with `SteamGameId` set to its id: the
//! shortcut's app id in the top 32 bits, `0x02000000` below. Its artwork is
//! read from each Steam user's `config/grid/`, by that app id: `<id>p.png` the
//! portrait capsule, `<id>.png` the wide one, `<id>_hero.png`, `<id>_logo.png`.
//! Written once, and never over a picture someone chose themselves.

use std::path::{Path, PathBuf};

const CAPSULE: &[u8] = include_bytes!("../dist/steam/capsule.png");
const WIDE: &[u8] = include_bytes!("../dist/steam/wide.png");
const HERO: &[u8] = include_bytes!("../dist/steam/hero.png");
const LOGO: &[u8] = include_bytes!("../dist/steam/logo.png");

/// The shortcut's app id, from a `SteamGameId` that names a non-Steam game.
pub fn shortcut_app_id(game_id: &str) -> Option<u32> {
    let id: u64 = game_id.trim().parse().ok()?;
    ((id & 0xFFFF_FFFF) == 0x0200_0000).then_some((id >> 32) as u32)
}

/// Every Steam user's grid directory on this machine.
fn grid_dirs(home: &Path) -> Vec<PathBuf> {
    let roots = [home.join(".steam/steam"), home.join(".local/share/Steam")];
    let mut dirs = Vec::new();
    for root in roots {
        let Ok(users) = std::fs::read_dir(root.join("userdata")) else {
            continue;
        };
        for user in users.flatten() {
            let grid = user.path().join("config/grid");
            if user.path().is_dir() && !dirs.contains(&grid) {
                dirs.push(grid);
            }
        }
    }
    dirs
}

/// Put the artwork where Steam looks for it, if this is a Steam launch.
pub fn place_artwork() {
    let Some(app) = std::env::var("SteamGameId")
        .ok()
        .as_deref()
        .and_then(shortcut_app_id)
    else {
        return;
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    for grid in grid_dirs(&home) {
        let _ = place_in(&grid, app);
    }
}

fn place_in(grid: &Path, app: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(grid)?;
    for (name, bytes) in [
        (format!("{app}p.png"), CAPSULE),
        (format!("{app}.png"), WIDE),
        (format!("{app}_hero.png"), HERO),
        (format!("{app}_logo.png"), LOGO),
    ] {
        let path = grid.join(name);
        if !path.exists() {
            std::fs::write(path, bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shortcuts_game_id_gives_its_app_id_and_a_steam_games_does_not() {
        let app: u64 = 3_456_789_012;
        assert_eq!(
            shortcut_app_id(&((app << 32) | 0x0200_0000).to_string()),
            Some(3_456_789_012)
        );
        assert_eq!(shortcut_app_id("1675200"), None, "a Steam game's own id");
        assert_eq!(shortcut_app_id("not a number"), None);
    }

    #[test]
    fn artwork_is_written_once_and_never_over_a_chosen_picture() {
        let dir = std::env::temp_dir().join(format!("x2rock-deck-grid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("42p.png"), b"mine").unwrap();
        place_in(&dir, 42).unwrap();
        assert_eq!(std::fs::read(dir.join("42p.png")).unwrap(), b"mine");
        assert_eq!(std::fs::read(dir.join("42_hero.png")).unwrap(), HERO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn install_script_adds_to_steam_only_on_first_install() {
        use std::os::unix::fs::PermissionsExt;
        let temp =
            std::env::temp_dir().join(format!("x2rock-deck-install-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let bin_dir = temp.join("bin");
        let pkg_dir = temp.join("pkg");
        let home_dir = temp.join("home");
        let log_file = temp.join("calls.log");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::create_dir_all(&home_dir).unwrap();

        // Stub steamos-add-to-steam
        let stub = bin_dir.join("steamos-add-to-steam");
        std::fs::write(
            &stub,
            format!("#!/bin/sh\necho \"$@\" >> \"{}\"\n", log_file.display()),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Copy install files to pkg_dir
        let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
        std::fs::copy(dist.join("install.sh"), pkg_dir.join("install.sh")).unwrap();
        std::fs::copy(
            dist.join("x2rock-deck.desktop"),
            pkg_dir.join("x2rock-deck.desktop"),
        )
        .unwrap();
        std::fs::copy(
            dist.join("x2rock-deck.png"),
            pkg_dir.join("x2rock-deck.png"),
        )
        .unwrap();
        std::fs::write(pkg_dir.join("x2rock-deck"), b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(
            pkg_dir.join("install.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            pkg_dir.join("x2rock-deck"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let path_env = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        // First run: fresh install
        let output1 = std::process::Command::new("sh")
            .arg(pkg_dir.join("install.sh"))
            .env("HOME", &home_dir)
            .env("PATH", &path_env)
            .output()
            .unwrap();
        assert!(
            output1.status.success(),
            "first install failed: {}",
            String::from_utf8_lossy(&output1.stderr)
        );
        let out1 = String::from_utf8_lossy(&output1.stdout);
        assert!(out1.contains("Installed x2rock"), "{out1}");
        assert!(out1.contains("Added to Steam"), "{out1}");
        let calls = std::fs::read_to_string(&log_file).unwrap();
        assert_eq!(
            calls.lines().count(),
            1,
            "should have called steamos-add-to-steam once"
        );

        // Second run: update
        let output2 = std::process::Command::new("sh")
            .arg(pkg_dir.join("install.sh"))
            .env("HOME", &home_dir)
            .env("PATH", &path_env)
            .output()
            .unwrap();
        assert!(
            output2.status.success(),
            "second install failed: {}",
            String::from_utf8_lossy(&output2.stderr)
        );
        let out2 = String::from_utf8_lossy(&output2.stdout);
        assert!(out2.contains("Updated x2rock"), "{out2}");
        assert!(!out2.contains("Added to Steam"), "{out2}");
        let calls_after = std::fs::read_to_string(&log_file).unwrap();
        assert_eq!(
            calls_after.lines().count(),
            1,
            "should not call steamos-add-to-steam on update"
        );

        let _ = std::fs::remove_dir_all(&temp);
    }
}
