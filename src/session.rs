//! Getting from "the user ran a command" to a live connection and a known household.
//!
//! Shared by the CLI and the daemon, so both find players the same way.

use std::net::IpAddr;

use anyhow::{Result, bail};

use crate::discover;
use crate::netid;
use crate::sonos::local::Connection;
use crate::sonos::proto::{Groups, Player};
use crate::state::State;

/// A live connection together with the household topology it reported.
pub struct Session {
    pub connection: Connection,
    pub groups: Groups,
}

/// Connect to a player, learn the household from it, and remember what it said.
pub async fn attach(ip: IpAddr, state: &mut State, fingerprint: Option<&str>) -> Result<Session> {
    let connection = Connection::open(ip).await?;
    let groups = connection.groups().await?;
    if let Some(fingerprint) = fingerprint {
        let household = connection.household_id().await?;
        if state.remember(fingerprint, &household, &groups) {
            state.save()?;
        }
    }
    Ok(Session { connection, groups })
}

/// Find a player to talk to: an explicit address, then whatever is remembered for
/// this network, then - only on a network we already know has players - a rescan.
///
/// An unrecognised network is never scanned automatically. This runs on hotel and
/// client-site WiFi, where an unprompted subnet sweep is bad manners at best.
pub async fn connect(explicit: Option<IpAddr>, state: &mut State) -> Result<Session> {
    let fingerprint = netid::network_fingerprint();
    if let Some(ip) = explicit {
        return attach(ip, state, fingerprint.as_deref()).await;
    }

    let Some(fingerprint) = fingerprint.as_deref() else {
        bail!("could not identify this network (no default gateway); pass --ip explicitly");
    };
    let known = state.players_on(fingerprint);
    if known.is_empty() {
        bail!("no players remembered for this network. Run `x2rock discover`, or pass --ip.");
    }

    for player in &known {
        if let Ok(session) = attach(player.ip, state, Some(fingerprint)).await {
            return Ok(session);
        }
    }

    // A known network where nothing answers: addresses have most likely moved.
    eprintln!("Remembered players did not answer; rescanning...");
    let scan = discover::scan_local_subnet(true).await?;
    match scan.found.first() {
        Some(ip) => attach(IpAddr::V4(*ip), state, Some(fingerprint)).await,
        None => {
            let names: Vec<_> = known.iter().map(|p| p.name.as_str()).collect();
            bail!(
                "no players found on this network (previously: {})",
                names.join(", ")
            )
        }
    }
}

/// The group a command applies to.
pub struct Target {
    pub group_id: String,
    pub name: String,
    /// The coordinator owns the group's queue, so UPnP calls go to it.
    pub coordinator_id: String,
    pub coordinator_ip: Option<IpAddr>,
}

pub fn target(groups: &Groups, room: Option<&str>) -> Result<Target> {
    let group = groups.resolve(room)?;
    Ok(Target {
        group_id: group.id.clone(),
        name: group.name.clone(),
        coordinator_id: group.coordinator_id.clone(),
        coordinator_ip: groups.player(&group.coordinator_id).and_then(Player::ip),
    })
}

/// Group commands go to the coordinator, which may not be the player we reached.
pub async fn coordinator(session: &Session, target: &Target) -> Result<Connection> {
    match target.coordinator_ip {
        Some(ip) if ip != session.connection.ip() => Connection::open(ip).await,
        _ => Ok(session.connection.clone()),
    }
}
