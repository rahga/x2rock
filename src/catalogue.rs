//! The music service catalogue, cached on disk.
//!
//! Reading it fresh costs three round trips before a single search happens: the
//! descriptor list from a player, then a manifest and a presentation map from
//! Sonos's CDN. That is fine once and wrong every keystroke, and the widget will
//! be invoking `x2rock search` as a subprocess.
//!
//! Cached under `$XDG_STATE_HOME/x2rock/` alongside the player list, for the same
//! reason: regenerable, machine-discovered, not something anyone wrote.
//!
//! Invalidation is not a guessed expiry. `ListAvailableServices` returns an
//! `AvailableServiceListVersion` in the same reply as the descriptors - the same
//! number `musicServiceAccounts:1` reports as `availableServicesVersion` when the
//! set changes - so the cheap LAN call settles whether the expensive internet ones
//! can be skipped.
//!
//! **A cache is only worth having if it survives the thing it protects against.**
//! With no route to a player or no route to the internet, a stale catalogue is
//! served and a line goes to stderr. Listing services and categories then keeps
//! working offline; only the query itself fails. See "Rule: search never enters
//! the daemon" in docs/architecture.md.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::credentials::Credentials;
use crate::sonos::smapi::{self, Category, Service};
use crate::sonos::upnp::Upnp;

/// Bumped whenever the *shape* of what is cached changes.
///
/// The player's `AvailableServiceListVersion` says whether the catalogue moved;
/// it says nothing about whether this program still reads it the same way. A
/// field added to `Service` deserializes as `None` from an older file forever,
/// because the version matches and nothing refetches - which is exactly how
/// `service_type` came back empty and made every cdudn underivable. Keying on
/// both is the fix.
///
/// 2: `Auth` stopped being Anonymous-or-Linked and became Anonymous, DeviceLink
/// or AppLink, so every cached `"auth":"Linked"` is unreadable - and a service
/// wrongly filed as unusable is exactly what linking exists to fix.
const SCHEMA: u32 = 2;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalogue {
    /// The shape this file was written in. Absent, and therefore 0, in anything
    /// written before this field existed - which is what makes it work.
    #[serde(default)]
    schema: u32,
    /// `AvailableServiceListVersion`, verbatim. Empty when nothing is cached.
    #[serde(default)]
    version: String,
    #[serde(default)]
    services: Vec<Service>,
    /// Service id -> its searchable categories. Cleared whenever `version` moves,
    /// because a service's presentation map can change without its id doing so.
    #[serde(default)]
    categories: BTreeMap<String, Vec<Category>>,
}

fn path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    let dir = dirs
        .state_dir()
        .ok_or_else(|| anyhow!("no XDG state directory on this platform"))?;
    Ok(dir.join("services.json"))
}

impl Catalogue {
    /// Load, treating a missing *or unreadable* file as empty.
    ///
    /// Unlike the player list, a corrupt catalogue is not worth failing over: it
    /// is wholly regenerable from the network, and refusing to search because a
    /// cache file is malformed would be the cache causing the outage it exists
    /// to prevent.
    pub fn load() -> Self {
        path()
            .and_then(|p| Ok(fs::read_to_string(p)?))
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            // A file this program no longer understands is treated as absent
            // rather than read half-heartedly.
            .filter(|c| c.schema == SCHEMA)
            .unwrap_or_default()
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

    /// Every service the player knows about, from cache where the version agrees.
    ///
    /// `refresh` forces the descriptors to be re-parsed and every cached category
    /// dropped, for when a service has changed under a version that did not move.
    /// Returns whether anything changed, so the caller can skip writing.
    pub async fn refresh(&mut self, upnp: &Upnp, force: bool) -> Result<bool> {
        let answer = match upnp.list_services().await {
            Ok(answer) => answer,
            Err(e) if !self.services.is_empty() => {
                // The player is the cheap half and it is on the LAN, so failing
                // here usually means the whole household is unreachable - in
                // which case the search will fail too, but listing what *can* be
                // searched should still work.
                eprintln!("x2rock: using the cached service list ({e:#})");
                return Ok(false);
            }
            Err(e) => return Err(e),
        };

        if !force && answer.version == self.version && !self.services.is_empty() {
            return Ok(false);
        }
        self.services = smapi::parse_services(&answer.descriptors, &answer.types)?;
        self.version = answer.version;
        self.schema = SCHEMA;
        // A category list belongs to a version of the catalogue, not to a
        // service id, so they all go when the version moves.
        self.categories.clear();
        Ok(true)
    }

    pub fn services(&self) -> &[Service] {
        &self.services
    }

    /// The services that can actually be searched: the anonymous ones, plus any
    /// this machine holds a token for.
    ///
    /// Takes the credentials rather than reading them itself so that the two
    /// stores stay independent - the catalogue is a cache and this is a secret,
    /// and only the caller has reason to hold both.
    pub fn searchable<'a>(&'a self, linked: &Credentials) -> Vec<&'a Service> {
        self.services
            .iter()
            .filter(|s| s.auth == smapi::Auth::Anonymous || linked.get(&s.id).is_some())
            .collect()
    }

    /// The services `x2rock link` could get a credential for, whether or not it
    /// already has one. `AppLink` is excluded because nothing can drive it.
    pub fn linkable(&self) -> Vec<&Service> {
        self.services
            .iter()
            .filter(|s| s.auth == smapi::Auth::DeviceLink)
            .collect()
    }

    /// A service by name across the *whole* catalogue, for commands that are not
    /// searching - linking one, or explaining why a name cannot be searched.
    pub fn find_any(&self, query: &str) -> Result<&Service> {
        let all: Vec<&Service> = self.services.iter().collect();
        Self::find(&all, query)
    }

    /// A service by name: exact match first, then unique prefix.
    ///
    /// Prefix matching is restricted to a *unique* prefix on purpose. "radio"
    /// matches a dozen services here, and silently searching whichever sorted
    /// first would be worse than saying so.
    pub fn find<'a>(candidates: &[&'a Service], query: &str) -> Result<&'a Service> {
        let needle = query.to_lowercase();
        if let Some(exact) = candidates.iter().find(|s| s.name.to_lowercase() == needle) {
            return Ok(exact);
        }
        let matches: Vec<_> = candidates
            .iter()
            .filter(|s| s.name.to_lowercase().starts_with(&needle))
            .collect();
        match matches.as_slice() {
            [only] => Ok(only),
            [] => Err(anyhow!(
                "no searchable service matching {query:?}. Run `x2rock search` to list them."
            )),
            several => {
                let names: Vec<_> = several.iter().map(|s| s.name.as_str()).collect();
                Err(anyhow!(
                    "{query:?} matches {} services: {}",
                    several.len(),
                    names.join(", ")
                ))
            }
        }
    }

    /// A service's searchable categories, fetching only on a cache miss.
    ///
    /// This is the pair of internet round trips the cache exists to avoid, so it
    /// is the one place worth being careful: a hit costs nothing, and a miss
    /// during an outage falls back to whatever was cached rather than failing.
    pub async fn categories_for(&mut self, service: &Service) -> Result<Vec<Category>> {
        if let Some(hit) = self.categories.get(&service.id) {
            return Ok(hit.clone());
        }
        match smapi::categories(service).await {
            Ok(fetched) => {
                self.categories.insert(service.id.clone(), fetched.clone());
                Ok(fetched)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(id: &str, name: &str, auth: smapi::Auth) -> Service {
        Service {
            id: id.into(),
            name: name.into(),
            uri: "https://example.test/x".into(),
            auth,
            manifest_uri: None,
            service_type: None,
        }
    }

    #[test]
    fn a_name_resolves_by_exact_match_then_by_unique_prefix() {
        let all = [
            service("1", "TuneIn", smapi::Auth::Anonymous),
            service("2", "Radio Javan", smapi::Auth::Anonymous),
            service("3", "Radio Paradise", smapi::Auth::Anonymous),
        ];
        let refs: Vec<&Service> = all.iter().collect();

        assert_eq!(Catalogue::find(&refs, "tunein").unwrap().id, "1");
        assert_eq!(Catalogue::find(&refs, "TUNE").unwrap().id, "1");
        // An ambiguous prefix names the alternatives rather than picking one.
        let err = Catalogue::find(&refs, "radio").unwrap_err().to_string();
        assert!(
            err.contains("Radio Javan") && err.contains("Radio Paradise"),
            "{err}"
        );
        assert!(Catalogue::find(&refs, "spotify").is_err());
    }

    #[test]
    fn an_exact_match_beats_a_prefix_that_would_be_ambiguous() {
        // "Radio" as a real service name must resolve, even though it prefixes
        // the other two.
        let all = [
            service("1", "Radio", smapi::Auth::Anonymous),
            service("2", "Radio Javan", smapi::Auth::Anonymous),
        ];
        let refs: Vec<&Service> = all.iter().collect();
        assert_eq!(Catalogue::find(&refs, "radio").unwrap().id, "1");
    }

    fn three_tiers() -> Catalogue {
        Catalogue {
            schema: SCHEMA,
            version: "v1".into(),
            services: vec![
                service("1", "TuneIn", smapi::Auth::Anonymous),
                service("2", "YouTube Music", smapi::Auth::AppLink),
                service("200", "Bandcamp", smapi::Auth::DeviceLink),
            ],
            categories: BTreeMap::new(),
        }
    }

    fn linked(service_id: &str) -> Credentials {
        let mut creds = Credentials::default();
        creds.remember(
            service_id,
            crate::credentials::Account {
                service_name: "Bandcamp".into(),
                auth_token: "tok".into(),
                private_key: "key".into(),
                user_id_hash_code: None,
                nickname: None,
                household: None,
                account_id: None,
                linked: 1,
            },
        );
        creds
    }

    #[test]
    fn searchable_is_the_anonymous_ones_until_something_is_linked() {
        let cat = three_tiers();
        let usable = cat.searchable(&Credentials::default());
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].name, "TuneIn");
        assert_eq!(cat.services().len(), 3, "the full list is still there");
    }

    #[test]
    fn a_stored_token_makes_its_service_searchable() {
        let cat = three_tiers();
        let names: Vec<&str> = cat
            .searchable(&linked("200"))
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["TuneIn", "Bandcamp"]);
    }

    #[test]
    fn a_token_for_an_app_link_service_is_honoured_too() {
        // Nothing can *mint* one, but if one ever arrives by another route the
        // catalogue should not be the thing standing in the way.
        let cat = three_tiers();
        let names: Vec<&str> = cat
            .searchable(&linked("2"))
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["TuneIn", "YouTube Music"]);
    }

    #[test]
    fn linkable_is_the_device_link_tier_only() {
        let cat = three_tiers();
        let names: Vec<&str> = cat.linkable().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Bandcamp"], "anonymous needs none, app-link cannot");
    }

    #[test]
    fn find_any_reaches_a_service_that_cannot_be_searched_yet() {
        let cat = three_tiers();
        assert_eq!(cat.find_any("bandcamp").unwrap().id, "200");
        assert_eq!(cat.find_any("youtube").unwrap().id, "2");
    }

    #[test]
    fn a_corrupt_cache_file_reads_as_empty_rather_than_failing() {
        // load() has no error path by design; this pins that down.
        let empty: Catalogue = serde_json::from_str("{}").unwrap();
        assert!(empty.services().is_empty());
        assert!(serde_json::from_str::<Catalogue>("not json").is_err());
    }

    #[test]
    fn a_file_from_an_older_shape_is_not_trusted() {
        // What actually happened: a cache written before `service_type` existed
        // kept deserializing cleanly, matched the player's version, and so was
        // never refetched - leaving every cdudn underivable.
        let old = r#"{"version":"RINCON:58","services":[
            {"id":"284","name":"YouTube Music","uri":"https://x","auth":"AppLink",
             "manifest_uri":null}]}"#;
        let parsed: Catalogue = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.schema, 0, "no schema field means schema 0");
        assert_eq!(parsed.services.len(), 1, "it still parses...");
        assert!(
            parsed.services[0].service_type.is_none(),
            "...but incompletely"
        );
        assert_ne!(parsed.schema, SCHEMA, "so load() discards it");
    }
}
