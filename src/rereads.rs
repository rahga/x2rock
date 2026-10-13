//! A token imported from the household, re-read from it when a service refuses
//! it.
//!
//! An imported token is a copy, and the copy goes stale on its own schedule:
//! Pandora's, imported 2026-10-11, was refused a day later with "Failed to
//! reauth device id", and the refresh SMAPI offers did nothing for it. The
//! Sonos app had kept the household's own copy fresh the whole time, so
//! `link --from-household` fixed it by hand. This does the same, once, inside
//! whichever call was refused - see `smapi::call`.
//!
//! **Only for imported accounts.** A browser link's token is x2rock's own, and
//! the household's token for the same service is not a substitute: Bandcamp
//! answers the household's with `Client.LoginInvalid` everywhere but a
//! speaker. An account counts as imported when its record carries the serial
//! only an import reads (`Account::serial`), not one `match` handed back.
//!
//! **Any refusal, not a worded one.** What a service says about a token it
//! will not take varies past filtering: Pandora's stale one was "Failed to
//! reauth device id", a garbled one `TypeError: Cannot read properties of null
//! (reading 'getListenerId')` (2026-10-13). The signal is the household
//! holding a different token for the account, which only a read can tell.
//!
//! **Bounded cost.** Reading the store means a speaker calling back into this
//! machine on [`crate::netid::INBOUND_PORT`], a second or so when the firewall
//! allows it. One read per household serves every refusal for
//! [`KEEP_FOR`], failures included - a firewall that drops the callback would
//! otherwise cost [`WAIT`] on every refused call - and the reads are taken one
//! at a time, so a search across every service refused at once reads once.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::credentials::Credentials;
use crate::sonos::smapi::{RefreshedToken, Token};
use crate::sonos::stored::{self, StoredAccount};

/// How long one read of a household's store answers for it.
const KEEP_FOR: Duration = Duration::from_secs(10 * 60);

/// How long to wait for the speaker's callback. Shorter than
/// `link --from-household` allows: that is someone asking for it, this is a
/// detour inside a command that was asked for something else.
const WAIT: Duration = Duration::from_secs(5);

/// Players reached in this process, most recent first, and how many are kept.
/// A read has to go to a player of the refused token's household; which one is
/// learned by trying, since a wrong household fails its integrity check.
static PLAYERS: Mutex<Vec<IpAddr>> = Mutex::new(Vec::new());
const PLAYERS_KEPT: usize = 4;

/// What each household's store held when it was last read; `None` for a read
/// that failed.
static READS: Mutex<Option<Reads>> = Mutex::new(None);

/// One read at a time, across the process.
static READING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Remember a player this process reached, as a place to read a store from.
pub fn reached(ip: IpAddr) {
    let mut players = PLAYERS.lock().unwrap_or_else(|e| e.into_inner());
    players.retain(|p| *p != ip);
    players.insert(0, ip);
    players.truncate(PLAYERS_KEPT);
}

/// The household's current token for the account `refused` came from, when it
/// is an imported account and the household now holds a different one - saved
/// to the store on the way out. `None` for anything else, which leaves the
/// refusal standing.
pub async fn reread(service_id: &str, refused: &Token) -> Option<RefreshedToken> {
    let household = refused.household.as_deref()?;
    let serial = refused.serial?;
    let linked = Credentials::load().ok()?;
    let held = linked.accounts_for(household, service_id)?;
    let (key, used) = match refused.account.as_deref() {
        Some(key) => (key, held.accounts.get(key)?),
        None => held.chosen()?,
    };
    if used.serial != Some(serial) {
        return None;
    }
    let (key, used) = (key.to_string(), used.clone());

    let accounts = store_of(household).await?;
    let fresh = accounts
        .iter()
        .find(|a| a.service_id.to_string() == service_id && a.serial == serial)
        .filter(|a| a.has_token() && a.token != refused.token)?;
    let refreshed = RefreshedToken {
        auth_token: fresh.token.clone(),
        private_key: fresh.key.clone(),
        user_id_hash_code: None,
    };
    let mut linked = linked;
    let _ = linked.update(|now| {
        now.apply_refresh(household, service_id, Some(&key), &used, refreshed.clone());
        Ok(())
    });
    Some(refreshed)
}

/// The household's stored accounts, read at most once per [`KEEP_FOR`].
async fn store_of(household: &str) -> Option<Vec<StoredAccount>> {
    let _one = READING.lock().await;
    let kept = with_reads(|reads| {
        reads
            .get(household)
            .filter(|(at, _)| at.elapsed() < KEEP_FOR)
            .map(|(_, read)| read.clone())
    });
    if let Some(read) = kept {
        return read;
    }
    let short = household.split('.').next().unwrap_or(household);
    let players = PLAYERS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut read = None;
    for player in players {
        let Ok(encoded) = stored::capture_envelope(player, crate::netid::INBOUND_PORT, WAIT).await
        else {
            continue;
        };
        if let Ok(accounts) = stored::decrypt_accounts(&encoded, short) {
            read = Some(accounts);
            break;
        }
    }
    with_reads(|reads| reads.insert(household.to_string(), (Instant::now(), read.clone())));
    read
}

type Reads = HashMap<String, (Instant, Option<Vec<StoredAccount>>)>;

fn with_reads<T>(change: impl FnOnce(&mut Reads) -> T) -> T {
    let mut reads = READS.lock().unwrap_or_else(|e| e.into_inner());
    change(reads.get_or_insert_with(HashMap::new))
}
