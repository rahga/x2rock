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
//! Keyed by **service id**, not name, because a name in Sonos's catalogue can
//! change under a stable id. The name is kept alongside for display.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::sonos::smapi::{DeviceAuth, Token};

/// The shape of the file. As with bookmarks and *unlike* the catalogue, a
/// mismatch here would be migrated rather than discarded - re-linking means
/// walking a person back through a browser flow.
const SCHEMA: u32 = 1;

/// Everything past owner read/write. A secret with any of these set is a bug
/// somewhere, most likely a hand-edit or a careless copy.
const LOOSE: u32 = 0o177;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub schema: u32,
    /// Service id -> the account linked for it. One account per service: Sonos
    /// allows several, and choosing between them is a feature nothing has asked
    /// for yet.
    #[serde(default)]
    pub services: BTreeMap<String, Account>,
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
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    let dir = dirs
        .state_dir()
        .ok_or_else(|| anyhow!("no XDG state directory on this platform"))?;
    Ok(dir.join("credentials.json"))
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
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
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
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write atomically at 0600.
    ///
    /// The mode is set when the temporary file is *created*, not after it is
    /// written: a `chmod` afterwards leaves a window in which the token exists
    /// on disk at the umask's mercy.
    pub fn save(&self) -> Result<()> {
        let path = path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        let copy = Self {
            schema: SCHEMA,
            services: self.services.clone(),
        };
        let text = serde_json::to_string_pretty(&copy)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    pub fn get(&self, service_id: &str) -> Option<&Account> {
        self.services.get(service_id)
    }

    /// Record a completed link, keeping what the service did not send this time.
    ///
    /// Re-linking a service that is already linked is the repair path - a
    /// revoked or expired token - so the new secrets always win, while the
    /// household registration `match` established is kept unless the new link
    /// names a different household.
    pub fn remember(&mut self, service_id: &str, mut account: Account) {
        if let Some(old) = self.services.get(service_id) {
            account.nickname = account.nickname.or_else(|| old.nickname.clone());
            account.user_id_hash_code = account
                .user_id_hash_code
                .or_else(|| old.user_id_hash_code.clone());
            if account.household == old.household {
                account.account_id = account.account_id.or_else(|| old.account_id.clone());
            }
        }
        self.services.insert(service_id.to_string(), account);
    }

    /// Forget a service's account. Returns what was dropped, so the caller can
    /// name it and say nothing was there when it was not.
    pub fn forget(&mut self, service_id: &str) -> Option<Account> {
        self.services.remove(service_id)
    }
}

/// Build an [`Account`] from a fresh `getDeviceAuthToken` reply.
pub fn from_device_auth(
    service_id: &str,
    service_name: &str,
    household: Option<&str>,
    nickname: Option<&str>,
    auth: DeviceAuth,
) -> (String, Account) {
    (
        service_id.to_string(),
        Account {
            service_name: service_name.to_string(),
            auth_token: auth.auth_token,
            private_key: auth.private_key,
            user_id_hash_code: auth.user_id_hash_code,
            nickname: nickname.map(str::to_string),
            household: household.map(str::to_string),
            account_id: None,
            linked: now(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str) -> Account {
        Account {
            service_name: name.into(),
            auth_token: "tok".into(),
            private_key: "key".into(),
            user_id_hash_code: Some("hash".into()),
            nickname: Some("nick".into()),
            household: Some("Sonos_house".into()),
            account_id: Some("42".into()),
            linked: 1_000,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("x2rock-cred-test-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("credentials.json")
    }

    #[test]
    fn a_saved_token_is_readable_only_by_its_owner() {
        let path = scratch("mode");
        let mut creds = Credentials::default();
        creds.remember("200", account("Bandcamp"));
        creds.save_to(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");

        let back = Credentials::load_from(&path).unwrap();
        assert_eq!(back.schema, SCHEMA);
        let got = back.get("200").unwrap();
        assert_eq!(got.auth_token, "tok");
        assert_eq!(got.private_key, "key");
        assert_eq!(got.household.as_deref(), Some("Sonos_house"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn a_loose_file_is_tightened_when_it_is_read() {
        let path = scratch("loose");
        Credentials::default().save_to(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        Credentials::load_from(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_file_is_no_accounts_and_a_corrupt_one_is_an_error() {
        let missing = scratch("missing").with_file_name("nothing-here.json");
        assert!(
            Credentials::load_from(&missing)
                .unwrap()
                .services
                .is_empty()
        );

        let path = scratch("corrupt");
        fs::write(&path, "{ not json").unwrap();
        // Unlike the service catalogue: this cannot be refetched in a second.
        assert!(Credentials::load_from(&path).is_err());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn relinking_replaces_the_secrets_and_keeps_the_registration() {
        let mut creds = Credentials::default();
        creds.remember("200", account("Bandcamp"));

        let mut fresh = account("Bandcamp");
        fresh.auth_token = "newtok".into();
        fresh.private_key = "newkey".into();
        fresh.nickname = None;
        fresh.user_id_hash_code = None;
        fresh.account_id = None;
        creds.remember("200", fresh);

        let got = creds.get("200").unwrap();
        assert_eq!(got.auth_token, "newtok", "the new secret always wins");
        assert_eq!(got.private_key, "newkey");
        assert_eq!(got.nickname.as_deref(), Some("nick"), "kept, not blanked");
        assert_eq!(got.user_id_hash_code.as_deref(), Some("hash"));
        assert_eq!(got.account_id.as_deref(), Some("42"), "same household");
    }

    #[test]
    fn linking_against_a_different_household_drops_the_old_account_id() {
        let mut creds = Credentials::default();
        creds.remember("200", account("Bandcamp"));

        let mut elsewhere = account("Bandcamp");
        elsewhere.household = Some("Sonos_other".into());
        elsewhere.account_id = None;
        creds.remember("200", elsewhere);

        // An account id is the *household's* name for the account, so it means
        // nothing on a different one.
        assert!(creds.get("200").unwrap().account_id.is_none());
    }

    #[test]
    fn forgetting_says_whether_there_was_anything_to_forget() {
        let mut creds = Credentials::default();
        creds.remember("200", account("Bandcamp"));
        assert!(creds.forget("200").is_some());
        assert!(creds.forget("200").is_none());
    }
}
