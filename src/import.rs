//! Music-service sign-ins the household already holds, read off a speaker and
//! kept in this machine's credential store: the CLI's `link --from-household`,
//! and how any other front end reaches services that need an account. Needs
//! the speaker salt (see `salt`); the capture needs the speaker to reach this
//! machine on a TCP port once.

use std::time::Duration;

use anyhow::{Result, ensure};

use crate::{catalogue, credentials, session, sonos};


/// What the household should call this machine's account, when nothing was given.
///
/// The hostname, because a household can have several machines linked to the
/// same service account and the Sonos app shows this string.
pub fn default_nickname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| format!("x2rock on {h}"))
        .unwrap_or_else(|| "x2rock".to_string())
}

/// Capture the household's encrypted account blob, with the short household id
/// the key derives from.
///
/// The blob is keyed to the *short* form (no `.suffix`), which is what
/// `GetHouseholdID` returns, while the SMAPI header and the credentials record
/// want the long one. The short is the long up to its first dot.
pub async fn capture_stored(
    session: &session::Session,
    long_household: &str,
    callback_port: u16,
) -> Result<(String, String)> {
    let short = sonos::stored::short_household(long_household).to_string();
    let encoded = sonos::stored::capture_envelope(
        session.connection.ip(),
        callback_port,
        HOUSEHOLD_EVENT_TIMEOUT,
    )
    .await?;
    Ok((short, encoded))
}

/// How long to wait for the player's account event. Generous: it is one round
/// trip, but it depends on the player choosing to open a connection back, and a
/// firewall that is going to drop it will drop it for the whole window.
pub const HOUSEHOLD_EVENT_TIMEOUT: Duration = Duration::from_secs(12);

/// `x2rock link --from-household`: keep the token the household already stores,
/// with no browser flow.
///
/// The counterpart to the whole device-link dance: instead of minting a token,
/// read the one the Sonos app minted and left on the speaker. Named a service
/// takes just that one; unnamed imports every service the household holds a
/// usable token for. No `match` step - this path never carries a
/// `userIdHashCode`, and playback rides the household's own registration, which
/// adding the service in the Sonos app already made.
pub async fn household_accounts(
    session: &session::Session,
    catalogue: &catalogue::Catalogue,
    linked: &mut credentials::Credentials,
    long_household: &str,
    service: Option<&String>,
    nickname: Option<&String>,
    callback_port: u16,
) -> Result<Vec<(String, String)>> {
    // Before the capture: without a salt there is nothing to decrypt with, and
    // the capture is a twelve-second wait on a firewall.
    let salt = crate::salt::require()?;
    let (short_household, encoded) = capture_stored(session, long_household, callback_port).await?;
    let accounts = sonos::stored::decrypt_accounts(&encoded, &short_household, &salt)?;

    // Only accounts that actually carry a token can be injected; the rest are
    // placeholders. Keyed by service id, which is how the catalogue and the
    // credentials store both file them.
    let usable: Vec<_> = accounts.into_iter().filter(|a| a.has_token()).collect();
    ensure!(
        !usable.is_empty(),
        "the household stores no music-service token this machine can read. \
         Add a service in the Sonos app first, then run this again."
    );

    // With a service named, resolve it to an id and keep only that one; the
    // resolution also gives the catalogue's own name for the record.
    let wanted = match service {
        Some(query) => {
            let chosen = catalogue.find_any(query)?;
            let id = chosen.id.clone();
            // Every account the household holds for it, not the first: naming
            // a service narrows *which service* to import, and a service with
            // two accounts has two either way.
            let matched: Vec<_> = usable
                .into_iter()
                .filter(|a| a.service_id.to_string() == id)
                .map(|a| (chosen.name.clone(), id.clone(), a))
                .collect();
            ensure!(
                !matched.is_empty(),
                "the household stores no token for {} - it has not been added \
                 in the Sonos app on this household",
                chosen.name
            );
            matched
        }
        None => usable
            .into_iter()
            .map(|a| {
                let id = a.service_id.to_string();
                // The catalogue names the service where it knows it; a token for
                // a service not in this household's catalogue is still kept, under
                // its own stored nickname or its id.
                let name = catalogue
                    .name_of(&id)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if a.nickname.is_empty() {
                            format!("service {id}")
                        } else {
                            a.nickname.clone()
                        }
                    });
                (name, id, a)
            })
            .collect(),
    };

    let mut records = Vec::with_capacity(wanted.len());
    for (name, id, account) in &wanted {
        // The stored nickname is the app's own label ("Qb1"); prefer an explicit
        // --nickname, then that, then this machine's default.
        let nick = nickname
            .map(String::to_string)
            .or_else(|| (!account.nickname.is_empty()).then(|| account.nickname.clone()))
            .unwrap_or_else(default_nickname);
        let auth = sonos::smapi::DeviceAuth {
            auth_token: account.token.clone(),
            private_key: account.key.clone(),
            user_id_hash_code: None,
        };
        let mut record =
            credentials::from_device_auth(name, Some(long_household), Some(&nick), auth);
        // The household's own serial for the account, which only this path ever
        // sees. It is what keys the record, so two accounts of one service stay
        // two records instead of the second landing on the first.
        record.serial = Some(account.serial);
        // The per-account `Username` selector, kept as a within-household
        // identity for the one shape the serial cannot cover: a second account
        // served with no distinct serial. `0`/empty name no account, so they
        // are dropped rather than stored as a key that identifies nothing.
        record.account_key = match account.account_key.as_str() {
            "" | "0" => None,
            k => Some(k.to_string()),
        };
        records.push((id.clone(), record));
    }
    linked.update(|now| {
        for (id, record) in records {
            now.remember(long_household, &id, record);
        }
        Ok(())
    })?;
    Ok(wanted.into_iter().map(|(name, id, _)| (name, id)).collect())
}
