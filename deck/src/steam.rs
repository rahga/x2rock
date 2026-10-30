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
        let Ok(users) = std::fs::read_dir(root.join("userdata")) else { continue };
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
    let Some(app) = std::env::var("SteamGameId").ok().as_deref().and_then(shortcut_app_id) else { return };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
    for grid in grid_dirs(&home) {
        let _ = place_in(&grid, app);
    }
}

fn place_in(grid: &Path, app: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(grid)?;
    for (name, bytes) in [(format!("{app}p.png"), CAPSULE), (format!("{app}.png"), WIDE), (format!("{app}_hero.png"), HERO), (format!("{app}_logo.png"), LOGO)] {
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
        assert_eq!(shortcut_app_id(&((app << 32) | 0x0200_0000).to_string()), Some(3_456_789_012));
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
}
