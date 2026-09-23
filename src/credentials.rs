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

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::sonos::smapi::{DeviceAuth, Token};
use crate::store;

/// The shape of the file. Schema 3 holds *several* accounts per service, since a
/// household can; schema 2 held exactly one; schema 1's flat `services` map is
/// not read back at all.
///
/// Schema 2 **migrates** rather than being refused, and the reason is concrete:
/// a store written at 2 can hold tokens for a household that is nowhere near
/// this machine - the office set, while the laptop is at home - and those cannot
/// be re-imported from here at any price. Schema 1 predates the household key
/// entirely, so there is nothing in it to place, and it is still refused.
///
/// Anything this build does not know is refused too, in both directions: a file
/// read as empty would report every service as unlinked and then be overwritten
/// by the next save, and this file is its own only copy.
const SCHEMA: u32 = 3;

/// The oldest schema this build can still read and convert.
const OLDEST_READABLE: u32 = 2;

/// Everything past owner read/write. A secret with any of these set is a bug
/// somewhere, most likely a hand-edit or a careless copy.
const LOOSE: u32 = 0o177;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub schema: u32,
    /// Household id -> (service id -> the accounts held for it there).
    ///
    /// Keyed by household because one machine sees more than one - a laptop that
    /// moves between home and the office - and each household holds its own
    /// account for a service, with its own token. Keeping them apart is what
    /// lets a search on the home network use the home token while the office
    /// token sits untouched, and what stops an auto-refresh on one from
    /// clobbering the other.
    ///
    /// Plural at the third level because a household really can hold two
    /// accounts for one service - the Sonos app numbers the second in its
    /// nickname, `iHeartRadio 885ebbcc` beside `iHeartRadio` - and collapsing
    /// them lost a token every import. See [`ServiceAccounts`].
    #[serde(default)]
    pub households: BTreeMap<String, BTreeMap<String, ServiceAccounts>>,
}

/// Every account one household holds for one service, and which of them to use.
///
/// The Sonos app's own answer to two accounts for a service is to let a person
/// prioritise one, so that a search comes back with one set of results rather
/// than two interleaved. This is that: the accounts, and the key of the one
/// every ordinary read resolves to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceAccounts {
    /// Account key -> the account. **Not** `#[serde(default)]`, deliberately:
    /// its absence is what tells a schema-2 record (a bare `Account` object) from
    /// a schema-3 one when [`StoredService`] deserializes them untagged.
    pub accounts: BTreeMap<String, Account>,
    /// Which account key search, browse and playback use. `None` means nothing
    /// has been chosen, which is the ordinary case for the single-account
    /// service; [`ServiceAccounts::chosen`] says what that resolves to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred: Option<String>,
}

impl ServiceAccounts {
    /// The account every read resolves to: the preferred one while it is still
    /// there, else the only one, else the first by key.
    ///
    /// The last arm matters more than it looks. Falling back to *insertion*
    /// order would make two accounts with no stated preference drift with write
    /// order - a re-import could quietly change which account a search uses -
    /// so the fallback is the `BTreeMap`'s own order, which is the same on every
    /// machine and every run.
    pub fn chosen(&self) -> Option<(&str, &Account)> {
        if let Some(key) = &self.preferred
            && let Some(account) = self.accounts.get(key)
        {
            return Some((key.as_str(), account));
        }
        self.accounts
            .first_key_value()
            .map(|(k, a)| (k.as_str(), a))
    }

    /// How to name one of these accounts so a person can tell it from its
    /// siblings and type it back at `--prefer` or `--account`.
    ///
    /// A nickname where that is enough, the key where it is not. Both halves
    /// are needed: an account can carry no nickname, and two accounts can carry
    /// the *same* one - a household that rotated a service's token leaves the
    /// old record and the new one both called `Hhh`, and two identical rows are
    /// no more useful than two blank ones.
    pub fn label_for(&self, key: &str) -> String {
        let Some(account) = self.accounts.get(key) else {
            return key.to_string();
        };
        let Some(nick) = account_nickname(account) else {
            return key.to_string();
        };
        if self.nickname_is_unique(key, nick) {
            nick.to_string()
        } else {
            format!("{nick} ({key})")
        }
    }

    /// The shortest thing that tells this account from its siblings: its
    /// nickname where that is unique among them, else the key.
    ///
    /// For the listing, where the service name is already printed beside it and
    /// a repeated nickname would be noise rather than information.
    pub fn distinguisher(&self, key: &str) -> String {
        match self.accounts.get(key).and_then(account_nickname) {
            Some(nick) if self.nickname_is_unique(key, nick) => nick.to_string(),
            _ => key.to_string(),
        }
    }

    fn nickname_is_unique(&self, key: &str, nick: &str) -> bool {
        !self
            .accounts
            .iter()
            .any(|(k, a)| k.as_str() != key && account_nickname(a) == Some(nick))
    }

    /// Whether this key is the one [`chosen`](Self::chosen) would return.
    pub fn is_chosen(&self, key: &str) -> bool {
        self.chosen().is_some_and(|(k, _)| k == key)
    }

    /// The key of an account already holding this exact `authToken`.
    ///
    /// Identity, cheaply: the household's stored blob hands back the very bytes
    /// x2rock filed, so an import that re-reads an account it already has can
    /// land on the record it already wrote instead of beside it under a second
    /// key. Without this, a migrated schema-2 record and its own re-import would
    /// sit side by side as two accounts that are one.
    fn key_holding(&self, auth_token: &str) -> Option<String> {
        self.accounts
            .iter()
            .find(|(_, a)| a.auth_token == auth_token)
            .map(|(k, _)| k.clone())
    }

    /// Where an incoming account belongs among the ones already held.
    ///
    /// Three questions in order, and the last one is the interesting one:
    ///
    /// 1. Is this token already here? Then it is that record, whatever route it
    ///    came by.
    /// 2. Does the account identify itself - a household serial, or a service's
    ///    `userIdHashCode`? Then that is its key, and a genuinely different
    ///    account lands beside this one rather than on it.
    /// 3. Neither. Re-linking through the browser is the repair path for a dead
    ///    token, and a service that sent a hash last time may send none this
    ///    time - so an unidentifiable account replaces the one already held when
    ///    there is exactly one, which is the account it is repairing. With
    ///    *several* held it could be any of them, and guessing would destroy a
    ///    working token to fix a different one: it is filed as its own record
    ///    instead, which loses nothing and can be unlinked.
    fn key_for(&self, account: &Account) -> String {
        if let Some(key) = self.key_holding(&account.auth_token) {
            return key;
        }
        if let Some(key) = identifying_key(account) {
            return key;
        }
        match self.accounts.first_key_value() {
            Some((only, _)) if self.accounts.len() == 1 => only.clone(),
            _ => UNIDENTIFIED.to_string(),
        }
    }
}

/// The key a browser-linked account gets when nothing identifies it. One such
/// account per service per household - which is what the store held for *every*
/// service before it learned the plural, so it is no new limit.
const UNIDENTIFIED: &str = "link";

/// One service's entry as it appears *on disk*, which is two shapes: schema 3's
/// [`ServiceAccounts`] and schema 2's bare [`Account`].
///
/// Untagged, and the discrimination is structural rather than a version tag:
/// `ServiceAccounts` requires `accounts`, which no schema-2 record has, and
/// `Account` requires `auth_token` and friends, which no schema-3 record has at
/// that level. Order still matters - the richer shape is tried first.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StoredService {
    Current(ServiceAccounts),
    Legacy(Box<Account>),
}

/// The file as read, before any conversion. Separate from [`Credentials`] so the
/// in-memory store never carries the legacy shape around.
#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    households: BTreeMap<String, BTreeMap<String, StoredService>>,
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
    /// The household's own serial for this account - the `sn_N` it is known by
    /// there - when it is known. `--from-household` reads it straight off the
    /// stored record (`SerialNum0`); a browser link never sees one unless
    /// `match` comes back with it, which is what `account_id` holds.
    ///
    /// Kept apart from `account_id` on purpose: this one says "the household has
    /// this account", `account_id` says "the household matched the account *this
    /// machine* registered". A record can honestly have the first and not the
    /// second, which is every imported account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<u32>,
    /// When the link completed, epoch seconds - the same unit bookmarks use.
    pub linked: u64,
}

impl Account {
    /// What goes in the SMAPI credentials header.
    ///
    /// `account` is the key this record is filed under, carried along for the
    /// same reason `household` is: a reply that refreshes the token has to be
    /// written back to the account it came from, and with several accounts per
    /// service "the one for this service" is no longer an answer.
    pub fn token(&self, account: &str) -> Token {
        Token {
            token: self.auth_token.clone(),
            key: self.private_key.clone(),
            household: self.household.clone(),
            account: Some(account.to_string()),
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
        let file: CredentialsFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        check_schema(file.schema, path)?;
        Ok(Self::from_file(file))
    }

    /// Convert what was read into the current shape, lifting every schema-2
    /// record into a one-account [`ServiceAccounts`].
    ///
    /// The lifted account keeps its token untouched and takes the key a fresh
    /// write would give it, so a later `link --from-household` that re-reads the
    /// same account lands *on* it (by [`ServiceAccounts::key_holding`]) rather
    /// than beside it. `preferred` is left unset: with one account there is
    /// nothing to prefer, and stating one would outlive the moment a second
    /// arrives.
    fn from_file(file: CredentialsFile) -> Self {
        let households = file
            .households
            .into_iter()
            .map(|(household, services)| {
                let services = services
                    .into_iter()
                    .map(|(id, entry)| {
                        let accounts = match entry {
                            StoredService::Current(accounts) => accounts,
                            StoredService::Legacy(account) => {
                                let mut lifted = ServiceAccounts::default();
                                let key = lifted.key_for(&account);
                                lifted.accounts.insert(key, *account);
                                lifted
                            }
                        };
                        (id, accounts)
                    })
                    .collect();
                (household, services)
            })
            .collect();
        Self {
            schema: SCHEMA,
            households,
        }
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
        // Written at SCHEMA whatever was read, which is how a migrated schema-2
        // file becomes a schema-3 one: the first save after the first load.
        store::write_atomically(path, &serde_json::to_string_pretty(&copy)?, store::SECRET)
    }

    /// The accounts held for a service *in one household*, and which is chosen.
    pub fn accounts_for(&self, household: &str, service_id: &str) -> Option<&ServiceAccounts> {
        self.households.get(household)?.get(service_id)
    }

    /// The one account a read resolves to for this service in this household.
    ///
    /// Every caller that wants "the token for X" goes through here, which is the
    /// whole point of holding the preference inside the store: `search`,
    /// `browse`, `play-item` and the rest never learn that a service can have
    /// two accounts, and there is exactly one place where which-one is decided.
    pub fn get(&self, household: &str, service_id: &str) -> Option<&Account> {
        self.chosen(household, service_id).map(|(_, a)| a)
    }

    /// As [`get`](Self::get), and says which key it landed on.
    pub fn chosen(&self, household: &str, service_id: &str) -> Option<(&str, &Account)> {
        self.accounts_for(household, service_id)?.chosen()
    }

    /// The token held for a service in a household - what every play path hands
    /// SMAPI. The household is the one the caller is currently connected to, so
    /// a machine that moves between systems uses the right account for each, and
    /// the token carries the account key so a refresh comes back to the right
    /// one of them.
    pub fn token_for(&self, household: &str, service_id: &str) -> Option<Token> {
        self.chosen(household, service_id).map(|(key, account)| {
            let mut tok = account.token(key);
            if tok.household.is_none() && !household.is_empty() {
                tok.household = Some(household.to_string());
            }
            tok
        })
    }

    /// Every account, across all households, as `(household, service_id,
    /// account_key, account)`. For `accounts`, which lists what the whole store
    /// holds - every account, not one per service.
    pub fn all(&self) -> impl Iterator<Item = (&str, &str, &str, &Account)> {
        self.households.iter().flat_map(|(hh, services)| {
            services.iter().flat_map(move |(id, held)| {
                held.accounts
                    .iter()
                    .map(move |(key, account)| (hh.as_str(), id.as_str(), key.as_str(), account))
            })
        })
    }

    /// Whether the store holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.all().next().is_none()
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
        for (_, id, _, account) in self.all() {
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
    /// Returns the key it was filed under.
    pub fn remember(&mut self, household: &str, service_id: &str, account: Account) -> String {
        let held = self
            .households
            .entry(household.to_string())
            .or_default()
            .entry(service_id.to_string())
            .or_default();
        let key = held.key_for(&account);
        // A record that arrived with nothing to identify it, and is now being
        // written again by a route that *does* know which account it is, moves
        // to the stable key. This is what a migrated schema-2 record does on
        // its first re-import: it was filed under the fallback because the old
        // file carried no serial, and the import knows one.
        if let Some(stable) = identifying_key(&account)
            && stable != key
            && held.accounts.contains_key(&key)
        {
            if let Some(moved) = held.accounts.remove(&key) {
                held.accounts.insert(stable.clone(), moved);
            }
            if held.preferred.as_deref() == Some(key.as_str()) {
                held.preferred = Some(stable.clone());
            }
            return Self::merge_into(held, stable, account);
        }
        Self::merge_into(held, key, account)
    }

    /// Insert `account` at `key`, keeping what the incoming record does not
    /// carry. The tail of [`remember`](Self::remember), shared with the rekey
    /// path above it.
    fn merge_into(held: &mut ServiceAccounts, key: String, mut account: Account) -> String {
        if let Some(old) = held.accounts.get(&key) {
            account.nickname = account.nickname.or_else(|| old.nickname.clone());
            account.user_id_hash_code = account
                .user_id_hash_code
                .or_else(|| old.user_id_hash_code.clone());
            account.account_id = account.account_id.or_else(|| old.account_id.clone());
            account.serial = account.serial.or(old.serial);
        }
        held.accounts.insert(key.clone(), account);
        key
    }

    /// Prefer one account for a service, so every read resolves to it. The key
    /// must be one this service actually holds; an unknown one is refused rather
    /// than stored, since a preference pointing at nothing reads as no
    /// preference at all and would look like the setting silently failing.
    pub fn prefer(&mut self, household: &str, service_id: &str, key: &str) -> Result<()> {
        let Some(held) = self
            .households
            .get_mut(household)
            .and_then(|s| s.get_mut(service_id))
        else {
            bail!("no account is held for that service in this household");
        };
        ensure!(
            held.accounts.contains_key(key),
            "no account {key:?} is held for that service"
        );
        held.preferred = Some(key.to_string());
        Ok(())
    }

    /// Forget every account a service has in one household. Returns how many
    /// were dropped.
    pub fn forget(&mut self, household: &str, service_id: &str) -> usize {
        let Some(services) = self.households.get_mut(household) else {
            return 0;
        };
        let dropped = services.remove(service_id).map_or(0, |h| h.accounts.len());
        if services.is_empty() {
            self.households.remove(household);
        }
        dropped
    }

    /// Forget one account of a service in one household, leaving its siblings.
    /// Returns what was dropped. A preference pointing at it is cleared with it,
    /// so the next read falls back rather than resolving through a dangling key.
    pub fn forget_account(
        &mut self,
        household: &str,
        service_id: &str,
        key: &str,
    ) -> Option<Account> {
        let services = self.households.get_mut(household)?;
        let held = services.get_mut(service_id)?;
        let dropped = held.accounts.remove(key)?;
        if held.preferred.as_deref() == Some(key) {
            held.preferred = None;
        }
        if held.accounts.is_empty() {
            services.remove(service_id);
        }
        if services.is_empty() {
            self.households.remove(household);
        }
        Some(dropped)
    }

    /// Drop a whole household. Returns how many accounts it held, so a wipe of
    /// one household can say what it cleared.
    pub fn forget_household(&mut self, household: &str) -> usize {
        self.households
            .remove(household)
            .map(|s| s.values().map(|h| h.accounts.len()).sum())
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

    /// Resolve a query to one account key of a service in a household.
    ///
    /// The same ladder the catalogue uses for a service name, over what people
    /// actually see: the account key itself, then an exact nickname, then a
    /// unique case-insensitive nickname prefix. Nickname first among the human
    /// forms because that is what `accounts` prints and what the Sonos app calls
    /// the account; the key is there for the case a nickname cannot settle -
    /// two accounts named the same, which the app permits.
    ///
    /// Ambiguity is refused by naming the candidates. Picking one would move a
    /// preference, or forget a token, that nobody asked about.
    pub fn resolve_account(
        &self,
        household: &str,
        service_id: &str,
        query: &str,
    ) -> Result<String> {
        let held = self
            .accounts_for(household, service_id)
            .ok_or_else(|| anyhow!("no account is held for that service in this household"))?;
        self.try_resolve_account(household, service_id, query)?
            .ok_or_else(|| {
                anyhow!(
                    "no account matching {query:?}. This service holds: {}.",
                    describe(&held.accounts)
                )
            })
    }

    /// As [`resolve_account`](Self::resolve_account), separating "nothing here
    /// matches" from "several do".
    ///
    /// `Ok(None)` is the first, and is not always a failure: `unlink <service>
    /// --account <x>` walks every household holding the service, and a
    /// household that has no account by that name is one to pass over rather
    /// than the end of the command. Ambiguity stays an error wherever it is
    /// found - there is no safe way to pass over *that*, since the point of
    /// refusing is that one of the candidates would otherwise be forgotten
    /// without being named.
    pub fn try_resolve_account(
        &self,
        household: &str,
        service_id: &str,
        query: &str,
    ) -> Result<Option<String>> {
        let Some(held) = self.accounts_for(household, service_id) else {
            return Ok(None);
        };
        // An empty query names nothing, and must not be allowed to *match*
        // something: an account with no nickname reads as the empty string, so
        // `--account ""` would match it exactly, and `starts_with("")` is true
        // of every account there is. Forgetting a token to a flag that was left
        // blank is not a thing to leave reachable.
        if query.trim().is_empty() {
            return Ok(None);
        }
        if held.accounts.contains_key(query) {
            return Ok(Some(query.to_string()));
        }
        let exact: Vec<(&String, &Account)> = held
            .accounts
            .iter()
            .filter(|(_, a)| account_nickname(a).is_some_and(|n| n.eq_ignore_ascii_case(query)))
            .collect();
        match exact.as_slice() {
            [(key, _)] => return Ok(Some((*key).clone())),
            [_, _, ..] => bail!(
                "{query:?} matches {} accounts: {}. Give the key.",
                exact.len(),
                describe_accounts(exact.iter().copied())
            ),
            [] => {}
        }
        let needle = query.to_lowercase();
        let matches: Vec<(&String, &Account)> = held
            .accounts
            .iter()
            .filter(|(_, a)| {
                account_nickname(a).is_some_and(|n| n.to_lowercase().starts_with(&needle))
            })
            .collect();
        match matches.as_slice() {
            [(key, _)] => Ok(Some((*key).clone())),
            [] => Ok(None),
            several => bail!(
                "{query:?} matches {} accounts: {}. Give the whole nickname, or the key.",
                several.len(),
                describe_accounts(several.iter().copied())
            ),
        }
    }
}

/// An account's nickname, or `None` where it has none *usable* - absent and
/// present-but-empty are the same thing to everything that reads one, and
/// treating them apart is how the empty string became a matchable name.
fn account_nickname(a: &Account) -> Option<&str> {
    a.nickname.as_deref().filter(|n| !n.is_empty())
}

/// What to call an account in output: its nickname where it has one, else the
/// key, which is always something a person can type back.
pub fn account_display(nickname: Option<&str>, key: &str) -> String {
    match nickname {
        Some(nick) if !nick.is_empty() => nick.to_string(),
        _ => key.to_string(),
    }
}

/// Name accounts the way an error should: nickname and key.
fn describe_accounts<'a>(accounts: impl IntoIterator<Item = (&'a String, &'a Account)>) -> String {
    accounts
        .into_iter()
        .map(|(key, a)| match account_nickname(a) {
            Some(nick) => format!("{nick} ({key})"),
            None => key.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Name every account of a service the way an error should: nickname and key.
fn describe(accounts: &BTreeMap<String, Account>) -> String {
    describe_accounts(accounts)
}

/// What an account says about which account it is, if anything:
///
/// 1. `serial`, the household's own `sn_N` for it - what `--from-household`
///    reads off the stored record, and the same namespace `match` answers in,
///    so the two routes to one account agree on a key.
/// 2. `user_id_hash_code`, which a device link gets back from the service and a
///    household import never carries.
///
/// `None` when it says neither, which is a browser-linked account for a service
/// that sent no hash. [`ServiceAccounts::key_for`] decides what to do then.
fn identifying_key(account: &Account) -> Option<String> {
    if let Some(serial) = account.serial {
        return Some(format!("sn{serial}"));
    }
    match account.user_id_hash_code.as_deref() {
        Some(hash) if !hash.is_empty() => Some(hash.to_string()),
        _ => None,
    }
}

/// Refuse a file this build cannot round-trip, and say which way out applies.
///
/// Serde ignores unknown fields, so a schema-1 file - tokens under a flat
/// `services` map - parses happily into zero households. Left there it would
/// present itself as "nothing linked", send someone back through a link flow,
/// and then be overwritten by the first save that followed, taking the only copy
/// of those tokens with it. The same holds in reverse for a file from a newer
/// build: whatever it keeps that this one cannot read would not survive the
/// round trip.
///
/// Schema 2 is *not* refused - it is lifted by [`Credentials::from_file`]. It
/// has to be: a schema-2 store can hold a household this machine is nowhere
/// near, and `link --from-household` cannot re-import what it cannot reach.
fn check_schema(schema: u32, path: &Path) -> Result<()> {
    if (OLDEST_READABLE..=SCHEMA).contains(&schema) {
        return Ok(());
    }
    let path = path.display();
    if schema < OLDEST_READABLE {
        bail!(
            "{path} is schema {schema} - an older x2rock keyed tokens by service alone, \
             with no household. The tokens are still valid at their services: re-import \
             them with `x2rock link --from-household`, or delete the file to start over."
        );
    }
    bail!(
        "{path} is schema {schema}, written by a newer x2rock than this one. Upgrade, or \
         move the file aside - saving over it from here would drop whatever this build \
         cannot read."
    )
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
        // A device link never learns the household's serial for the account;
        // only `match`, later, says anything about it, and that lands in
        // `account_id`. `--from-household` is the path that reads one.
        serial: None,
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
            serial: None,
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

    /// One service's two accounts, the way a household hands them over.
    fn imported(nick: &str, serial: u32, token: &str) -> Account {
        Account {
            service_name: "iHeartRadio".into(),
            auth_token: token.into(),
            private_key: String::new(),
            user_id_hash_code: None,
            nickname: Some(nick.into()),
            household: Some(HH.into()),
            account_id: None,
            serial: Some(serial),
            linked: 1_000,
        }
    }

    #[test]
    fn two_accounts_for_one_service_are_both_kept() {
        let mut creds = Credentials::default();
        creds.remember(HH, "6", imported("iHeartRadio", 24, "tok-a"));
        creds.remember(HH, "6", imported("iHeartRadio 885ebbcc", 25, "tok-b"));

        let held = creds.accounts_for(HH, "6").unwrap();
        assert_eq!(held.accounts.len(), 2, "the second did not overwrite");
        assert_eq!(creds.all().count(), 2);
        // With nothing preferred it is the first by key, not by write order.
        assert_eq!(creds.get(HH, "6").unwrap().auth_token, "tok-a");
    }

    #[test]
    fn re_reading_an_account_lands_on_the_record_it_already_wrote() {
        let mut creds = Credentials::default();
        let key = creds.remember(HH, "6", imported("iHeartRadio", 24, "tok-a"));
        // The same token arriving with nothing to identify it - a migrated
        // record's own re-import - must not become a second account.
        let mut anonymous = imported("iHeartRadio", 24, "tok-a");
        anonymous.serial = None;
        let again = creds.remember(HH, "6", anonymous);
        assert_eq!(again, key, "matched on the token it already holds");
        assert_eq!(creds.accounts_for(HH, "6").unwrap().accounts.len(), 1);
    }

    #[test]
    fn the_preferred_account_is_what_every_read_resolves_to() {
        let mut creds = Credentials::default();
        creds.remember(HH, "6", imported("iHeartRadio", 24, "tok-a"));
        creds.remember(HH, "6", imported("iHeartRadio 885ebbcc", 25, "tok-b"));

        let key = creds
            .resolve_account(HH, "6", "iHeartRadio 885ebbcc")
            .unwrap();
        creds.prefer(HH, "6", &key).unwrap();
        assert_eq!(creds.get(HH, "6").unwrap().auth_token, "tok-b");
        // And the token carries the key, so a refresh knows where to land.
        let token = creds.token_for(HH, "6").unwrap();
        assert_eq!(token.account.as_deref(), Some(key.as_str()));

        // A preference pointing at an account that has gone falls back rather
        // than resolving to nothing - losing search is worse than a fallback.
        creds.forget_account(HH, "6", &key).unwrap();
        assert_eq!(creds.get(HH, "6").unwrap().auth_token, "tok-a");
    }

    #[test]
    fn forgetting_one_account_leaves_its_sibling() {
        let mut creds = Credentials::default();
        creds.remember(HH, "6", imported("iHeartRadio", 24, "tok-a"));
        creds.remember(HH, "6", imported("iHeartRadio 885ebbcc", 25, "tok-b"));

        let key = creds
            .resolve_account(HH, "6", "iHeartRadio 885ebbcc")
            .unwrap();
        let gone = creds.forget_account(HH, "6", &key).unwrap();
        assert_eq!(gone.auth_token, "tok-b");
        assert_eq!(creds.accounts_for(HH, "6").unwrap().accounts.len(), 1);
        // And forgetting the service drops what is left, counted in accounts.
        assert_eq!(creds.forget(HH, "6"), 1);
        assert!(creds.is_empty());
    }

    #[test]
    fn an_account_resolves_by_nickname_prefix_and_refuses_an_ambiguous_one() {
        let mut creds = Credentials::default();
        creds.remember(HH, "6", imported("iHeartRadio", 24, "tok-a"));
        creds.remember(HH, "6", imported("iHeartRadio 885ebbcc", 25, "tok-b"));

        // Exact beats prefix: "iHeartRadio" is also a prefix of the other one.
        let exact = creds.resolve_account(HH, "6", "iHeartRadio").unwrap();
        assert_eq!(
            creds.accounts_for(HH, "6").unwrap().accounts[&exact].auth_token,
            "tok-a"
        );
        // A distinguishing prefix resolves.
        assert!(creds.resolve_account(HH, "6", "iHeartRadio 88").is_ok());
        // The key itself always works, for two accounts named the same.
        assert!(creds.resolve_account(HH, "6", "sn25").is_ok());
        // Ambiguity names the candidates instead of picking one.
        let err = creds
            .resolve_account(HH, "6", "iheart")
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 accounts"), "{err}");
        assert!(err.contains("sn24") && err.contains("sn25"), "{err}");
    }

    #[test]
    fn a_household_without_the_named_account_is_passed_over_not_fatal() {
        // What `unlink <service> --account <x>` walks into: two households hold
        // the service, and the nickname names an account in only one of them.
        const OTHER: &str = "Sonos_office";
        let mut creds = Credentials::default();
        creds.remember(HH, "6", imported("Kids", 24, "tok-kids"));
        creds.remember(OTHER, "6", imported("Main", 99, "tok-main"));

        // Nothing here matches - reported as "nothing", not as an error, so a
        // caller sweeping households can carry on to the one that does.
        assert_eq!(creds.try_resolve_account(OTHER, "6", "Kids").unwrap(), None);
        // A household that holds no such service at all answers the same way.
        assert_eq!(
            creds.try_resolve_account(OTHER, "999", "Kids").unwrap(),
            None
        );
        // And where it does match, it resolves.
        let key = creds.try_resolve_account(HH, "6", "Kids").unwrap();
        assert_eq!(key.as_deref(), Some("sn24"));

        // Ambiguity is still an error in the permissive form: passing over it
        // would forget an account nobody named.
        creds.remember(HH, "6", imported("Kids B", 25, "tok-kids-b"));
        // "Kids" would still resolve - an exact nickname beats a prefix, and
        // it is one. "Kid" is the query that matches both and equals neither.
        assert_eq!(
            creds
                .try_resolve_account(HH, "6", "kids")
                .unwrap()
                .as_deref(),
            Some("sn24"),
            "an exact nickname is not ambiguous just because it prefixes another"
        );
        assert!(creds.try_resolve_account(HH, "6", "Kid").is_err());

        // The strict form still names what is held when nothing matches.
        let err = creds
            .resolve_account(OTHER, "6", "Kids")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no account matching"), "{err}");
        assert!(err.contains("Main"), "{err}");
    }

    #[test]
    fn identical_nicknames_refuse_ambiguous_query_and_require_key() {
        let mut creds = Credentials::default();
        creds.remember(HH, "6", imported("Kids", 24, "tok-a"));
        creds.remember(HH, "6", imported("Kids", 25, "tok-b"));

        // Two accounts named identically: querying the nickname is ambiguous.
        let err = creds
            .resolve_account(HH, "6", "Kids")
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 accounts"), "{err}");
        assert!(
            err.contains("Kids (sn24)") && err.contains("Kids (sn25)"),
            "{err}"
        );
        assert!(err.contains("Give the key"), "{err}");

        // The key settles it.
        assert_eq!(creds.resolve_account(HH, "6", "sn24").unwrap(), "sn24");
        assert_eq!(creds.resolve_account(HH, "6", "sn25").unwrap(), "sn25");
    }

    #[test]
    fn token_for_attaches_household_when_account_omits_it() {
        let mut creds = Credentials::default();
        let mut acct = account("Deezer");
        acct.household = None;
        creds.remember(HH, "2", acct);

        let tok = creds.token_for(HH, "2").expect("token should be held");
        assert_eq!(tok.household.as_deref(), Some(HH));
    }

    #[test]
    fn a_blank_query_names_no_account() {
        let mut creds = Credentials::default();
        let mut no_nickname = imported("", 24, "tok-a");
        no_nickname.nickname = None;
        creds.remember(HH, "6", no_nickname);

        // An account with no nickname reads as the empty string, so a blank
        // `--account` would otherwise match it exactly and forget it. One
        // account held, so nothing is ambiguous and nothing protects it but
        // this.
        assert_eq!(creds.try_resolve_account(HH, "6", "").unwrap(), None);
        assert_eq!(creds.try_resolve_account(HH, "6", "   ").unwrap(), None);
        // A present-but-empty nickname is the same as none.
        creds.remember(HH, "6", imported("", 25, "tok-b"));
        assert_eq!(creds.try_resolve_account(HH, "6", "").unwrap(), None);
        // And such an account is still reachable by its key.
        assert_eq!(creds.resolve_account(HH, "6", "sn25").unwrap(), "sn25");
        // The error names them by key, since they have no other name.
        let err = creds
            .resolve_account(HH, "6", "nothing")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sn24") && err.contains("sn25"), "{err}");
    }

    #[test]
    fn two_accounts_sharing_a_nickname_are_still_told_apart() {
        // A household that rotated a service's token leaves the record written
        // before it and the one written after, both carrying the nickname the
        // app gave that service. Observed on a real household: two YouTube
        // Music records, both `Hhh`.
        // In the order it really happened: the unidentified record first (a
        // schema-2 row, lifted with no serial), then the import that knows one.
        // The reverse order is a *re-link* of the one account held, which
        // replaces it - see `key_for`.
        let mut creds = Credentials::default();
        let mut older = imported("Hhh", 15, "old-token");
        older.serial = None;
        creds.remember(HH, "284", older);
        creds.remember(HH, "284", imported("Hhh", 15, "new-token"));

        let held = creds.accounts_for(HH, "284").unwrap();
        assert_eq!(held.accounts.len(), 2, "different tokens, kept apart");
        // Neither is nameable by the nickname alone, so both carry their key.
        let labels: Vec<String> = held.accounts.keys().map(|k| held.label_for(k)).collect();
        assert_eq!(labels, vec!["Hhh (link)", "Hhh (sn15)"]);
        // The listing drops the repeated nickname: it is already beside the
        // service name, and saying it twice distinguishes nothing.
        assert_eq!(held.distinguisher("link"), "link");
        assert_eq!(held.distinguisher("sn15"), "sn15");
        // And the nickname is refused as a selector, naming both.
        let err = creds
            .resolve_account(HH, "284", "Hhh")
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 accounts"), "{err}");

        // Where a nickname *is* unique it stands alone, with no key noise.
        creds.remember(HH, "6", imported("Kids", 24, "k"));
        creds.remember(HH, "6", imported("Grown-ups", 25, "g"));
        let held = creds.accounts_for(HH, "6").unwrap();
        assert_eq!(held.label_for("sn24"), "Kids");
        assert_eq!(held.distinguisher("sn25"), "Grown-ups");
    }

    #[test]
    fn a_schema_two_file_is_lifted_rather_than_refused() {
        let dir = TempDir::new("cred-migrate");
        let path = dir.path().join("credentials.json");
        // Schema 2: one Account per service, no `accounts` map. This must
        // survive - a store written at 2 can hold a household nowhere near this
        // machine, which `link --from-household` cannot re-import from here.
        fs::write(
            &path,
            r#"{"schema":2,"households":{"Sonos_office":{"31":{"service_name":"Qobuz",
               "auth_token":"office-tok","private_key":"office-key","nickname":"Qb1",
               "household":"Sonos_office","linked":1000}}}}"#,
        )
        .unwrap();

        let creds = Credentials::load_from(&path).unwrap();
        let got = creds.get("Sonos_office", "31").expect("the token survived");
        assert_eq!(got.auth_token, "office-tok");
        assert_eq!(got.private_key, "office-key");
        let held = creds.accounts_for("Sonos_office", "31").unwrap();
        assert_eq!(held.accounts.len(), 1);
        assert!(held.preferred.is_none(), "one account, nothing to prefer");

        // And it is written back at the current schema, readable again.
        creds.save_to(&path).unwrap();
        let back = Credentials::load_from(&path).unwrap();
        assert_eq!(back.schema, SCHEMA);
        assert_eq!(
            back.get("Sonos_office", "31").unwrap().auth_token,
            "office-tok"
        );
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
        assert_eq!(creds.forget(HH, "200"), 1);
        assert_eq!(creds.forget(HH, "200"), 0);
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
