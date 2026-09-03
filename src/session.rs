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
        // An unregistered network: this gateway has never been discovered on.
        // `players_on` is empty only for a fingerprint not in state, because a
        // network is remembered only once it has players - so this branch *is*
        // the unregistered-network case, and says so by name.
        //
        // The `fix` is deliberately null. `x2rock discover` scans the local
        // network, which must not be auto-run on an unfamiliar one (a cafe, a
        // client site) just because an agent follows a "run the fix" rule - the
        // exact behaviour the road-warrior design avoids. Discovery is offered,
        // not run. The message says how; the field withholds the command.
        return Err(crate::hint::Hint::new(
            format!(
                "unregistered network (gateway {fingerprint}): no speakers are known here. This is \
                 normal away from home. `x2rock discover` will scan *this* network for speakers - \
                 offer it rather than run it unasked - or pass `--ip <speaker>`."
            ),
            "unregistered_network",
            None,
        )
        .into());
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
            Err(crate::hint::Hint::new(
                format!(
                    "no players found on this network (previously: {})",
                    names.join(", ")
                ),
                "no_player",
                Some("x2rock discover".into()),
            )
            .into())
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
