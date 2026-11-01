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

    /// install.sh, run in a home of its own with a stub steamos-add-to-steam:
    /// nothing but the arguments and environment set here reach it.
    #[cfg(unix)]
    struct Install {
        dir: x2rock::testdir::TempDir,
    }

    #[cfg(unix)]
    impl Install {
        fn new(name: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = x2rock::testdir::TempDir::new(&format!("deck-install-{name}"));
            let root = dir.path();
            let (bin, pkg) = (root.join("bin"), root.join("pkg"));
            for dir in [&bin, &pkg, &root.join("home")] {
                std::fs::create_dir_all(dir).unwrap();
            }
            // Logs each call; fails while a file named `fail` exists.
            let stub = format!(
                "#!/bin/sh\necho \"$@\" >> '{0}/calls'\n[ ! -e '{0}/fail' ]\n",
                root.display()
            );
            std::fs::write(bin.join("steamos-add-to-steam"), stub).unwrap();
            let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("dist");
            for file in ["install.sh", "x2rock-deck.desktop", "x2rock-deck.png"] {
                std::fs::copy(dist.join(file), pkg.join(file)).unwrap();
            }
            std::fs::write(pkg.join("x2rock-deck"), b"#!/bin/sh\n").unwrap();
            for file in [bin.join("steamos-add-to-steam"), pkg.join("x2rock-deck")] {
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Install { dir }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// Exit code and standard output.
        fn run(&self, args: &[&str]) -> (i32, String) {
            let home = self.root().join("home");
            let path = format!("{}:/usr/bin:/bin", self.root().join("bin").display());
            let out = std::process::Command::new("/bin/sh")
                .arg(self.root().join("pkg/install.sh"))
                .args(args)
                .env_clear()
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("PATH", path)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            (out.status.code().unwrap_or(-1), stdout)
        }

        fn calls(&self) -> usize {
            std::fs::read_to_string(self.root().join("calls"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_script_adds_to_steam_once_and_only_says_so_when_it_did() {
        let i = Install::new("added");
        let (code, out) = i.run(&[]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("Added to Steam"), "{out}");
        assert!(
            i.root()
                .join("home/.local/share/applications/x2rock-deck.desktop")
                .exists(),
            "the menu entry goes under the given XDG_DATA_HOME"
        );
        let (code, out) = i.run(&[]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("Updated x2rock"), "{out}");
        assert!(out.contains("this script added"), "{out}");
        assert_eq!(i.calls(), 1, "an update adds no second shortcut");

        // A failed first add: later runs neither claim a shortcut nor add one.
        let i = Install::new("failed");
        std::fs::write(i.root().join("fail"), b"").unwrap();
        let (code, out) = i.run(&[]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("Add a Non-Steam Game"), "{out}");
        std::fs::remove_file(i.root().join("fail")).unwrap();
        let (code, out) = i.run(&[]);
        assert_eq!(code, 0, "{out}");
        assert!(!out.contains("this script added"), "{out}");
        assert!(out.contains("Add a Non-Steam Game"), "{out}");
        assert_eq!(i.calls(), 1);

        // --remove forgets the add, so a fresh install adds again.
        assert_eq!(i.run(&["--remove"]).0, 0);
        assert!(i.run(&[]).1.contains("Added to Steam"));
        assert_eq!(i.calls(), 2);

        // A mistyped option installs nothing.
        let i = Install::new("typo");
        assert_eq!(i.run(&["--udpate"]).0, 2);
        assert!(!i.root().join("home/Applications").exists());
        assert_eq!(i.calls(), 0);
    }
}
