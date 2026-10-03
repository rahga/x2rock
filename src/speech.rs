//! Text to speech: a provider, its key, and a clip of audio. Nothing here knows
//! what a speaker is.
//!
//! `x2rock say` turns a sentence into a clip and hands the clip to a room the
//! way `notify` does (see `clipserve.rs` for the handing). This module is the
//! first half: which service makes the audio, with what credential, and in what
//! voice. It is confined to the CLI like every other internet call this tool
//! makes - "talking to a service never enters the daemon" (docs/architecture.md)
//! - and the daemon never loads it.
//!
//! **One seam, several vendors.** Every text-to-speech API in use today has the
//! same shape: a JSON POST naming a voice and a model, answered with the audio
//! bytes. ElevenLabs is the one built, because it is the account to hand; the
//! LLM vendors are converging on OpenAI's `/v1/audio/speech` under a bearer
//! key, and that is the shape the next [`Kind`] takes. The config file is keyed
//! by provider name for that reason, with a `base_url` slot for an
//! OpenAI-shaped endpoint hosted somewhere other than OpenAI, so adding a
//! vendor is a variant and a request builder, not a second file format.
//!
//! **The key.** `$XDG_STATE_HOME/x2rock/speech.json`, mode 0600, its own file
//! rather than a corner of `credentials.json`: that file is the music-service
//! tokens, keyed by household, and a text-to-speech key belongs to this machine
//! whatever network it is on. The vendor's own environment variable
//! (`ELEVENLABS_API_KEY`) overrides the file, which is what a script or a CI
//! job expects. Saving a key reads it from stdin, never from an argument, so it
//! never lands in a shell history or a process list.
//!
//! **What was measured (2026-10-02, against the real API).** One sentence to
//! `eleven_flash_v2_5` came back in 0.6s as 49KB of 128kbps mono MP3, billed at
//! 8 characters by the `character-cost` response header, with `request-id` and
//! `history-item-id` alongside. A key scoped to text-to-speech alone - the
//! sensible way to issue one - lacks `voices_read`, so `--voices` and a voice
//! named rather than given by id report that permission by name. The API reads
//! an `Authorization` header too, but wants a bearer token there, not the key,
//! so a speaker can never fetch from ElevenLabs directly (it can send only
//! `Authorization`); the audio has to come through this machine.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hint::{Code, Hint};
use crate::sonos::http;
use crate::store;

/// The config and key file, under the state directory.
pub const FILE: &str = "speech.json";
const SCHEMA: u32 = 1;
/// A service in another country, making audio: patient, but bounded.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Longer than any announcement; shorter than a bug.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Which service. One so far; see the module docs for the second's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ElevenLabs,
}

impl Kind {
    pub const DEFAULT: Kind = Kind::ElevenLabs;

    pub fn parse(name: &str) -> Result<Kind> {
        match name.to_ascii_lowercase().as_str() {
            "elevenlabs" | "eleven" | "11labs" => Ok(Kind::ElevenLabs),
            other => bail!("unknown text-to-speech provider '{other}' (known: elevenlabs)"),
        }
    }

    /// The name the config file and `--provider` use.
    pub fn id(self) -> &'static str {
        match self {
            Kind::ElevenLabs => "elevenlabs",
        }
    }

    /// The vendor's own environment variable for its key; set, it wins.
    pub fn env_key(self) -> &'static str {
        match self {
            Kind::ElevenLabs => "ELEVENLABS_API_KEY",
        }
    }

    fn base_url(self) -> &'static str {
        match self {
            Kind::ElevenLabs => "https://api.elevenlabs.io",
        }
    }

    /// The cheapest model that sounds like a person. ElevenLabs bills Flash at
    /// half the rate of Multilingual v2, and an announcement is not where the
    /// difference is heard.
    fn default_model(self) -> &'static str {
        match self {
            Kind::ElevenLabs => "eleven_flash_v2_5",
        }
    }

    /// A voice that works with a key scoped to text-to-speech alone, so the
    /// first `say` needs no voice chosen. ElevenLabs's premade "George".
    fn default_voice(self) -> &'static str {
        match self {
            Kind::ElevenLabs => "JBFqnCBsd6RMkjVDRZzb",
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub schema: u32,
    /// Set by every write-side accessor, so a command saves once at the end
    /// and only when something moved. Never on disk.
    #[serde(skip)]
    changed: bool,
    /// The provider `say` uses when `--provider` is not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Where to reach a provider other than at its vendor: an OpenAI-shaped
    /// endpoint hosted elsewhere. Unused by ElevenLabs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The provider's voices as last listed, so a name resolves here first.
    ///
    /// Listing needs a permission (`voices_read`) that a key scoped to
    /// text-to-speech alone does not have, and generating never needs. Keeping
    /// the list means the key can be broad for one `--voices` and narrow ever
    /// after, with every name still working.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voices: Vec<Voice>,
    /// When `voices` was taken, unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voices_at: Option<u64>,
}

fn path() -> Result<PathBuf> {
    store::path(FILE)
}

impl Config {
    /// Load, treating a missing file as empty and a corrupt one as an error -
    /// the same stance as the credentials file, for the same reason: starting
    /// over silently would present itself as "no key saved".
    pub fn load() -> Result<Self> {
        Self::load_from(&path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let Some(text) = store::read_optional(path)? else {
            return Ok(Self::default());
        };
        let config: Config =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.schema > SCHEMA {
            bail!(
                "{} is schema {}, newer than this build understands ({SCHEMA})",
                path.display(),
                config.schema
            );
        }
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&path()?)
    }

    /// Written at 0600 whatever is in it: the file exists to hold a key.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let copy = Config {
            schema: SCHEMA,
            changed: false,
            provider: self.provider.clone(),
            providers: self.providers.clone(),
        };
        store::write_atomically(path, &serde_json::to_string_pretty(&copy)?, store::SECRET)
    }

    /// The provider's entry, for writing; marks the config as changed.
    pub fn provider_mut(&mut self, kind: Kind) -> &mut ProviderConfig {
        self.changed = true;
        self.providers.entry(kind.id().to_string()).or_default()
    }

    /// Keep a freshly listed voice list for `kind`, replacing the last.
    pub fn remember_voices(&mut self, kind: Kind, voices: Vec<Voice>) {
        let entry = self.provider_mut(kind);
        entry.voices = voices;
        entry.voices_at = Some(crate::credentials::now());
    }

    /// Write the file if anything was written to the config since it was
    /// loaded or last saved.
    pub fn save_if_changed(&mut self) -> Result<()> {
        if self.changed {
            self.save()?;
            self.changed = false;
        }
        Ok(())
    }

    /// The provider to use: `--provider`, else the saved default, else
    /// [`Kind::DEFAULT`].
    pub fn kind(&self, asked: Option<&str>) -> Result<Kind> {
        match asked.or(self.provider.as_deref()) {
            Some(name) => Kind::parse(name),
            None => Ok(Kind::DEFAULT),
        }
    }
}

/// Read a key the way `--set-key` does: the whole of stdin, trimmed. A key is
/// one token, so trailing newlines from `echo` or a file are not part of it.
pub fn key_from_stdin() -> Result<String> {
    let mut text = String::new();
    // A key is a line; a file mistaken for one should not be buffered whole.
    std::io::stdin()
        .take(64 * 1024)
        .read_to_string(&mut text)
        .context("reading the key from stdin")?;
    let key = text.trim().to_string();
    if key.is_empty() {
        bail!("no key on stdin - pipe or redirect it: x2rock say --set-key < keyfile");
    }
    if key.chars().any(char::is_whitespace) {
        bail!("that is more than one token; a key has no spaces in it");
    }
    Ok(key)
}

/// A provider, resolved and ready: key, endpoint, voice and model all decided.
#[derive(Debug, Clone)]
pub struct Synth {
    pub kind: Kind,
    key: String,
    base_url: String,
    pub voice: String,
    pub model: String,
    /// The voices remembered from the last listing; see
    /// [`ProviderConfig::voices`].
    pub remembered: Vec<Voice>,
    pub remembered_at: Option<u64>,
}

/// One clip of speech, as the provider returned it.
#[derive(Debug)]
pub struct Clip {
    pub bytes: Vec<u8>,
    /// What the provider said it charged, in its own unit (characters, for
    /// ElevenLabs), when it said.
    pub cost: Option<u32>,
}

/// What every clip is: MP3, which every Sonos player accepts for an audio clip
/// and every provider here is asked for.
pub const MIME: &str = "audio/mpeg";

/// What [`Synth::list_voices`] came back with, and from where.
#[derive(Debug)]
pub enum Listing {
    /// The provider answered; the list is now remembered.
    Fresh(Vec<Voice>),
    /// The provider refused and this is the last list it gave, with when,
    /// and the refusal for the caller to show.
    Remembered {
        voices: Vec<Voice>,
        at: Option<u64>,
        because: anyhow::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Voice {
    pub id: String,
    pub name: String,
    pub category: String,
    pub labels: BTreeMap<String, String>,
}

impl Synth {
    /// Resolve a provider from the config and the flags, or say what is missing.
    pub fn from_config(
        config: &Config,
        provider: Option<&str>,
        voice: Option<&str>,
        model: Option<&str>,
    ) -> Result<Synth> {
        let kind = config.kind(provider)?;
        let saved = config.providers.get(kind.id()).cloned().unwrap_or_default();
        let key = std::env::var(kind.env_key())
            .ok()
            .filter(|k| !k.trim().is_empty())
            .or(saved.api_key)
            .ok_or_else(|| not_configured(kind))?;
        Ok(Synth {
            kind,
            key,
            base_url: saved
                .base_url
                .unwrap_or_else(|| kind.base_url().to_string()),
            voice: voice
                .map(str::to_string)
                .or(saved.voice)
                .unwrap_or_else(|| kind.default_voice().to_string()),
            model: model
                .map(str::to_string)
                .or(saved.model)
                .unwrap_or_else(|| kind.default_model().to_string()),
            remembered: saved.voices,
            remembered_at: saved.voices_at,
        })
    }

    /// What a clip of `text` in this voice is filed under: an MD5 over the
    /// provider, the voice, the model and the text - a key, not a security
    /// property. The same sentence in the same voice is one file, so "dinner is
    /// ready" is paid for once.
    pub fn cache_key(&self, text: &str) -> String {
        let mut hasher = Md5::new();
        for part in [self.kind.id(), &self.voice, &self.model, text] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        format!("{:x}", hasher.finalize())
    }

    fn headers(&self) -> Vec<(&str, &str)> {
        match self.kind {
            Kind::ElevenLabs => vec![
                ("xi-api-key", self.key.as_str()),
                ("Content-Type", "application/json"),
                ("Accept", "audio/mpeg"),
            ],
        }
    }

    /// Make the clip.
    pub async fn synthesize(&self, text: &str) -> Result<Clip> {
        let (url, body) = match self.kind {
            Kind::ElevenLabs => (
                format!(
                    "{}/v1/text-to-speech/{}?output_format=mp3_44100_128",
                    self.base_url,
                    http::urlencode(&self.voice)
                ),
                json!({ "text": text, "model_id": self.model }).to_string(),
            ),
        };
        let (status, head, bytes) =
            http::post_bytes(&url, &self.headers(), &body, TIMEOUT, MAX_BYTES).await?;
        if status != 200 {
            return Err(self.refusal(status, &bytes, "making the clip"));
        }
        let content_type = http::header(&head, "content-type").unwrap_or("");
        if !content_type.starts_with("audio/") {
            bail!(
                "{} answered 200 with {content_type:?} rather than audio",
                self.kind.id()
            );
        }
        if bytes.is_empty() {
            bail!("{} answered 200 with an empty body", self.kind.id());
        }
        Ok(Clip {
            bytes,
            cost: http::header(&head, "character-cost").and_then(|v| v.parse().ok()),
        })
    }

    /// The provider's voices, asked for afresh - the ones this key may list.
    async fn fetch_voices(&self) -> Result<Vec<Voice>> {
        match self.kind {
            Kind::ElevenLabs => {
                let url = format!("{}/v2/voices?page_size=100", self.base_url);
                let (status, _head, bytes) = http::get_bytes_with(
                    &url,
                    &[
                        ("xi-api-key", self.key.as_str()),
                        ("Accept", "application/json"),
                    ],
                    TIMEOUT,
                    MAX_BYTES,
                )
                .await?;
                if status != 200 {
                    return Err(self.refusal(status, &bytes, "listing voices"));
                }
                let value: Value = serde_json::from_slice(&bytes)
                    .context("parsing the voice list from ElevenLabs")?;
                Ok(elevenlabs_voices(&value))
            }
        }
    }

    /// The voices: fresh from the provider and remembered into `config` when
    /// the key can list, else the remembered list and the reason it is being
    /// shown instead. A key that can do neither is an error.
    pub async fn list_voices(&self, config: &mut Config) -> Result<Listing> {
        match self.fetch_voices().await {
            Ok(list) => {
                config.remember_voices(self.kind, list.clone());
                Ok(Listing::Fresh(list))
            }
            Err(because) if !self.remembered.is_empty() => Ok(Listing::Remembered {
                voices: self.remembered.clone(),
                at: self.remembered_at,
                because,
            }),
            Err(e) => Err(e),
        }
    }

    /// A voice as the person gave it: an id passes through; a name is looked
    /// up in the remembered list first and in the provider's own list only
    /// when that fails - and a list fetched for the purpose is remembered into
    /// `config`, since that was a moment the key could list.
    pub async fn resolve_voice(&self, config: &mut Config, query: &str) -> Result<String> {
        if looks_like_voice_id(self.kind, query) {
            return Ok(query.to_string());
        }
        if let Some(id) = find_voice(&self.remembered, query)? {
            return Ok(id);
        }
        let voices = self.fetch_voices().await.with_context(|| {
            if self.remembered.is_empty() {
                format!("looking up the voice {query:?} by name (give the id to skip the lookup)")
            } else {
                format!(
                    "{query:?} is not among the {} voices remembered from the last `say --voices`, \
                     and listing afresh failed (a key that can list once lets names work ever after; \
                     or give the id)",
                    self.remembered.len()
                )
            }
        })?;
        let found = find_voice(&voices, query)?;
        config.remember_voices(self.kind, voices);
        found.ok_or_else(|| {
            anyhow!(
                "no voice named {query:?} among the voices this key can list (x2rock say --voices)"
            )
        })
    }

    /// Turn a non-200 into something that says what to do. ElevenLabs's
    /// errors are JSON with `detail.status` and `detail.message`, and the
    /// message already names a missing permission by name.
    fn refusal(&self, status: u16, body: &[u8], doing: &str) -> anyhow::Error {
        let detail = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
            let d = v.get("detail")?;
            let message = d.get("message").and_then(Value::as_str)?.to_string();
            let code = d.get("status").and_then(Value::as_str).map(str::to_string);
            Some((message, code))
        });
        match detail {
            Some((message, code)) => {
                let code = code.map(|c| format!(" [{c}]")).unwrap_or_default();
                anyhow!(
                    "{} refused {doing} (HTTP {status}{code}): {message}",
                    self.kind.id()
                )
            }
            None => anyhow!(
                "{} refused {doing} with HTTP {status}: {}",
                self.kind.id(),
                String::from_utf8_lossy(&body[..body.len().min(200)])
            ),
        }
    }
}

/// An ElevenLabs voice id is twenty characters of base62; anything else given
/// as `--voice` is a name to look up.
fn looks_like_voice_id(kind: Kind, value: &str) -> bool {
    match kind {
        Kind::ElevenLabs => value.len() == 20 && value.chars().all(|c| c.is_ascii_alphanumeric()),
    }
}

/// A name against a list: the whole name first, case-insensitively, then a
/// unique partial match. `Ok(None)` when nothing matches; several partial
/// matches are an error naming them, since guessing between voices is worse
/// than asking.
pub fn find_voice(voices: &[Voice], query: &str) -> Result<Option<String>> {
    let wanted = query.to_lowercase();
    if let Some(v) = voices.iter().find(|v| v.name.to_lowercase() == wanted) {
        return Ok(Some(v.id.clone()));
    }
    let partial: Vec<_> = voices
        .iter()
        .filter(|v| v.name.to_lowercase().contains(&wanted))
        .collect();
    match partial.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.id.clone())),
        several => bail!(
            "{query:?} matches several voices: {} - name one in full, or give its id",
            several
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn elevenlabs_voices(value: &Value) -> Vec<Voice> {
    value
        .get("voices")
        .and_then(Value::as_array)
        .map(|voices| {
            voices
                .iter()
                .filter_map(|v| {
                    Some(Voice {
                        id: v.get("voice_id")?.as_str()?.to_string(),
                        name: v.get("name")?.as_str()?.to_string(),
                        category: v
                            .get("category")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        labels: v
                            .get("labels")
                            .and_then(Value::as_object)
                            .map(|m| {
                                m.iter()
                                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// No key for the provider. No `fix`: saving one reads stdin, which is a
/// person's step, not a command an agent should run blind.
fn not_configured(kind: Kind) -> anyhow::Error {
    Hint::new(
        format!(
            "no {} key is saved and {} is not set. Save one with: x2rock say --set-key < keyfile \
             (it is kept in {} at mode 0600)",
            kind.id(),
            kind.env_key(),
            path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| FILE.into())
        ),
        Code::SpeechNotConfigured,
        None,
    )
    .with_data(json!({ "provider": kind.id(), "env": kind.env_key() }))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;

    #[test]
    fn a_missing_file_is_an_empty_config_and_a_saved_one_round_trips_at_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("speech-config");
        let path = tmp.path().join("speech.json");
        let empty = Config::load_from(&path).unwrap();
        assert!(empty.providers.is_empty());
        assert_eq!(empty.kind(None).unwrap(), Kind::ElevenLabs);

        let mut config = Config::default();
        config.provider_mut(Kind::ElevenLabs).api_key = Some("sk-test".into());
        config.provider_mut(Kind::ElevenLabs).voice = Some("abc".into());
        config.save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let back = Config::load_from(&path).unwrap();
        assert_eq!(back.schema, SCHEMA);
        let saved = &back.providers["elevenlabs"];
        assert_eq!(saved.api_key.as_deref(), Some("sk-test"));
        assert_eq!(saved.voice.as_deref(), Some("abc"));
        assert!(saved.voices.is_empty() && saved.voices_at.is_none());

        // A remembered list survives the round trip and is dated.
        config.remember_voices(Kind::ElevenLabs, vec![junior()]);
        config.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        let saved = &back.providers["elevenlabs"];
        assert_eq!(saved.voices, vec![junior()]);
        assert!(saved.voices_at.is_some());
    }

    impl Config {
        /// Tests build configs by writing to them, which marks them changed.
        fn save_if_changed_marker_reset(&mut self) {
            self.changed = false;
        }
    }

    fn junior() -> Voice {
        Voice {
            id: "tNczbDo8I6QZ94GTdVG0".into(),
            name: "Junior".into(),
            category: "cloned".into(),
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn a_name_matches_whole_first_then_uniquely_in_part_and_never_by_guess() {
        let v = |id: &str, name: &str| Voice {
            id: id.into(),
            name: name.into(),
            category: String::new(),
            labels: BTreeMap::new(),
        };
        let list = vec![
            v("1", "Adam - warm and friendly"),
            v("2", "Adam - Dominant, Firm"),
            v("3", "Junior"),
            v("4", "Junior Junior"),
        ];
        // Whole name wins even when it is also a prefix of another.
        assert_eq!(find_voice(&list, "junior").unwrap().as_deref(), Some("3"));
        // A unique partial match is enough.
        assert_eq!(find_voice(&list, "dominant").unwrap().as_deref(), Some("2"));
        // Several partial matches: refused, naming them.
        let err = find_voice(&list, "adam").unwrap_err().to_string();
        assert!(
            err.contains("Adam - warm and friendly") && err.contains("Dominant"),
            "{err}"
        );
        // Nothing: None, not an error - the caller decides whether to go and ask.
        assert_eq!(find_voice(&list, "nobody").unwrap(), None);
        assert_eq!(find_voice(&[], "junior").unwrap(), None);
    }

    /// With the name remembered, no request leaves this machine: the key here
    /// is nonsense and the base URL unreachable, and it still resolves.
    #[tokio::test]
    async fn a_remembered_name_resolves_without_the_provider() {
        let mut config = Config::default();
        config.provider_mut(Kind::ElevenLabs).api_key = Some("not-a-key".into());
        config.provider_mut(Kind::ElevenLabs).base_url = Some("http://127.0.0.1:9".into());
        config.remember_voices(Kind::ElevenLabs, vec![junior()]);
        let synth = Synth::from_config(&config, None, None, None).unwrap();
        config.save_if_changed_marker_reset();
        let id = synth.resolve_voice(&mut config, "Junior").await.unwrap();
        assert_eq!(id, "tNczbDo8I6QZ94GTdVG0");
        // An id never goes anywhere either.
        let id = synth
            .resolve_voice(&mut config, "JBFqnCBsd6RMkjVDRZzb")
            .await
            .unwrap();
        assert_eq!(id, "JBFqnCBsd6RMkjVDRZzb");
        // Neither touched the config.
        assert!(!config.changed);
        // A name not remembered does go and ask, and the failure says what the
        // list did not hold.
        let err = synth
            .resolve_voice(&mut config, "Nobody")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("remembered"), "{err:#}");
        // The refusal falls back to the remembered list, dated, with the reason.
        match synth.list_voices(&mut config).await.unwrap() {
            Listing::Remembered {
                voices,
                at,
                because,
            } => {
                assert_eq!(voices, vec![junior()]);
                assert!(at.is_some());
                assert!(!format!("{because:#}").is_empty());
            }
            Listing::Fresh(_) => panic!("nothing at 127.0.0.1:9 can answer"),
        }
    }

    #[test]
    fn the_flags_win_over_the_file_and_the_file_over_the_defaults() {
        let mut config = Config::default();
        config.provider_mut(Kind::ElevenLabs).api_key = Some("k".into());
        config.provider_mut(Kind::ElevenLabs).model = Some("saved_model".into());
        let synth = Synth::from_config(&config, None, Some("voice-x"), None).unwrap();
        assert_eq!(synth.voice, "voice-x");
        assert_eq!(synth.model, "saved_model");
        assert_eq!(synth.base_url, "https://api.elevenlabs.io");

        let synth = Synth::from_config(&config, None, None, None).unwrap();
        assert_eq!(synth.voice, Kind::ElevenLabs.default_voice());
    }

    #[test]
    fn no_key_is_the_speech_not_configured_code_with_no_fix() {
        // The env var would satisfy it; make sure the test is not running with one.
        if std::env::var_os("ELEVENLABS_API_KEY").is_some() {
            return;
        }
        let err = Synth::from_config(&Config::default(), None, None, None).unwrap_err();
        let (code, fix) = crate::hint::of(&err);
        assert_eq!(code, Code::SpeechNotConfigured);
        assert!(fix.is_none());
    }

    #[test]
    fn an_unknown_provider_name_is_refused_and_aliases_are_not() {
        assert!(Kind::parse("polly").is_err());
        assert_eq!(Kind::parse("ElevenLabs").unwrap(), Kind::ElevenLabs);
        assert_eq!(Kind::parse("11labs").unwrap(), Kind::ElevenLabs);
    }

    #[test]
    fn the_cache_key_changes_with_every_input_and_nothing_else() {
        let mut config = Config::default();
        config.provider_mut(Kind::ElevenLabs).api_key = Some("k".into());
        let a = Synth::from_config(&config, None, Some("v1"), Some("m1")).unwrap();
        let b = Synth::from_config(&config, None, Some("v2"), Some("m1")).unwrap();
        let c = Synth::from_config(&config, None, Some("v1"), Some("m2")).unwrap();
        assert_eq!(a.cache_key("hi"), a.cache_key("hi"));
        assert_ne!(a.cache_key("hi"), a.cache_key("ho"));
        assert_ne!(a.cache_key("hi"), b.cache_key("hi"));
        assert_ne!(a.cache_key("hi"), c.cache_key("hi"));
        assert_eq!(a.cache_key("hi").len(), 32);
    }

    #[test]
    fn a_voice_id_passes_and_a_name_does_not() {
        assert!(looks_like_voice_id(
            Kind::ElevenLabs,
            "JBFqnCBsd6RMkjVDRZzb"
        ));
        assert!(!looks_like_voice_id(Kind::ElevenLabs, "George"));
        assert!(!looks_like_voice_id(
            Kind::ElevenLabs,
            "JBFqnCBsd6RMkjVDRZz-"
        ));
    }

    #[test]
    fn elevenlabs_voice_list_is_read_with_its_labels() {
        let value = json!({ "voices": [
            { "voice_id": "id1", "name": "George", "category": "premade",
              "labels": { "accent": "British", "gender": "male" } },
            { "voice_id": "id2", "name": "NoLabels" },
            { "name": "no id, skipped" }
        ]});
        let voices = elevenlabs_voices(&value);
        assert_eq!(voices.len(), 2);
        assert_eq!(voices[0].labels["accent"], "British");
        assert_eq!(voices[1].category, "");
    }

    #[test]
    fn a_refusal_quotes_the_providers_own_message_and_status() {
        let mut config = Config::default();
        config.provider_mut(Kind::ElevenLabs).api_key = Some("k".into());
        let synth = Synth::from_config(&config, None, None, None).unwrap();
        let body =
            br#"{"detail":{"status":"missing_permissions","message":"missing voices_read"}}"#;
        let err = synth.refusal(401, body, "listing voices");
        let text = format!("{err:#}");
        assert!(text.contains("missing voices_read"), "{text}");
        assert!(text.contains("[missing_permissions]"), "{text}");
        let err = synth.refusal(502, b"<html>bad gateway</html>", "making the clip");
        assert!(format!("{err:#}").contains("502"));
    }
}
