//! This device's own settings, under the TV app's names for them: the theme,
//! the room order, the television's room, and the rest as they arrive. One
//! JSON file in the platform's config directory (`~/.config/x2rock-deck/` on
//! Linux, `%APPDATA%\rahga\x2rock-deck\config\` on Windows), written whole by
//! renaming a temporary over it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A scene to bring back with one press: these speakers at these levels, and
/// the favourite playing if what played was one (PresetStore.kt). Players, not
/// groups: a regroup mints new group ids, and the speakers stay put.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub player_ids: Vec<String>,
    pub volumes: std::collections::BTreeMap<String, u8>,
    pub favorite_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// An `AppColorTheme` name.
    pub color_theme: Option<String>,
    /// Player ids in the order the viewer placed them; rooms never placed
    /// follow alphabetically.
    pub room_ranks: Vec<String>,
    /// The soundbar this screen sits on, stated or detected: a player id.
    pub tv_player_id: Option<String>,
    pub recent_searches: Vec<String>,
    pub preferred_services: Vec<String>,
    /// "stay" keeps Browse open after starting something.
    pub after_playing: Option<String>,
    pub beta_features: bool,
    pub presets: Vec<Preset>,
    /// On by default: a newer release is looked for once a launch when on.
    pub check_for_updates: Option<bool>,
    #[serde(skip)]
    pub theme: String,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    unreadable: bool,
}

impl Prefs {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "rahga", "x2rock-deck")
            .map(|d| d.config_dir().join("prefs.json"))
    }

    pub fn load() -> Self {
        Self::load_from(Self::path())
    }

    pub fn load_from(path: Option<PathBuf>) -> Self {
        let Some(p) = path else {
            return Self::default();
        };
        // Missing is default; unreadable or corrupt is kept aside at the next save.
        let mut prefs = x2rock::store::load_json::<Prefs>(&p).unwrap_or_else(|e| {
            eprintln!("x2rock-deck: could not load settings: {e:#}");
            Prefs {
                unreadable: true,
                ..Default::default()
            }
        });
        prefs.path = Some(p);
        prefs.theme = prefs
            .color_theme
            .clone()
            .unwrap_or_else(|| "DEFAULT".into());
        prefs
    }

    /// Kept in memory only: the demo and the screenshots.
    pub fn ephemeral(theme: &str) -> Self {
        Prefs {
            theme: theme.to_owned(),
            color_theme: Some(theme.to_owned()),
            ..Default::default()
        }
    }

    pub fn save(&mut self) {
        self.color_theme = Some(self.theme.clone());
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.unreadable {
            if path.exists() {
                let bad = x2rock::store::sibling(&path, "bad");
                if let Err(e) = std::fs::rename(&path, &bad) {
                    eprintln!(
                        "x2rock-deck: could not preserve unreadable settings to {}: {e}",
                        bad.display()
                    );
                }
            }
            self.unreadable = false;
        }
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        if let Err(e) = x2rock::store::write_atomically(&path, &text, x2rock::store::PLAIN) {
            eprintln!(
                "x2rock-deck: could not save settings to {}: {e:#}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_prefs_preserved_as_bad_and_no_tmp_left_behind() {
        let temp = x2rock::testdir::TempDir::new("prefs-unreadable");
        let prefs_path = temp.path().join("prefs.json");
        let garbage = b"{ invalid json: true, oops ";
        std::fs::write(&prefs_path, garbage).expect("write garbage prefs");

        let mut prefs = Prefs::load_from(Some(prefs_path.clone()));
        assert_eq!(prefs.theme, "DEFAULT");
        assert!(prefs.unreadable);

        prefs.save();

        let bad_path = x2rock::store::sibling(&prefs_path, "bad");
        assert!(bad_path.exists(), "prefs.json.bad should exist");
        assert_eq!(
            std::fs::read(&bad_path).expect("read bad file"),
            garbage,
            "garbage preserved under .bad"
        );

        assert!(prefs_path.exists(), "new prefs.json should exist");
        let new_text = std::fs::read_to_string(&prefs_path).expect("read new prefs");
        let parsed: Prefs = serde_json::from_str(&new_text).expect("new prefs must be valid json");
        assert_eq!(parsed.color_theme.as_deref(), Some("DEFAULT"));
        let reloaded = Prefs::load_from(Some(prefs_path.clone()));
        assert_eq!(reloaded.theme, "DEFAULT");
        assert!(!reloaded.unreadable);

        // Verify no .tmp file was left behind
        let entries = std::fs::read_dir(temp.path()).expect("read temp dir");
        for entry in entries {
            let entry = entry.expect("dir entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.ends_with(".tmp"),
                "found leftover temporary file: {name}"
            );
        }

        // A second save does not overwrite .bad with the newly saved prefs
        prefs.theme = "OCEAN".into();
        prefs.save();
        assert_eq!(
            std::fs::read(&bad_path).expect("read bad file"),
            garbage,
            ".bad still holds original garbage"
        );
    }

    #[test]
    fn missing_prefs_loads_defaults_without_creating_bad() {
        let temp = x2rock::testdir::TempDir::new("prefs-missing");
        let prefs_path = temp.path().join("prefs.json");

        let mut prefs = Prefs::load_from(Some(prefs_path.clone()));
        assert_eq!(prefs.theme, "DEFAULT");
        assert!(!prefs.unreadable);

        prefs.save();

        let bad_path = x2rock::store::sibling(&prefs_path, "bad");
        assert!(!bad_path.exists(), "no .bad should be created when missing");
        assert!(prefs_path.exists(), "prefs.json should be created");
    }
}
