//! The one secret x2rock keeps: a music service's device-link token.
//!
//! Everything else this tool stores is either regenerable (the player list, the
//! service catalogue) or merely annoying to lose (bookmarks). This is different:
//! an `authToken` and a `privateKey` minted by a music service for this machine,
//! which will play that account's music for anyone holding them.
//!
//! Stored under `$XDG_STATE_HOME/x2rock/credentials.json`, **mode 0600**, in its
//! own file rather than mixed into any other. Three reasons for a separate file:
//! it can be backed up or deleted on its own, `cat`ing the player list in front
//! of someone stays harmless, and the permission bits belong to a file whose
//! every byte is secret rather than to one where they would be over-strict.
//!
//! Deliberately *not* the DBus Secret Service. A keyring would encrypt this at
//! rest, at the cost of a new dependency and a new failure mode - a locked or
//! absent keyring standing between a person and their music - in a tool that is
//! expected to work over ssh and in a bar widget's subprocess. The token is
//! scoped to one music service, it is revocable from that service's own account
//! page, and this file leans on the disk encryption a laptop already has.
//!
//! Keyed by **household, then service id** - not by name, because a name in
//! Sonos's catalogue can change under a stable id, and not by service alone,
//! because a machine that moves between households holds a separate account for
//! each. The name is kept alongside for display.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::sonos::smapi::{DeviceAuth, Token};
use crate::store;

/// The shape of the file. Schema 2 keys accounts by household; schema 1's flat
/// `services` map is not read back. There are no users to migrate, so there is
/// no migration - but a file of any other schema is *refused* rather than read
/// as empty, because unknown fields are ignored on load and an old file would
/// otherwise deserialize to an empty store, report every service as unlinked,
/// and be overwritten by the next save. This file is its own only copy.
const SCHEMA: u32 = 2;

/// Everything past owner read/write. A secret with any of these set is a bug
/// somewhere, most likely a hand-edit or a careless copy.
const LOOSE: u32 = 0o177;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub schema: u32,
    /// Household id -> (service id -> the account held for it there).
    ///
    /// Keyed by household because one machine sees more than one - a laptop that
    /// moves between home and the office - and each household holds its own
    /// account for a service, with its own token. Keeping them apart is what
    /// lets a search on the home network use the home token while the office
    /// token sits untouched, and what stops an auto-refresh on one from
    /// clobbering the other. Within a household it is still one account per
    /// service; several accounts for one service in one household is a Sonos
    /// feature nothing has asked for yet.
    ///
    /// The old flat `services` map (schema 1) is not read back; such a file is
    /// refused by [`Credentials::check_schema`] with the re-import named, rather
    /// than loading as no accounts at all.
    #[serde(default)]
    pub households: BTreeMap<String, BTreeMap<String, Account>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// Which service this is, for display. The id is the key.
    pub service_name: String,
    /// `authToken` from `getDeviceAuthToken`. Secret.
    pub auth_token: String,
    /// `privateKey` from the same reply. Secret.
    pub private_key: String,
    /// `userIdHashCode`, also from that reply.
    ///
    /// Worth its own note: an earlier reading of the Control API spec concluded
    /// that only a service's own SMAPI server could compute this, and used that
    /// to argue a controller could never register an account. Wrong - the field
    /// is handed to whoever completes the link, because it is the controller
    /// that later calls `musicServiceAccounts:1 match`. Not every service sends
    /// one, so it is optional, and without it `match` cannot be attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id_hash_code: Option<String>,
    /// What the household should call this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// The household the token was minted against. Sent back in the SMAPI
    /// `loginToken` header, which is why it is stored rather than re-derived:
    /// searching from a cached catalogue must not need a player on the LAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub household: Option<String>,
    /// The account id `match` gave back, when the household was reached. Absent
    /// means the token works for search but the household does not know about
    /// the account yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// When the link completed, epoch seconds - the same unit bookmarks use.
    pub linked: u64,
}

impl Account {
    /// What goes in the SMAPI credentials header.
    pub fn token(&self) -> Token {
        Token {
            token: self.auth_token.clone(),
            key: self.private_key.clone(),
            household: self.household.clone(),
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> Result<PathBuf> {
    store::path("credentials.json")
}

impl Credentials {
    /// Load, treating a missing file as empty.
    ///
    /// A corrupt file is an error, as with bookmarks: silently starting over
    /// would present itself as "no account linked" and send someone through the
    /// browser flow again to fix a typo in a file they could have edited back.
    pub fn load() -> Result<Self> {
        let path = path()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let Some(text) = store::read_optional(path)? else {
            return Ok(Self::default());
        };
        // Tighten rather than warn and carry on. A secret readable by the rest
        // of the machine is worth fixing at the first opportunity, and the fix
        // is one syscall that cannot lose anything.
        if let Ok(meta) = fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & LOOSE != 0 {
                match fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
                    Ok(()) => eprintln!(
                        "x2rock: {} was mode {:04o}; tightened to 0600",
                        path.display(),
                        mode & 0o7777
                    ),
                    Err(e) => eprintln!(
                        "x2rock: {} is mode {:04o} and could not be tightened ({e})",
                        path.display(),
                        mode & 0o7777
                    ),
                }
            }
        }
        let creds: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        creds.check_schema(path)?;
        Ok(creds)
    }

    /// Refuse a file this build does not write.
    ///
    /// Serde ignores unknown fields, so a schema-1 file - tokens under a flat
    /// `services` map - parses happily into zero households. Left there it would
    /// present itself as "nothing linked", send someone back through a link
    /// flow, and then be overwritten by the first save that followed, taking the
    /// only copy of those tokens with it. The same holds in reverse for a file
    /// from a newer build: whatever it keeps that this one cannot read would not
    /// survive the round trip.
    ///
    /// So this is loud, and it names both ways out: the re-import, or the
    /// deletion that a wipe would otherwise have done for you (`unlink --all`
    /// loads the store too, so it cannot be the escape hatch here).
    fn check_schema(&self, path: &Path) -> Result<()> {
        if self.schema == SCHEMA {
            return Ok(());
        }
        let path = path.display();
        if self.schema < SCHEMA {
            bail!(
                "{path} is schema {} - an older x2rock keyed tokens by service alone, \
                 and this build keys them by household. The tokens are still valid at \
                 their services: re-import them with `x2rock link --from-household`, \
                 or delete the file to start over.",
                self.schema
            );
        }
        bail!(
            "{path} is schema {}, written by a newer x2rock than this one. Upgrade, or \
             move the file aside - saving over it from here would drop whatever this \
             build cannot read.",
            self.schema
        )
    }

    /// Write atomically at 0600.
    ///
    /// The mode is set when the temporary file is *created*, not after it is
    /// written: a `chmod` afterwards leaves a window in which the token exists
    /// on disk at the umask's mercy. `store` creates it fresh for the same
    /// reason - a mode on `open` binds only a file that did not exist, and a
    /// leftover at 0644 would otherwise have kept its bits under the secret.
    pub fn save(&self) -> Result<()> {
        self.save_to(&path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let copy = Self {
            schema: SCHEMA,
            households: self.households.clone(),
        };
        store::write_atomically(path, &serde_json::to_string_pretty(&copy)?, store::SECRET)
    }

    /// The account held for a service *in one household*.
    pub fn get(&self, household: &str, service_id: &str) -> Option<&Account> {
        self.households.get(household)?.get(service_id)
    }

    /// The token held for a service in a household - what every play path hands
    /// SMAPI. The household is the one the caller is currently connected to, so
    /// a machine that moves between systems uses the right account for each.
    pub fn token_for(&self, household: &str, service_id: &str) -> Option<Token> {
        self.get(household, service_id).map(Account::token)
    }

    /// Every account, across all households, as `(household, service_id,
    /// account)`. For `accounts`, which lists what the whole store holds.
    pub fn all(&self) -> impl Iterator<Item = (&str, &str, &Account)> {
        self.households.iter().flat_map(|(hh, services)| {
            services
                .iter()
                .map(move |(id, account)| (hh.as_str(), id.as_str(), account))
        })
    }

    /// Whether the store holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.households.values().all(BTreeMap::is_empty)
    }

    /// The one household this store knows, if it knows exactly one - a fallback
    /// for a command running with no player reached (an offline browse of a
    /// cached catalogue), where there is no live household to ask. `None` when
    /// zero or several are held, since then it cannot be guessed.
    pub fn sole_household(&self) -> Option<String> {
        let mut keys = self.households.keys();
        let first = keys.next()?;
        keys.next().is_none().then(|| first.clone())
    }

    /// Resolve a query (service id, exact name, or unique name prefix) to a
    /// service id, searching across every household. For `unlink`, which has no
    /// player and so no single household in hand: it forgets the named service
    /// from all of them.
    ///
    /// A prefix that matches several distinct services is refused by name
    /// rather than resolved to the first - forgetting the wrong token is a quiet
    /// mistake that only surfaces the next time that service is asked to play.
    pub fn find_service_id(&self, query: &str) -> Result<(String, String)> {
        // Collapse to one entry per service id, since the same service can sit
        // in several households; any household's copy names it.
        let mut by_id: BTreeMap<&str, &Account> = BTreeMap::new();
        for (_, id, account) in self.all() {
            by_id.entry(id).or_insert(account);
        }
        if let Some(account) = by_id.get(query) {
            return Ok((query.to_string(), account.service_name.clone()));
        }
        if let Some((id, account)) = by_id
            .iter()
            .find(|(_, a)| a.service_name.eq_ignore_ascii_case(query))
        {
            return Ok((id.to_string(), account.service_name.clone()));
        }
        let needle = query.to_lowercase();
        let matches: Vec<_> = by_id
            .iter()
            .filter(|(_, a)| a.service_name.to_lowercase().starts_with(&needle))
            .collect();
        match matches.len() {
            0 => bail!("no account linked for {query:?}. Run `x2rock accounts` to see them."),
            1 => {
                let (id, account) = matches[0];
                Ok((id.to_string(), account.service_name.clone()))
            }
            several => {
                let shown: Vec<_> = matches
                    .iter()
                    .map(|(id, a)| format!("{} (id {id})", a.service_name))
                    .collect();
                bail!(
                    "{several} linked services start with {query:?}: {}. \
                     Give the id, or the whole name.",
                    shown.join(", ")
                )
            }
        }
    }

    /// Record a completed link for a household, keeping what the service did not
    /// send this time.
    ///
    /// Re-linking a service already linked in this household is the repair path
    /// for a revoked or expired token, so the new secrets always win, while the
    /// nickname, hash and matched account id survive when the new record omits
    /// them.
    pub fn remember(&mut self, household: &str, service_id: &str, mut account: Account) {
        if let Some(old) = self.get(household, service_id) {
            account.nickname = account.nickname.or_else(|| old.nickname.clone());
            account.user_id_hash_code = account
                .user_id_hash_code
                .or_else(|| old.user_id_hash_code.clone());
            account.account_id = account.account_id.or_else(|| old.account_id.clone());
        }
        self.households
            .entry(household.to_string())
            .or_default()
            .insert(service_id.to_string(), account);
    }

    /// Forget a service's account in one household. Returns what was dropped.
    pub fn forget(&mut self, household: &str, service_id: &str) -> Option<Account> {
        let services = self.households.get_mut(household)?;
        let dropped = services.remove(service_id);
        if services.is_empty() {
            self.households.remove(household);
        }
        dropped
    }

    /// Drop a whole household. Returns how many accounts it held, so a wipe of
    /// one household can say what it cleared.
    pub fn forget_household(&mut self, household: &str) -> usize {
        self.households
            .remove(household)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Resolve a household query - an exact stored id, or a unique
    /// case-insensitive substring of one - to the stored key. Household ids are
    /// long and ugly, so a distinctive fragment (what `accounts` shows) is
    /// enough; an ambiguous one is refused by naming the matches rather than
    /// wiping the wrong household.
    pub fn resolve_household(&self, query: &str) -> Result<String> {
        if self.households.contains_key(query) {
            return Ok(query.to_string());
        }
        let needle = query.to_lowercase();
        let matches: Vec<&String> = self
            .households
            .keys()
            .filter(|h| h.to_lowercase().contains(&needle))
            .collect();
        match matches.as_slice() {
            [only] => Ok((*only).clone()),
            [] => {
                bail!("no stored household matches {query:?}. Run `x2rock accounts` to see them.")
            }
            several => bail!(
                "{query:?} matches {} stored households: {}. Give more of the id.",
                several.len(),
                several
                    .iter()
                    .map(|h| h.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Forget a service everywhere it is held. Returns how many households it
    /// was dropped from, so `unlink` can say what it did.
    pub fn forget_everywhere(&mut self, service_id: &str) -> usize {
        let households: Vec<String> = self
            .households
            .iter()
            .filter(|(_, s)| s.contains_key(service_id))
            .map(|(hh, _)| hh.clone())
            .collect();
        for hh in &households {
            self.forget(hh, service_id);
        }
        households.len()
    }
}

/// Build an [`Account`] from a fresh `getDeviceAuthToken` reply. The service
/// id is not part of it - that is the key the caller files it under.
pub fn from_device_auth(
    service_name: &str,
    household: Option<&str>,
    nickname: Option<&str>,
    auth: DeviceAuth,
) -> Account {
    Account {
        service_name: service_name.to_string(),
        auth_token: auth.auth_token,
        private_key: auth.private_key,
        user_id_hash_code: auth.user_id_hash_code,
        nickname: nickname.map(str::to_string),
        household: household.map(str::to_string),
        account_id: None,
        linked: now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;

    const HH: &str = "Sonos_house";

    fn account(name: &str) -> Account {
        Account {
            service_name: name.into(),
            auth_token: "tok".into(),
            private_key: "key".into(),
            user_id_hash_code: Some("hash".into()),
            nickname: Some("nick".into()),
            household: Some(HH.into()),
            account_id: Some("42".into()),
            linked: 1_000,
        }
    }

    #[test]
    fn a_saved_token_is_readable_only_by_its_owner() {
        let dir = TempDir::new("cred-mode");
        let path = dir.path().join("credentials.json");
        let mut creds = Credentials::default();
        creds.remember(HH, "200", account("Bandcamp"));
        creds.save_to(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");

        let back = Credentials::load_from(&path).unwrap();
        assert_eq!(back.schema, SCHEMA);
        let got = back.get(HH, "200").unwrap();
        assert_eq!(got.auth_token, "tok");
        assert_eq!(got.private_key, "key");
        assert_eq!(got.household.as_deref(), Some("Sonos_house"));
    }

    #[test]
    fn a_loose_file_is_tightened_when_it_is_read() {
        let dir = TempDir::new("cred-loose");
        let path = dir.path().join("credentials.json");
        Credentials::default().save_to(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        Credentials::load_from(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");
    }

    #[test]
    fn a_missing_file_is_no_accounts_and_a_corrupt_one_is_an_error() {
        let gone = TempDir::new("cred-missing");
        let missing = gone.path().join("nothing-here.json");
        assert!(Credentials::load_from(&missing).unwrap().is_empty());

        let dir = TempDir::new("cred-corrupt");
        let path = dir.path().join("credentials.json");
        fs::write(&path, "{ not json").unwrap();
        // Unlike the service catalogue: this cannot be refetched in a second.
        assert!(Credentials::load_from(&path).is_err());
    }

    #[test]
    fn an_old_flat_file_is_refused_rather_than_read_as_no_accounts() {
        let dir = TempDir::new("cred-schema");
        let path = dir.path().join("credentials.json");
        // Schema 1: tokens under a flat `services` map, which this build's
        // `households` field cannot see. Loading it as an empty store would
        // report "nothing linked" and then overwrite the only copy.
        fs::write(
            &path,
            r#"{"schema":1,"services":{"200":{"service_name":"Bandcamp",
               "auth_token":"tok","private_key":"key","linked":1000}}}"#,
        )
        .unwrap();

        let err = Credentials::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("schema 1"), "{err}");
        assert!(err.contains("--from-household"), "{err}");
        // And the file it refused is still there to re-import or delete.
        assert!(path.exists());

        // A file from a newer build is refused for the mirror-image reason.
        fs::write(&path, r#"{"schema":99,"households":{}}"#).unwrap();
        let err = Credentials::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("newer x2rock"), "{err}");
    }

    #[test]
    fn relinking_replaces_the_secrets_and_keeps_the_registration() {
        let mut creds = Credentials::default();
        creds.remember(HH, "200", account("Bandcamp"));

        let mut fresh = account("Bandcamp");
        fresh.auth_token = "newtok".into();
        fresh.private_key = "newkey".into();
        fresh.nickname = None;
        fresh.user_id_hash_code = None;
        fresh.account_id = None;
        creds.remember(HH, "200", fresh);

        let got = creds.get(HH, "200").unwrap();
        assert_eq!(got.auth_token, "newtok", "the new secret always wins");
        assert_eq!(got.private_key, "newkey");
        assert_eq!(got.nickname.as_deref(), Some("nick"), "kept, not blanked");
        assert_eq!(got.user_id_hash_code.as_deref(), Some("hash"));
        assert_eq!(got.account_id.as_deref(), Some("42"), "same household");
    }

    #[test]
    fn the_same_service_in_two_households_is_two_independent_accounts() {
        let mut creds = Credentials::default();
        creds.remember(HH, "31", account("Qobuz"));

        let mut other = account("Qobuz");
        other.auth_token = "office-tok".into();
        other.account_id = Some("sn_9".into());
        creds.remember("Sonos_office", "31", other);

        // Neither clobbered the other; each household keeps its own token.
        assert_eq!(creds.get(HH, "31").unwrap().auth_token, "tok");
        assert_eq!(
            creds.get("Sonos_office", "31").unwrap().auth_token,
            "office-tok"
        );
        assert_eq!(creds.all().count(), 2);
    }

    #[test]
    fn forgetting_says_whether_there_was_anything_to_forget() {
        let mut creds = Credentials::default();
        creds.remember(HH, "200", account("Bandcamp"));
        assert!(creds.forget(HH, "200").is_some());
        assert!(creds.forget(HH, "200").is_none());
        // The now-empty household is dropped, so the store is empty again.
        assert!(creds.is_empty());
    }

    #[test]
    fn a_household_can_be_wiped_without_touching_the_others() {
        let mut creds = Credentials::default();
        creds.remember("Sonos_home", "31", account("Qobuz"));
        creds.remember("Sonos_office", "31", account("Qobuz"));
        creds.remember("Sonos_office", "164", account("Saavn"));

        assert_eq!(creds.forget_household("Sonos_office"), 2);
        assert!(creds.get("Sonos_office", "31").is_none());
        // Home is untouched.
        assert!(creds.get("Sonos_home", "31").is_some());
    }

    #[test]
    fn a_household_resolves_by_id_or_unique_fragment() {
        let mut creds = Credentials::default();
        creds.remember("Sonos_HomeAbc", "31", account("Qobuz"));
        creds.remember("Sonos_OfficeXyz", "31", account("Qobuz"));

        assert_eq!(
            creds.resolve_household("Sonos_HomeAbc").unwrap(),
            "Sonos_HomeAbc"
        );
        // A distinctive fragment, case-insensitive.
        assert_eq!(
            creds.resolve_household("office").unwrap(),
            "Sonos_OfficeXyz"
        );
        // "sonos_" is in both.
        assert!(
            creds
                .resolve_household("sonos_")
                .unwrap_err()
                .to_string()
                .contains("matches 2")
        );
        assert!(creds.resolve_household("nowhere").is_err());
    }

    #[test]
    fn forget_everywhere_drops_a_service_from_every_household() {
        let mut creds = Credentials::default();
        creds.remember(HH, "31", account("Qobuz"));
        creds.remember("Sonos_office", "31", account("Qobuz"));
        creds.remember(HH, "200", account("Bandcamp"));

        assert_eq!(creds.forget_everywhere("31"), 2);
        assert!(creds.get(HH, "31").is_none());
        assert!(creds.get("Sonos_office", "31").is_none());
        // Bandcamp untouched.
        assert!(creds.get(HH, "200").is_some());
    }

    #[test]
    fn find_service_id_matches_by_id_name_and_prefix_across_households() {
        let mut creds = Credentials::default();
        creds.remember(HH, "200", account("Bandcamp"));
        // In a different household, so the search has to span them.
        creds.remember("Sonos_office", "284", account("YouTube Music"));

        assert_eq!(creds.find_service_id("200").unwrap().1, "Bandcamp");
        assert_eq!(creds.find_service_id("bandcamp").unwrap().0, "200");
        assert_eq!(creds.find_service_id("youtube music").unwrap().0, "284");
        assert_eq!(creds.find_service_id("band").unwrap().0, "200");
        assert!(creds.find_service_id("Spotify").is_err());

        // A prefix matching two is refused, and names both.
        creds.remember(HH, "285", account("YouTube"));
        let ambiguous = creds.find_service_id("you").unwrap_err().to_string();
        assert!(ambiguous.contains("YouTube Music (id 284)"), "{ambiguous}");
        assert!(ambiguous.contains("YouTube (id 285)"), "{ambiguous}");
        // The whole name still resolves, though it is a prefix of the other.
        assert_eq!(creds.find_service_id("YouTube").unwrap().0, "285");
    }
}
