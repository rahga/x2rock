//! What x2rock remembers between runs: which players live on which network.
//!
//! Stored under `$XDG_STATE_HOME/x2rock/` rather than the config directory - it
//! is regenerable, machine-discovered state, not something the user wrote. Keyed by
//! network fingerprint, because this laptop moves and a player address is only
//! meaningful on the network it was found on.

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::sonos::proto::Groups;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Network fingerprint -> household id -> players seen there.
    #[serde(default)]
    pub networks: BTreeMap<String, BTreeMap<String, Vec<KnownPlayer>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPlayer {
    pub id: String,
    pub name: String,
    pub ip: IpAddr,
}

fn path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    let dir = dirs
        .state_dir()
        .ok_or_else(|| anyhow!("no XDG state directory on this platform"))?;
    Ok(dir.join("networks.json"))
}

impl State {
    /// Load, treating a missing file as empty state.
    pub fn load() -> Result<Self> {
        let path = path()?;
        match fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically, so a crash mid-write cannot leave a truncated file.
    pub fn save(&self) -> Result<()> {
        let path = path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    /// Record what a household reported about itself. Returns whether anything changed,
    /// so callers can skip the write on the common no-op path.
    pub fn remember(&mut self, fingerprint: &str, household: &str, groups: &Groups) -> bool {
        let mut players: Vec<KnownPlayer> = groups
            .players
            .iter()
            .filter_map(|p| {
                Some(KnownPlayer {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    ip: p.ip()?,
                })
            })
            .collect();
        players.sort_by(|a, b| a.name.cmp(&b.name));

        let slot = self
            .networks
            .entry(fingerprint.to_string())
            .or_default()
            .entry(household.to_string())
            .or_default();
        if *slot == players {
            return false;
        }
        *slot = players;
        true
    }

    /// Every player remembered on a network, across households.
    pub fn players_on(&self, fingerprint: &str) -> Vec<KnownPlayer> {
        self.networks
            .get(fingerprint)
            .map(|households| households.values().flatten().cloned().collect())
            .unwrap_or_default()
    }
}
