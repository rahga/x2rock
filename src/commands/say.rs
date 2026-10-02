//! `x2rock say`: a sentence, in a voice, over whatever a room is playing.
//!
//! `notify` with the clip made for you. The text goes to a text-to-speech
//! provider ([`crate::speech`]), the audio comes back here and is kept, and the
//! room's own player fetches it from this machine ([`crate::clipserve`]) and
//! plays it ducked over its programme, exactly as a `notify` URL would be. The
//! three pieces are separate on purpose: the provider knows nothing of
//! speakers, the server knows nothing of providers, and this is the only place
//! that knows both.
//!
//! **The cache.** `$XDG_CACHE_HOME/x2rock/say/`, owner only, one MP3 per
//! (provider, voice, model, text), named for the MD5 of those. The same
//! sentence in the same voice is generated once and billed once; "dinner is
//! ready" the second time costs nothing and takes no round trip to the
//! provider. Bounded by count rather than age - the oldest go when there are
//! more than [`MAX_CACHED`] - because a cached announcement does not go stale.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use crate::clipserve;
use crate::commands::Report;
use crate::commands::speaker::named_speaker;
use crate::commands::stream::APP_ID;
use crate::session::{Session, Target};
use crate::speech::{self, Config, Synth};

/// How long the player gets to come for the clip. It took under a second in
/// testing; this is for a busy player, not a slow one.
const WAIT: Duration = Duration::from_secs(8);
/// How many clips the cache keeps before the oldest go.
pub const MAX_CACHED: usize = 200;

#[derive(Debug, Serialize)]
pub struct SayOutcome {
    pub room: String,
    pub text: String,
    pub provider: &'static str,
    pub voice: String,
    pub model: String,
    /// Served from the cache: nothing was sent to the provider.
    pub cached: bool,
    /// What the provider charged this time, in its unit; null when cached or
    /// when it did not say.
    pub cost: Option<u32>,
    pub bytes: usize,
    /// From the clip request to the player having fetched the file.
    pub fetched_in_ms: u128,
    /// How many times the player asked for it (one, normally).
    pub fetches: u32,
}

impl Report for SayOutcome {
    fn text(&self) -> String {
        let how = if self.cached {
            " (cached)".to_string()
        } else {
            self.cost
                .map(|c| format!(" ({c} characters)"))
                .unwrap_or_default()
        };
        format!("{:<24} said {:?}{how}", self.room, self.text)
    }
}

/// Say `text` on the room's own player.
#[allow(clippy::too_many_arguments)]
pub async fn say(
    session: &Session,
    target: &Target,
    room: Option<&str>,
    text: &str,
    provider: Option<&str>,
    voice: Option<&str>,
    model: Option<&str>,
    volume: Option<u8>,
    port: u16,
) -> Result<SayOutcome> {
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to say");
    }
    let config = Config::load()?;
    let mut synth = Synth::from_config(&config, provider, None, model)?;
    if let Some(voice) = voice {
        synth.voice = synth.resolve_voice(voice).await?;
    }

    // The speaker first, before anything is spent: an unknown room should not
    // cost a generation.
    let (this, upnp) = named_speaker(session, target, room)?;
    let ip = upnp.ip();
    let player = session.player(Some(ip)).await?;

    let key = synth.cache_key(text);
    let dir = cache_dir()?;
    let file = dir.join(format!("{key}.mp3"));
    let (bytes, cached, cost) = match read_cached(&file) {
        Some(bytes) => (bytes, true, None),
        None => {
            let clip = synth.synthesize(text).await?;
            keep(&dir, &file, &clip.bytes)?;
            prune(&dir, MAX_CACHED);
            (clip.bytes, false, clip.cost)
        }
    };
    let size = bytes.len();
    let bytes = Arc::new(bytes);

    let served = clipserve::serve_once(
        bytes,
        speech::MIME,
        &format!("{key}.mp3"),
        ip,
        port,
        WAIT,
        |url| async move {
            player
                .load_audio_clip(&this.id, APP_ID, "x2rock say", Some(&url), volume)
                .await
        },
    )
    .await?;

    Ok(SayOutcome {
        room: this.name.clone(),
        text: text.to_string(),
        provider: synth.kind.id(),
        voice: synth.voice,
        model: synth.model,
        cached,
        cost,
        bytes: size,
        fetched_in_ms: served.waited.as_millis(),
        fetches: served.requests,
    })
}

/// The parts of `say` that need no speaker: saving a key, saving defaults,
/// listing voices. Returns `true` when there is nothing left to do - no text
/// was given, or the voices were listed - so the caller can return without a
/// session.
#[allow(clippy::too_many_arguments)]
pub async fn manage(
    text: Option<&str>,
    provider: Option<&str>,
    voice: Option<&str>,
    model: Option<&str>,
    voices: bool,
    set_key: bool,
    set_default: bool,
    json: bool,
) -> Result<bool> {
    if !(voices || set_key || set_default) {
        if text.is_none() {
            bail!("nothing to say: give the text, or one of --voices, --set-key, --set-default");
        }
        return Ok(false);
    }
    let mut config = Config::load()?;
    let kind = config.kind(provider)?;

    if set_key {
        let key = speech::key_from_stdin()?;
        config.provider_mut(kind).api_key = Some(key);
        if config.provider.is_none() {
            config.provider = Some(kind.id().to_string());
        }
        config.save()?;
        let path = crate::store::path(speech::FILE)?;
        if json {
            println!(
                "{}",
                serde_json::json!({ "provider": kind.id(), "saved": true, "file": path })
            );
        } else {
            println!("{} key saved in {} (mode 0600)", kind.id(), path.display());
        }
    }

    if set_default {
        if voice.is_none() && model.is_none() && provider.is_none() {
            bail!("--set-default saves --voice, --model or --provider; give at least one");
        }
        if let Some(voice) = voice {
            // Resolve a name to an id now, so the saved default never needs
            // the voices permission again.
            let synth = Synth::from_config(&config, provider, None, None)?;
            let id = synth.resolve_voice(voice).await?;
            config.provider_mut(kind).voice = Some(id);
        }
        if let Some(model) = model {
            config.provider_mut(kind).model = Some(model.to_string());
        }
        if provider.is_some() {
            config.provider = Some(kind.id().to_string());
        }
        config.save()?;
        let saved = config.providers.get(kind.id()).cloned().unwrap_or_default();
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "provider": kind.id(),
                    "voice": saved.voice,
                    "model": saved.model,
                    "default_provider": config.provider,
                })
            );
        } else {
            println!(
                "{} defaults: voice {}, model {}",
                kind.id(),
                saved.voice.as_deref().unwrap_or("(provider's)"),
                saved.model.as_deref().unwrap_or("(provider's)")
            );
        }
    }

    if voices {
        let synth = Synth::from_config(&config, provider, None, None)?;
        let list = synth.voices().await?;
        if json {
            println!("{}", serde_json::to_value(&list)?);
        } else if list.is_empty() {
            println!("no voices listed for this key");
        } else {
            for v in &list {
                let labels: Vec<_> = ["gender", "accent", "age", "descriptive", "use_case"]
                    .iter()
                    .filter_map(|k| v.labels.get(*k).map(String::as_str))
                    .collect();
                println!(
                    "{}  {:<20} {:<12} {}",
                    v.id,
                    v.name,
                    v.category,
                    labels.join(", ")
                );
            }
        }
        return Ok(true);
    }

    Ok(text.is_none())
}

/// `$XDG_CACHE_HOME/x2rock/say`.
fn cache_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    Ok(dirs.cache_dir().join("say"))
}

/// A cached clip, its modification time bumped so pruning by age-of-use works
/// where `atime` does not.
fn read_cached(file: &Path) -> Option<Vec<u8>> {
    let bytes = fs::read(file).ok().filter(|b| !b.is_empty())?;
    let _ = fs::File::options()
        .append(true)
        .open(file)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    Some(bytes)
}

/// Write the clip at 0600 under a 0700 directory, by rename, so a crash leaves
/// no half-written MP3 for the next `say` to serve.
fn keep(dir: &Path, file: &Path, bytes: &[u8]) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let scratch = dir.join(format!(".{}.tmp", std::process::id()));
    {
        let mut f = fs::File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&scratch)
            .with_context(|| format!("writing {}", scratch.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&scratch, file).with_context(|| format!("placing {}", file.display()))
}

/// Keep at most `max` clips: the oldest by modification time go first.
fn prune(dir: &Path, max: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut clips: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "mp3"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if clips.len() <= max {
        return;
    }
    clips.sort();
    for (_, path) in clips.iter().take(clips.len() - max) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;

    #[test]
    fn the_text_line_names_the_room_the_words_and_what_it_cost() {
        let mut out = SayOutcome {
            room: "Kitchen".into(),
            text: "Dinner is ready".into(),
            provider: "elevenlabs",
            voice: "v".into(),
            model: "m".into(),
            cached: false,
            cost: Some(8),
            bytes: 100,
            fetched_in_ms: 500,
            fetches: 1,
        };
        assert_eq!(
            out.text(),
            format!("{:<24} said \"Dinner is ready\" (8 characters)", "Kitchen")
        );
        out.cached = true;
        assert!(out.text().ends_with("(cached)"));
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["cached"], true);
        assert_eq!(json["fetched_in_ms"], 500);
    }

    #[test]
    fn a_kept_clip_is_0600_reads_back_and_the_oldest_are_pruned_first() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("say-cache");
        let dir = tmp.path().join("say");
        let file = dir.join("abc.mp3");
        assert!(read_cached(&file).is_none());
        keep(&dir, &file, b"mp3 bytes").unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(read_cached(&file).unwrap(), b"mp3 bytes");
        // No scratch file left beside it.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        // Three more, with distinct times; keep two and the two newest remain.
        for (i, name) in ["b", "c", "d"].iter().enumerate() {
            let f = dir.join(format!("{name}.mp3"));
            keep(&dir, &f, b"x").unwrap();
            let t = std::time::SystemTime::now() + Duration::from_secs(10 * (i as u64 + 1));
            fs::File::options()
                .append(true)
                .open(&f)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        prune(&dir, 2);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["c.mp3", "d.mp3"]);
    }
}
