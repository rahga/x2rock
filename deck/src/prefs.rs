//! This device's own settings, under the TV app's names for them: the theme,
//! the room order, the television's room, and the rest as they arrive. One
//! JSON file in the platform's config directory (`~/.config/x2rock-deck/` on
//! Linux, `%APPDATA%\rahga\x2rock-deck\` on Windows), written whole by
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
}

impl Prefs {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "rahga", "x2rock-deck")
            .map(|d| d.config_dir().join("prefs.json"))
    }

    pub fn load() -> Self {
        let path = Self::path();
        let mut prefs: Prefs = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        prefs.path = path;
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
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
            std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = result {
            eprintln!(
                "x2rock-deck: could not save settings to {}: {e}",
                path.display()
            );
        }
    }
}
