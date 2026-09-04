//! UPnP/SOAP on port 1400, used only for what the Control API cannot do: the queue.
//!
//! The Control API - cloud or local - has no view of the queue the official apps
//! populate; UPnP has all of it. These are plain outbound HTTP calls, so they work
//! behind a default-deny firewall. UPnP *eventing* (GENA) is never used: it needs
//! the player to connect back to us, which that same firewall silently blocks.
//!
//! The HTTP client is deliberately minimal. It talks to exactly one server
//! implementation, which answers with `Transfer-Encoding: chunked` and
//! `Connection: close` (verified), so it dechunks and reads to end of stream.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use roxmltree::Document;

use super::http;

pub const PORT: u16 = 1400;
const TIMEOUT: Duration = Duration::from_secs(8);
/// How long to let a soundbar's group handoff swallow the reply to the request
/// that caused it before going to look instead, how long to allow each look, how
/// long to wait between looks, and how many to make. The switch completes at
/// about 13.5s, so the budget reaches past 25s: long enough that the answer is
/// always waited for, short enough that a room which never switches says so.
const TV_HANDOFF: Duration = Duration::from_millis(1500);
const TV_ASK: Duration = Duration::from_secs(3);
const TV_POLL: Duration = Duration::from_millis(500);
const TV_CONFIRMATIONS: u32 = 8;
/// Queues can hold tens of thousands of tracks; listing stops here and says so.
pub const MAX_QUEUE_ITEMS: u32 = 1000;
const PAGE: u32 = 100;

#[derive(Clone, Copy)]
enum Service {
    AvTransport,
    ContentDirectory,
    MusicServices,
    /// Alarms. Household-wide: any player answers for all of them, and each
    /// alarm names the room it belongs to.
    AlarmClock,
    /// Tone controls. Per speaker, and reachable nowhere else: the Control API
    /// has no EQ namespace at all, so this is the only door to bass, treble and
    /// loudness - the Sonos app's own "EQ Settings for <room>" panel.
    RenderingControl,
}

impl Service {
    fn path(self) -> &'static str {
        match self {
            Self::AvTransport => "/MediaRenderer/AVTransport/Control",
            Self::ContentDirectory => "/MediaServer/ContentDirectory/Control",
            Self::MusicServices => "/MusicServices/Control",
            Self::AlarmClock => "/AlarmClock/Control",
            Self::RenderingControl => "/MediaRenderer/RenderingControl/Control",
        }
    }

    fn urn(self) -> &'static str {
        match self {
            Self::AvTransport => "urn:schemas-upnp-org:service:AVTransport:1",
            Self::ContentDirectory => "urn:schemas-upnp-org:service:ContentDirectory:1",
            Self::MusicServices => "urn:schemas-upnp-org:service:MusicServices:1",
            Self::AlarmClock => "urn:schemas-upnp-org:service:AlarmClock:1",
            Self::RenderingControl => "urn:schemas-upnp-org:service:RenderingControl:1",
        }
    }
}

/// The bounds `RenderingControl`'s own service description gives for bass and
/// treble: `i2`, `-10..10`, step 1. Read off the player rather than guessed -
/// `http://<ip>:1400/xml/RenderingControl1.xml` publishes them.
pub const TONE_RANGE: std::ops::RangeInclusive<i8> = -10..=10;

/// One speaker's tone controls.
///
/// Per player, never per group: the app calls this panel "EQ Settings for
/// <room>", and two speakers playing together keep their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tone {
    pub bass: i8,
    pub treble: i8,
    /// A low-frequency lift that does its work at low listening levels. **On
    /// from the factory on every Sonos speaker**, so a household that has never
    /// touched it is not neutral.
    pub loudness: bool,
    /// TruePlay: whether the stored room correction is being applied.
    ///
    /// A different mechanism from the three above, applied underneath them: the
    /// iPhone app measures a room and stores a per-speaker curve. So a speaker
    /// can report flat bass and treble while this is reshaping it, and turning
    /// loudness off does not touch it.
    pub trueplay: bool,
    /// Whether there is a calibration to apply at all. `trueplay` is only a
    /// toggle: it reads on with nothing measured behind it, which is why both
    /// are reported rather than one.
    pub trueplay_available: bool,
}

/// One alarm, exactly as `ListAlarms` reports it.
///
/// Every field is carried even where the CLI does nothing with it, because
/// **`UpdateAlarm` demands all of them**: omitting `Volume` is a UPnP 402, not a
/// "leave it alone". So changing one thing means reading the alarm, editing the
/// field and writing the whole record back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    pub id: u32,
    /// `HH:MM:SS` local. Reported as `StartTime` and written back as
    /// `StartLocalTime` - the two names are the same field.
    pub start: String,
    pub duration: String,
    /// `ONCE`, `WEEKDAYS`, `WEEKENDS`, `DAILY` - or `ON_<digits>` for named
    /// days, which the service accepts but its own description does not list.
    pub recurrence: String,
    pub enabled: bool,
    pub room_uuid: String,
    pub program_uri: String,
    pub program_metadata: String,
    pub play_mode: String,
    pub volume: u8,
    pub include_linked_zones: bool,
}

impl Alarm {
    /// How long it plays for, in milliseconds. `None` if the player reported
    /// something that is not a duration.
    pub fn duration_ms(&self) -> Option<u128> {
        parse_hms(&self.duration).map(|d| d.as_millis())
    }

    /// The program in a word where there is one. `x-rincon-buzzer:0` is the
    /// built-in chime and every controller shows it as a name rather than a
    /// URI; anything else is a stream or a queue and is shown as it is.
    pub fn program(&self) -> &str {
        if self.program_uri.starts_with("x-rincon-buzzer") {
            "chime"
        } else {
            &self.program_uri
        }
    }
}

pub struct Upnp {
    ip: IpAddr,
}

/// What `ListAvailableServices` answers with, unparsed.
pub struct Services {
    pub descriptors: String,
    /// `AvailableServiceTypeList`: comma-separated `serviceId * 256 + type`.
    pub types: String,
    /// `AvailableServiceListVersion`, for deciding whether a cache is stale.
    pub version: String,
}

#[derive(Debug)]
pub struct QueueItem {
    /// 1-based, as Sonos numbers them.
    pub index: u32,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub art_url: Option<String>,
}

/// Something that can be put in the queue: a saved playlist, a favorite.
#[derive(Debug)]
pub struct BrowseItem {
    /// The player's own id, e.g. `SQ:3` or `FV:2/19`.
    pub id: String,
    pub title: String,
    /// What `AddURIToQueue` enqueues.
    pub uri: Option<String>,
    /// The item's `r:resMD`, carried through untouched.
    ///
    /// For anything from a music service this holds a `<desc id="cdudn">`
    /// bearing that service's account token. Rebuilding metadata from the
    /// title and URI would drop it and the service would refuse the track, so
    /// this is passed on exactly as it arrived rather than parsed and remade.
    pub metadata: String,
    pub art_url: Option<String>,
    /// A service's navigation entry rather than something to play: `FV:2`
    /// carries these alongside real favorites, marked `<r:type>shortcut</r:type>`
    /// and with an empty `<res>`. "Trending Now" and "Discover Sonos Radio" are
    /// two of them. They belong in the Sonos app's browse tree, not in a list of
    /// things a room can be told to play.
    pub shortcut: bool,
}

impl BrowseItem {
    /// What the item actually is, taken from the metadata it carries rather
    /// than its own class - a favorite always wears `sonos-favorite` on the
    /// outside, whatever it points at.
    pub fn kind(&self) -> Option<String> {
        let inner = Document::parse(&self.metadata).ok()?;
        Some(text_of(&inner, "class")?.to_owned())
    }

    /// Whether this can be appended to a queue.
    ///
    /// The rule is the *kind*, not the URI scheme: an individual track from a
    /// music service enqueues perfectly well on a scheme whose container form
    /// is refused. Verified on both - a `musicTrack` favorite and a queued
    /// service track both add, while the playlist container behind the same
    /// `x-sonosapi-hls-static` answers 800.
    pub fn can_enqueue(&self) -> bool {
        match self.kind() {
            // Anything a player can hold a position in.
            Some(kind) => kind.contains("musicTrack"),
            // No metadata to judge by: saved queues and plain streams, which
            // are known to enqueue, carry none.
            None => self.uri.as_deref().is_some_and(|uri| {
                matches!(
                    uri.split(':').next().unwrap_or_default(),
                    "file" | "x-rincon-mp3radio" | "http" | "https"
                )
            }),
        }
    }
}

#[derive(Debug)]
pub struct Queue {
    /// The queue's version, as `Browse` reports it. Mutating actions send it
    /// back so the player can reject a change aimed at a queue that has since
    /// moved under us - someone else with the Sonos app open, typically.
    pub update_id: String,
    pub items: Vec<QueueItem>,
    /// Total tracks in the queue, which may exceed `items.len()` if truncated.
    pub total: u32,
}

impl Upnp {
    pub fn new(ip: IpAddr) -> Self {
        Self { ip }
    }

    /// Every alarm in the household.
    ///
    /// `CurrentAlarmList` is an escaped XML document inside the reply, so this
    /// parses twice: the envelope, then the `<Alarms>` it carries as text.
    pub async fn alarms(&self) -> Result<Vec<Alarm>> {
        let text = self.soap(Service::AlarmClock, "ListAlarms", &[]).await?;
        let outer = Document::parse(&text)?;
        let inner = text_of(&outer, "CurrentAlarmList").unwrap_or("");
        if inner.trim().is_empty() {
            return Ok(Vec::new());
        }
        alarms_in(inner)
    }

    /// The household's own clock, and whether it knows what timezone it is in.
    ///
    /// Returns the local time it reports and its timezone index, where **-1
    /// means unset**. This matters because an alarm's `StartLocalTime` is local
    /// *to the household*: with no timezone configured the clock runs UTC, so
    /// "07:00" is 07:00 UTC and not 07:00 wherever the person typing it is.
    pub async fn household_time(&self) -> Result<(String, i32)> {
        let now = self.soap(Service::AlarmClock, "GetTimeNow", &[]).await?;
        let doc = Document::parse(&now)?;
        let local = text_of(&doc, "CurrentLocalTime").unwrap_or("").to_string();
        let zone = self.soap(Service::AlarmClock, "GetTimeZone", &[]).await?;
        let doc = Document::parse(&zone)?;
        let index = text_of(&doc, "Index")
            .and_then(|i| i.trim().parse().ok())
            .unwrap_or(-1);
        Ok((local, index))
    }

    /// Create an alarm, returning the id the household assigns it.
    ///
    /// Takes an [`Alarm`] with its `id` ignored, so the ten fields are named
    /// once rather than twice - the create and update argument lists are
    /// identical but for `ID` being an output here and an input there.
    pub async fn create_alarm(&self, alarm: &Alarm) -> Result<u32> {
        let bit = |on: bool| if on { "1" } else { "0" };
        let text = self
            .soap(
                Service::AlarmClock,
                "CreateAlarm",
                &[
                    ("StartLocalTime", &alarm.start),
                    ("Duration", &alarm.duration),
                    ("Recurrence", &alarm.recurrence),
                    ("Enabled", bit(alarm.enabled)),
                    ("RoomUUID", &alarm.room_uuid),
                    ("ProgramURI", &alarm.program_uri),
                    ("ProgramMetaData", &alarm.program_metadata),
                    ("PlayMode", &alarm.play_mode),
                    ("Volume", &alarm.volume.to_string()),
                    ("IncludeLinkedZones", bit(alarm.include_linked_zones)),
                ],
            )
            .await?;
        let doc = Document::parse(&text)?;
        text_of(&doc, "AssignedID")
            .and_then(|id| id.trim().parse().ok())
            .ok_or_else(|| anyhow!("CreateAlarm answered without an AssignedID: {text}"))
    }

    /// Write an alarm back whole.
    ///
    /// Every field goes, because the action requires it - see [`Alarm`]. Read
    /// one, change what you meant to change, hand it back.
    pub async fn update_alarm(&self, alarm: &Alarm) -> Result<()> {
        let bit = |on: bool| if on { "1" } else { "0" };
        self.soap(
            Service::AlarmClock,
            "UpdateAlarm",
            &[
                ("ID", &alarm.id.to_string()),
                // Reported as StartTime, written as StartLocalTime.
                ("StartLocalTime", &alarm.start),
                ("Duration", &alarm.duration),
                ("Recurrence", &alarm.recurrence),
                ("Enabled", bit(alarm.enabled)),
                ("RoomUUID", &alarm.room_uuid),
                ("ProgramURI", &alarm.program_uri),
                ("ProgramMetaData", &alarm.program_metadata),
                ("PlayMode", &alarm.play_mode),
                ("Volume", &alarm.volume.to_string()),
                ("IncludeLinkedZones", bit(alarm.include_linked_zones)),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn destroy_alarm(&self, id: u32) -> Result<()> {
        self.soap(
            Service::AlarmClock,
            "DestroyAlarm",
            &[("ID", &id.to_string())],
        )
        .await?;
        Ok(())
    }

    /// What is left on the group's sleep timer, or `None` when none is set.
    ///
    /// An unset timer answers with an **empty** duration rather than a zero,
    /// and the generation counter reads 0 - either is the signal, and the empty
    /// element is the one that cannot be confused with a timer about to fire.
    pub async fn sleep_timer(&self) -> Result<Option<Duration>> {
        let text = self
            .soap(
                Service::AvTransport,
                "GetRemainingSleepTimerDuration",
                &[("InstanceID", "0")],
            )
            .await?;
        let doc = Document::parse(&text)?;
        Ok(text_of(&doc, "RemainingSleepTimerDuration")
            .filter(|raw| !raw.trim().is_empty())
            .and_then(parse_hms))
    }

    /// Arm the sleep timer, or cancel it with `None`.
    ///
    /// The duration goes on the wire as `HH:MM:SS` and nothing else: a bare
    /// count of seconds comes back UPnP 402. Cancelling is an **empty string**
    /// to the same action - there is no separate cancel, which is why this
    /// takes an `Option` rather than having a sibling.
    pub async fn set_sleep_timer(&self, after: Option<Duration>) -> Result<()> {
        let value = match after {
            Some(d) => {
                let secs = d.as_secs();
                format!(
                    "{:02}:{:02}:{:02}",
                    secs / 3600,
                    (secs / 60) % 60,
                    secs % 60
                )
            }
            None => String::new(),
        };
        self.soap(
            Service::AvTransport,
            "ConfigureSleepTimer",
            &[("InstanceID", "0"), ("NewSleepTimerDuration", &value)],
        )
        .await?;
        Ok(())
    }

    /// Read all three tone controls.
    ///
    /// Three round trips because the service offers no combined read; they go
    /// out together rather than in sequence, since none depends on the others.
    pub async fn tone(&self) -> Result<Tone> {
        let (bass, treble, loudness, calibration) = tokio::try_join!(
            self.tone_number("GetBass", "CurrentBass"),
            self.tone_number("GetTreble", "CurrentTreble"),
            self.loudness(),
            self.calibration(),
        )?;
        let (trueplay, trueplay_available) = calibration;
        Ok(Tone {
            bass,
            treble,
            loudness,
            trueplay,
            trueplay_available,
        })
    }

    /// TruePlay's two booleans: applied, and available to apply.
    ///
    /// One call answers both, and the pair is the whole of what the speaker
    /// will say - there is no read for the curve itself, or for when or where
    /// it was measured.
    async fn calibration(&self) -> Result<(bool, bool)> {
        let text = self
            .soap(
                Service::RenderingControl,
                "GetRoomCalibrationStatus",
                &[("InstanceID", "0")],
            )
            .await?;
        let doc = Document::parse(&text)?;
        let flag = |tag: &str| text_of(&doc, tag).map(|v| v.trim() == "1");
        Ok((
            flag("RoomCalibrationEnabled").unwrap_or(false),
            flag("RoomCalibrationAvailable").unwrap_or(false),
        ))
    }

    pub async fn set_trueplay(&self, on: bool) -> Result<()> {
        self.soap(
            Service::RenderingControl,
            "SetRoomCalibrationStatus",
            &[
                ("InstanceID", "0"),
                ("RoomCalibrationEnabled", if on { "1" } else { "0" }),
            ],
        )
        .await?;
        Ok(())
    }

    async fn tone_number(&self, action: &str, field: &str) -> Result<i8> {
        let text = self
            .soap(Service::RenderingControl, action, &[("InstanceID", "0")])
            .await?;
        level_in(&text, field)
    }

    /// Loudness alone takes a `Channel`, which the other two do not. The
    /// service description says so and the player enforces it: without it the
    /// call comes back a UPnP 402 (invalid args), which reads like a bad level
    /// rather than a missing field.
    async fn loudness(&self) -> Result<bool> {
        let text = self
            .soap(
                Service::RenderingControl,
                "GetLoudness",
                &[("InstanceID", "0"), ("Channel", "Master")],
            )
            .await?;
        loudness_in(&text)
    }

    pub async fn set_bass(&self, level: i8) -> Result<()> {
        self.set_tone("SetBass", "DesiredBass", level).await
    }

    pub async fn set_treble(&self, level: i8) -> Result<()> {
        self.set_tone("SetTreble", "DesiredTreble", level).await
    }

    async fn set_tone(&self, action: &str, field: &str, level: i8) -> Result<()> {
        if !TONE_RANGE.contains(&level) {
            bail!(
                "{level} is outside the {}..{} the player accepts",
                TONE_RANGE.start(),
                TONE_RANGE.end()
            );
        }
        self.soap(
            Service::RenderingControl,
            action,
            &[("InstanceID", "0"), (field, &level.to_string())],
        )
        .await?;
        Ok(())
    }

    pub async fn set_loudness(&self, on: bool) -> Result<()> {
        self.soap(
            Service::RenderingControl,
            "SetLoudness",
            &[
                ("InstanceID", "0"),
                ("Channel", "Master"),
                ("DesiredLoudness", if on { "1" } else { "0" }),
            ],
        )
        .await?;
        Ok(())
    }

    /// One HTTP/1.1 POST, returning `(status, body)`.
    async fn post(&self, path: &str, soap_action: &str, body: &str) -> Result<(u16, String)> {
        let endpoint = http::Endpoint::Lan {
            ip: self.ip,
            port: PORT,
        };
        let action = format!("\"{soap_action}\"");
        http::post(
            &endpoint,
            false,
            path,
            &[
                ("Content-Type", "text/xml; charset=\"utf-8\""),
                ("SOAPACTION", &action),
            ],
            body,
            TIMEOUT,
        )
        .await
    }

    /// Invoke one action and return the response envelope, with UPnP faults as errors.
    async fn soap(&self, service: Service, action: &str, args: &[(&str, &str)]) -> Result<String> {
        let mut params = String::new();
        for (name, value) in args {
            params.push_str(&format!("<{name}>{}</{name}>", escape(value)));
        }
        let envelope = format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{urn}">{params}</u:{action}></s:Body></s:Envelope>"#,
            urn = service.urn()
        );
        let soap_action = format!("{}#{action}", service.urn());
        let (status, text) = self.post(service.path(), &soap_action, &envelope).await?;

        if status == 200 {
            return Ok(text);
        }
        if status == 403 {
            bail!(
                "the player refused UPnP (HTTP 403). Enable it in the Sonos app: \
                 Settings > Privacy & Security > UPnP"
            );
        }
        let doc = Document::parse(&text).ok();
        let code = doc
            .as_ref()
            .and_then(|d| text_of(d, "errorCode"))
            .unwrap_or("?");
        let detail = match code {
            "701" => "no media loaded or action not available in this state",
            "711" => "no such track in the queue",
            // 800 is UPnP's *undefined* error, so it means whatever the action
            // decided it means. It was glossed as "no such position in the
            // queue", which is only true of `Seek` and `ReorderTracks` - and
            // that gloss then narrated an `AddURIToQueue` refusal for a
            // percent-encoding bug and, later, for a live stream that simply
            // cannot be queued. A wrong explanation is worse than none, so this
            // says what the code actually is and lets the action name itself.
            "800" => "the player refused it, with no reason given",
            // 1028 is a queue that moved since the version we were given -
            // someone else editing it.
            "1028" => "the queue changed while this was in flight; try again",
            "402" => "invalid arguments",
            _ => doc
                .as_ref()
                .and_then(|d| text_of(d, "errorDescription"))
                .unwrap_or(""),
        };
        bail!("{action} failed: UPnP error {code} ({detail})")
    }

    /// One page of the queue.
    async fn browse_queue(&self, start: u32, count: u32) -> Result<Queue> {
        let text = self
            .soap(
                Service::ContentDirectory,
                "Browse",
                &[
                    ("ObjectID", "Q:0"),
                    ("BrowseFlag", "BrowseDirectChildren"),
                    ("Filter", "*"),
                    ("StartingIndex", &start.to_string()),
                    ("RequestedCount", &count.to_string()),
                    ("SortCriteria", ""),
                ],
            )
            .await?;
        let envelope = Document::parse(&text).context("parsing Browse response")?;
        let total = text_of(&envelope, "TotalMatches")
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        let update_id = text_of(&envelope, "UpdateID").unwrap_or("0").to_owned();
        // The DIDL-Lite document is carried as escaped text; the parser has
        // already unescaped one layer, leaving XML to parse again.
        let didl_text = text_of(&envelope, "Result").unwrap_or("");
        if didl_text.trim().is_empty() {
            return Ok(Queue {
                total,
                update_id,
                items: Vec::new(),
            });
        }
        let didl = Document::parse(didl_text).context("parsing queue DIDL-Lite")?;

        let items = didl
            .descendants()
            .filter(|n| n.has_tag_name("item"))
            .map(|item| {
                let child = |name: &str| {
                    item.children()
                        .find(|c| c.tag_name().name() == name)
                        .and_then(|c| c.text())
                        .map(str::to_owned)
                };
                let index = item
                    .attribute("id")
                    .and_then(|id| id.rsplit('/').next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let duration = item
                    .children()
                    .find(|c| c.has_tag_name("res"))
                    .and_then(|r| r.attribute("duration"))
                    .and_then(parse_hms);
                QueueItem {
                    index,
                    title: child("title").unwrap_or_default(),
                    artist: child("creator"),
                    album: child("album"),
                    duration,
                    art_url: child("albumArtURI").map(|uri| self.absolute(&uri)),
                }
            })
            .collect();
        Ok(Queue {
            items,
            total,
            update_id,
        })
    }

    /// The whole queue, up to [`MAX_QUEUE_ITEMS`].
    pub async fn queue(&self) -> Result<Queue> {
        let mut page = self.browse_queue(0, PAGE).await?;
        let total = page.total;
        let update_id = page.update_id.clone();
        let mut items = Vec::with_capacity(total.min(MAX_QUEUE_ITEMS) as usize);
        let mut start = 0;
        loop {
            let got = page.items.len() as u32;
            items.extend(page.items);
            start += got;
            if got == 0 || start >= total || start >= MAX_QUEUE_ITEMS {
                break;
            }
            page = self.browse_queue(start, PAGE).await?;
        }
        Ok(Queue {
            items,
            total,
            update_id,
        })
    }

    /// Browse a container - `SQ:` for saved playlists, `FV:2` for favorites.
    ///
    /// Kept apart from [`Self::browse_queue`] because the two want different
    /// things from the same DIDL: the queue needs positions and durations,
    /// this needs what it would take to enqueue the thing.
    /// Every music service Sonos knows about, as the raw descriptor list.
    ///
    /// This is the whole catalogue, not the household's - there is no command
    /// for the second, and `musicServiceAccounts:1` only reports that the set
    /// has changed. Parsing belongs to `smapi`, which is what uses it.
    ///
    /// No credential: a player answers this to anyone on the LAN.
    ///
    /// Returns the descriptors and the list's version, which rides along in the
    /// same reply. The version is what makes caching honest: it is the same
    /// number `musicServiceAccounts:1` reports as `availableServicesVersion`
    /// when the set changes, so a cache can be checked against it rather than
    /// against a guessed expiry.
    pub async fn list_services(&self) -> Result<Services> {
        let text = self
            .soap(Service::MusicServices, "ListAvailableServices", &[])
            .await?;
        let envelope = Document::parse(&text).context("parsing ListAvailableServices")?;
        let descriptors = text_of(&envelope, "AvailableServiceDescriptorList")
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no service descriptors in the reply"))?;
        let version = text_of(&envelope, "AvailableServiceListVersion")
            .unwrap_or_default()
            .to_string();
        // The type list is the other half of a service's identity: each entry is
        // `serviceId * 256 + type`, and the type is what a cdudn is built from.
        let types = text_of(&envelope, "AvailableServiceTypeList")
            .unwrap_or_default()
            .to_string();
        Ok(Services {
            descriptors,
            types,
            version,
        })
    }

    /// One page of DIDL-Lite into items. Split out from `browse_content` so the
    /// shape of what a player actually sends can be tested without one.
    fn items_from_didl(&self, didl_text: &str) -> Result<Vec<BrowseItem>> {
        let didl = Document::parse(didl_text)?;
        // Playlists come back as containers and favorites as items; both are
        // enqueued the same way.
        Ok(didl
            .descendants()
            .filter(|n| n.has_tag_name("item") || n.has_tag_name("container"))
            .map(|node| {
                let child = |name: &str| {
                    node.children()
                        .find(|c| c.tag_name().name() == name)
                        .and_then(|c| c.text())
                };
                BrowseItem {
                    id: node.attribute("id").unwrap_or_default().to_owned(),
                    title: child("title").unwrap_or_default().to_owned(),
                    uri: child("res").map(str::to_owned),
                    metadata: child("resMD").unwrap_or_default().to_owned(),
                    art_url: child("albumArtURI").map(|uri| self.absolute(uri)),
                    // The marker, not the missing `res`: a real favorite whose
                    // content the service resolves can also arrive without one,
                    // and dropping those would lose things that do play.
                    shortcut: child("type").is_some_and(|t| t.eq_ignore_ascii_case("shortcut")),
                }
            })
            .collect())
    }

    pub async fn browse_content(&self, object_id: &str) -> Result<Vec<BrowseItem>> {
        let mut items = Vec::new();
        let mut start = 0;
        loop {
            let text = self
                .soap(
                    Service::ContentDirectory,
                    "Browse",
                    &[
                        ("ObjectID", object_id),
                        ("BrowseFlag", "BrowseDirectChildren"),
                        ("Filter", "*"),
                        ("StartingIndex", &start.to_string()),
                        ("RequestedCount", &PAGE.to_string()),
                        ("SortCriteria", ""),
                    ],
                )
                .await?;
            let envelope =
                Document::parse(&text).with_context(|| format!("parsing Browse of {object_id}"))?;
            let total: u32 = text_of(&envelope, "TotalMatches")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            // Nothing there comes back as an empty <Result/>, which roxmltree
            // reads as no text at all - a list of none, not a parse error.
            let didl_text = text_of(&envelope, "Result").unwrap_or("");
            if total == 0 || didl_text.trim().is_empty() {
                break;
            }
            let page = self
                .items_from_didl(didl_text)
                .with_context(|| format!("parsing DIDL-Lite of {object_id}"))?;
            let got = page.len() as u32;
            items.extend(page);
            start += got;
            if got == 0 || start >= total || start >= MAX_QUEUE_ITEMS {
                break;
            }
        }
        Ok(items)
    }

    /// Put something in the queue, returning the queue's new length.
    ///
    /// `metadata` is the source item's `r:resMD` verbatim; empty is fine for a
    /// saved queue, which needs no service credential.
    pub async fn add_to_queue(&self, uri: &str, metadata: &str, next: bool) -> Result<u32> {
        // "Next" is a position, not a flag: EnqueueAsNext on its own still
        // appends (verified), so the position has to be named outright. With
        // nothing playing the current track reads 0, which puts it at the
        // front - the only sensible reading of "next" from a standing start.
        // Only meaningful when the queue is the source: with a radio stream or a
        // service station playing, the AVTransport track number reads 1 and has
        // nothing to do with where the queue currently sits, so "next" would
        // land at position 2 of a queue nobody is playing. Append instead.
        let position = if next && self.playing_from_queue().await? {
            self.current_track().await? + 1
        } else {
            0
        };
        let text = self
            .soap(
                Service::AvTransport,
                "AddURIToQueue",
                &[
                    ("InstanceID", "0"),
                    ("EnqueuedURI", uri),
                    ("EnqueuedURIMetaData", metadata),
                    // 0 appends.
                    ("DesiredFirstTrackNumberEnqueued", &position.to_string()),
                    ("EnqueueAsNext", if next { "1" } else { "0" }),
                ],
            )
            .await?;
        let doc = Document::parse(&text).context("parsing AddURIToQueue response")?;
        Ok(text_of(&doc, "NewQueueLength")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0))
    }

    /// How many tracks the queue holds, without fetching them.
    pub async fn queue_len(&self) -> Result<u32> {
        Ok(self.browse_queue(0, 1).await?.total)
    }

    /// The queue's current version, read cheaply.
    ///
    /// Every mutation asks for this immediately beforehand rather than making
    /// callers carry one around: a version fetched a moment ago is the whole
    /// point of the field, and one carried across a user's deliberations is not.
    pub async fn update_id(&self) -> Result<String> {
        Ok(self.browse_queue(0, 1).await?.update_id)
    }

    /// Drop one track, by its 1-based queue position.
    pub async fn remove_track(&self, index: u32) -> Result<()> {
        let update_id = self.update_id().await?;
        self.soap(
            Service::AvTransport,
            "RemoveTrackFromQueue",
            &[
                ("InstanceID", "0"),
                ("ObjectID", &format!("Q:0/{index}")),
                ("UpdateID", &update_id),
            ],
        )
        .await?;
        Ok(())
    }

    /// Drop `count` tracks from `start`. The player does the whole range itself,
    /// so a half-removed range is not a state this can leave behind.
    pub async fn remove_range(&self, start: u32, count: u32) -> Result<()> {
        let update_id = self.update_id().await?;
        self.soap(
            Service::AvTransport,
            "RemoveTrackRangeFromQueue",
            &[
                ("InstanceID", "0"),
                ("UpdateID", &update_id),
                ("StartingIndex", &start.to_string()),
                ("NumberOfTracks", &count.to_string()),
            ],
        )
        .await?;
        Ok(())
    }

    /// Empty the queue. Sonos keeps no undo for this.
    pub async fn clear_queue(&self) -> Result<()> {
        self.soap(
            Service::AvTransport,
            "RemoveAllTracksFromQueue",
            &[("InstanceID", "0")],
        )
        .await?;
        Ok(())
    }

    /// Move the track at `from` so that it sits at `to`, both 1-based.
    pub async fn move_track(&self, from: u32, to: u32) -> Result<()> {
        let update_id = self.update_id().await?;
        self.soap(
            Service::AvTransport,
            "ReorderTracksInQueue",
            &[
                ("InstanceID", "0"),
                ("StartingIndex", &from.to_string()),
                ("NumberOfTracks", "1"),
                ("InsertBefore", &insert_before(from, to).to_string()),
                ("UpdateID", &update_id),
            ],
        )
        .await?;
        Ok(())
    }

    /// Save the queue as a Sonos playlist, returning the id it was given.
    pub async fn save_queue(&self, title: &str) -> Result<String> {
        let text = self
            .soap(
                Service::AvTransport,
                "SaveQueue",
                &[
                    ("InstanceID", "0"),
                    ("Title", title),
                    // Empty means "a new playlist" rather than overwriting one.
                    ("ObjectID", ""),
                ],
            )
            .await?;
        let doc = Document::parse(&text).context("parsing SaveQueue response")?;
        Ok(text_of(&doc, "AssignedObjectID")
            .unwrap_or_default()
            .to_owned())
    }

    /// 1-based queue index of the current track; 0 when nothing is loaded.
    pub async fn current_track(&self) -> Result<u32> {
        let text = self
            .soap(
                Service::AvTransport,
                "GetPositionInfo",
                &[("InstanceID", "0")],
            )
            .await?;
        let doc = Document::parse(&text).context("parsing GetPositionInfo response")?;
        Ok(text_of(&doc, "Track")
            .and_then(|t| t.parse().ok())
            .unwrap_or(0))
    }

    /// Whether the group is currently playing from its queue rather than, say, a
    /// radio stream or line-in.
    pub async fn playing_from_queue(&self) -> Result<bool> {
        let text = self
            .soap(Service::AvTransport, "GetMediaInfo", &[("InstanceID", "0")])
            .await?;
        let doc = Document::parse(&text).context("parsing GetMediaInfo response")?;
        Ok(text_of(&doc, "CurrentURI").is_some_and(|uri| uri.starts_with("x-rincon-queue:")))
    }

    /// Make the coordinator's queue the current source.
    pub async fn use_queue(&self, coordinator_id: &str) -> Result<()> {
        self.soap(
            Service::AvTransport,
            "SetAVTransportURI",
            &[
                ("InstanceID", "0"),
                ("CurrentURI", &format!("x-rincon-queue:{coordinator_id}#0")),
                ("CurrentURIMetaData", ""),
            ],
        )
        .await?;
        Ok(())
    }

    /// Point a soundbar at its TV input.
    ///
    /// The Control API cannot do this: its `loadLineIn` is for analog line-in
    /// and answers `ERROR_NOT_CAPABLE`, "player does not have line-in", on a
    /// Beam. The URI is the one the device itself reports as `CurrentURI` while
    /// on TV, and `spdif` covers HDMI-ARC as well as optical.
    ///
    /// `player_id` is the soundbar's own and `bar` its address; the call itself
    /// goes to whichever player coordinates its group, as all AVTransport calls
    /// do, so the two differ whenever a soundbar is a member rather than the
    /// coordinator.
    ///
    /// That case needs care. Switching the group to the soundbar's HDMI hands
    /// coordination to the soundbar, and the old coordinator stops coordinating
    /// before it answers this very request: the read waits out its timeout and
    /// reports a failure for something that plainly worked (verified - the group
    /// followed the TV while the CLI printed "timed out reading from ...").
    /// Addressing the soundbar instead would answer in milliseconds, but it
    /// means something else entirely - the soundbar leaves the group and takes
    /// the TV alone, rather than bringing the room with it (also verified). So
    /// the request stands as it is, and a lost answer is checked rather than
    /// believed.
    ///
    /// How long that check takes is set by the hardware, and it is not quick.
    /// Bringing up HDMI stalls AVTransport across the whole handoff: measured,
    /// both the soundbar and the old coordinator answer in ~10ms until 1.1s in,
    /// then answer nothing at all for twelve seconds, then come back at ~13.5s
    /// with the soundbar on `x-sonos-htastream:` and the old coordinator
    /// following it. There is no faster witness - an uninvolved player answers
    /// its topology in 30ms throughout but reports the old grouping for just as
    /// long. So this waits, because roughly fourteen seconds is what the switch
    /// costs, and reporting it sooner would only be guessing.
    ///
    /// Nothing is checked when the soundbar already coordinates: there is then
    /// no group to hand over, no stall, and no answer to lose.
    pub async fn use_tv_input(&self, player_id: &str, bar: IpAddr) -> Result<()> {
        // Bound rather than inlined: the future holds the arguments by reference
        // and outlives the statement that builds them.
        let uri = format!("x-sonos-htastream:{player_id}:spdif");
        let args = [
            ("InstanceID", "0"),
            ("CurrentURI", uri.as_str()),
            ("CurrentURIMetaData", ""),
        ];
        let call = self.soap(Service::AvTransport, "SetAVTransportURI", &args);

        // A soundbar that already coordinates hands nothing over, so its answer
        // is the answer and an error from it is a real one.
        if bar == self.ip {
            call.await?;
            return Ok(());
        }

        // Otherwise the handoff is certain, and so is losing the answer to it.
        // Waiting out the full read timeout only to ask the soundbar anyway
        // costs the caller twenty seconds for a switch that took four, so give
        // the reply a brief chance and then go and look.
        let refusal = match tokio::time::timeout(TV_HANDOFF, call).await {
            Ok(Ok(_)) => return Ok(()),
            // A refusal does arrive, and quickly. Hold it: it is the honest
            // thing to report if the soundbar turns out not to have switched.
            Ok(Err(e)) => Some(e),
            Err(_) => None,
        };

        let soundbar = Upnp::new(bar);
        let mut last = String::new();
        for _ in 0..TV_CONFIRMATIONS {
            // Bounded per ask, because the stall opens partway through and an
            // unbounded one would sit inside it rather than asking again.
            if let Ok(Ok(uri)) = tokio::time::timeout(TV_ASK, soundbar.current_uri()).await {
                if is_tv_stream(&uri) {
                    return Ok(());
                }
                last = uri;
            }
            tokio::time::sleep(TV_POLL).await;
        }
        Err(refusal.unwrap_or_else(|| {
            // Still following means the request was never acted on; anything
            // else means it was, and stopped somewhere short of the TV.
            if is_following(&last) {
                anyhow!("{bar} is still following its group; it did not take the TV input")
            } else {
                anyhow!("{bar} did not reach its TV input; it is on {last:?}")
            }
        }))
    }

    /// What this player is currently playing from: its own queue, a stream, its
    /// TV input, or the room it is following.
    pub async fn current_uri(&self) -> Result<String> {
        let text = self
            .soap(Service::AvTransport, "GetMediaInfo", &[("InstanceID", "0")])
            .await?;
        let doc = Document::parse(&text).context("parsing GetMediaInfo response")?;
        Ok(text_of(&doc, "CurrentURI").unwrap_or_default().to_owned())
    }

    /// Jump to a 1-based queue position. Does not change the transport state.
    pub async fn seek_track(&self, index: u32) -> Result<()> {
        self.soap(
            Service::AvTransport,
            "Seek",
            &[
                ("InstanceID", "0"),
                ("Unit", "TRACK_NR"),
                ("Target", &index.to_string()),
            ],
        )
        .await?;
        Ok(())
    }

    /// Album art comes back as a path on the player; make it a URL.
    fn absolute(&self, uri: &str) -> String {
        if uri.starts_with("http://") || uri.starts_with("https://") {
            uri.to_owned()
        } else {
            format!("http://{}:{PORT}{uri}", self.ip)
        }
    }
}

/// The alarms in an `<Alarms>` document.
///
/// Split from the call because the document arrives escaped inside the reply
/// and is worth reading on its own - it is the one place every field of an
/// alarm appears at once.
fn alarms_in(xml: &str) -> Result<Vec<Alarm>> {
    let doc = Document::parse(xml)?;
    Ok(doc
        .descendants()
        .filter(|n| n.tag_name().name() == "Alarm")
        .filter_map(|n| {
            let attr = |name: &str| n.attribute(name).unwrap_or("").to_string();
            Some(Alarm {
                // An alarm with no usable id could not be addressed afterwards,
                // so it is dropped rather than listed as one nothing can act on.
                id: n.attribute("ID")?.parse().ok()?,
                start: attr("StartTime"),
                duration: attr("Duration"),
                recurrence: attr("Recurrence"),
                enabled: attr("Enabled") == "1",
                room_uuid: attr("RoomUUID"),
                program_uri: attr("ProgramURI"),
                program_metadata: attr("ProgramMetaData"),
                play_mode: attr("PlayMode"),
                volume: attr("Volume").parse().unwrap_or(0),
                include_linked_zones: attr("IncludeLinkedZones") == "1",
            })
        })
        .collect())
}

/// A tone level out of a `RenderingControl` reply.
fn level_in(text: &str, field: &str) -> Result<i8> {
    let doc = Document::parse(text)?;
    let raw = text_of(&doc, field).ok_or_else(|| anyhow!("no {field} in the reply: {text}"))?;
    raw.trim()
        .parse()
        .with_context(|| format!("{field} was {raw:?}, which is not a level"))
}

/// Loudness out of a `GetLoudness` reply.
///
/// The wire carries `1` and `0`, which `parse::<bool>()` rejects - it wants
/// "true"/"false" - so the comparison is written out rather than parsed.
fn loudness_in(text: &str) -> Result<bool> {
    let doc = Document::parse(text)?;
    let raw = text_of(&doc, "CurrentLoudness")
        .ok_or_else(|| anyhow!("no CurrentLoudness in the reply: {text}"))?;
    Ok(raw.trim() == "1")
}

fn text_of<'a>(doc: &'a Document, tag: &str) -> Option<&'a str> {
    doc.descendants()
        .find(|n| n.tag_name().name() == tag)
        .and_then(|n| n.text())
}

/// The URI a player reports while on its TV input. The player id in the middle
/// is the soundbar's own, so this recognises the input without naming a room.
fn is_tv_stream(uri: &str) -> bool {
    uri.starts_with("x-sonos-htastream:")
}

/// Whether a player is following another room rather than driving its own
/// transport. `x-rincon-queue:` is its own queue and is not following, which is
/// why the two prefixes have to be told apart rather than matched on `x-rincon`.
fn is_following(uri: &str) -> bool {
    uri.starts_with("x-rincon:")
}

fn insert_before(from: u32, to: u32) -> u32 {
    if to > from { to + 1 } else { to }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `H:MM:SS` or `H:MM:SS.mmm`, as UPnP reports durations.
/// Every `(service id, account serial)` a list of items names in its URIs.
///
/// The household will not enumerate its accounts - `musicServiceAccounts:1` has
/// no read command and neither UPnP service offers one - so the serials have to
/// be read off content that happens to mention them. `FV:2` and `Q:0` are where
/// they turn up.
///
/// **This is not the household's account list, and must never be presented as
/// one.** A serial stays visible after its account is deleted, because deleting
/// an account does not rewrite a queue that names it; and an account that has
/// only ever played a station is absent, because a station never enters a
/// queue. Both were observed on one household within an hour. See "The harvest
/// showed a deleted account and hid a live one" in docs/architecture.md.
pub fn serials_in(items: &[BrowseItem]) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for item in items {
        for text in [item.uri.as_deref(), item.art_url.as_deref()]
            .into_iter()
            .flatten()
        {
            // Art URLs carry the same query percent-encoded, as the `u=`
            // parameter of `/getaa`, so one decode serves both shapes.
            let flat = text
                .replace("%3d", "=")
                .replace("%3D", "=")
                .replace("%26", "&");
            if let (Some(sid), Some(sn)) = (digits_after(&flat, "sid="), digits_after(&flat, "sn="))
            {
                out.insert((sid.to_string(), sn.to_string()));
            }
        }
    }
    out
}

/// The run of digits immediately after `key`, if there is one.
fn digits_after<'a>(hay: &'a str, key: &str) -> Option<&'a str> {
    let start = hay.find(key)? + key.len();
    let rest = &hay[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

pub fn parse_hms(text: &str) -> Option<Duration> {
    let mut parts = text.split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Duration::from_secs_f64((h * 3600 + m * 60) as f64 + s))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replies captured off the Media Room speaker, 2026-09-04.
    const GET_BASS: &str = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:GetBassResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1"><CurrentBass>0</CurrentBass></u:GetBassResponse></s:Body></s:Envelope>"#;
    const GET_LOUDNESS: &str = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:GetLoudnessResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1"><CurrentLoudness>1</CurrentLoudness></u:GetLoudnessResponse></s:Body></s:Envelope>"#;

    /// The `<Alarms>` document as the player produced it, 2026-09-04.
    #[test]
    fn an_alarm_is_read_out_of_the_escaped_list() {
        let list = r#"<Alarms><Alarm ID="1" StartTime="07:00:00" Duration="00:15:00" Recurrence="ON_13" Enabled="0" RoomUUID="RINCON_48A6B81853E001400" ProgramURI="x-rincon-buzzer:0" ProgramMetaData="" PlayMode="NORMAL" Volume="10" IncludeLinkedZones="0"/></Alarms>"#;
        let alarms = alarms_in(list).unwrap();
        assert_eq!(alarms.len(), 1);
        let a = &alarms[0];
        assert_eq!(a.id, 1);
        // Reported as StartTime; written back as StartLocalTime. Same field,
        // two names, and only the write side uses the longer one.
        assert_eq!(a.start, "07:00:00");
        assert_eq!(a.duration_ms(), Some(900_000));
        // `ON_13` is a day bitmap the service accepts although its own
        // description lists only ONCE/WEEKDAYS/WEEKENDS/DAILY, so it is carried
        // through rather than validated against that list.
        assert_eq!(a.recurrence, "ON_13");
        assert!(!a.enabled);
        assert_eq!(a.volume, 10);
        assert_eq!(a.program(), "chime");

        // An empty list is empty, not an error.
        assert!(alarms_in("<Alarms></Alarms>").unwrap().is_empty());
        // An entry with no id cannot be addressed later, so it is dropped.
        assert!(
            alarms_in(r#"<Alarms><Alarm StartTime="07:00:00"/></Alarms>"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_tone_level_is_read_out_of_the_reply_signed() {
        assert_eq!(level_in(GET_BASS, "CurrentBass").unwrap(), 0);
        // The signed half is the one worth pinning: the state variable is `i2`
        // over -10..10, so a cut at zero would silently lose everything below
        // flat - and "less bass" is the whole reason to reach for this.
        assert_eq!(
            level_in(&GET_BASS.replace(">0<", ">-7<"), "CurrentBass").unwrap(),
            -7
        );
        // A field that is not there is an error naming it, not a quiet 0: a
        // flat EQ and an unanswered question must not read the same.
        assert!(level_in(GET_BASS, "CurrentTreble").is_err());
    }

    #[test]
    fn loudness_reads_the_wires_one_rather_than_the_word_true() {
        assert!(loudness_in(GET_LOUDNESS).unwrap());
        assert!(!loudness_in(&GET_LOUDNESS.replace(">1<", ">0<")).unwrap());
        assert!(loudness_in(GET_BASS).is_err(), "wrong reply, not a false");
    }

    #[test]
    fn hms_parses_upnp_durations() {
        assert_eq!(parse_hms("0:02:56"), Some(Duration::from_secs(176)));
        assert_eq!(parse_hms("1:00:00"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_hms("0:00:01.500"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_hms("NOT_IMPLEMENTED"), None);
        assert_eq!(parse_hms("1:2:3:4"), None);
    }

    #[test]
    fn the_tv_input_is_told_apart_from_every_other_source() {
        assert!(is_tv_stream(
            "x-sonos-htastream:RINCON_48A6B830668701400:spdif"
        ));
        // A room following a group's queue, and a radio stream: neither is TV,
        // and both are what a soundbar reports the rest of the time.
        assert!(!is_tv_stream("x-rincon-queue:RINCON_48A6B818D13801400#0"));
        assert!(!is_tv_stream("x-sonosapi-stream:s24940?sid=254&flags=8"));
        assert!(!is_tv_stream(""));
    }

    #[test]
    fn following_a_room_is_told_apart_from_driving_a_queue() {
        // The prefixes differ by one character before the colon, and the
        // difference is the whole confirmation that a soundbar took the handoff.
        assert!(is_following("x-rincon:RINCON_48A6B818D13801400"));
        assert!(!is_following("x-rincon-queue:RINCON_48A6B830668701400#0"));
        assert!(!is_following(
            "x-sonos-htastream:RINCON_48A6B830668701400:spdif"
        ));
        assert!(!is_following(""));
    }

    #[test]
    fn moving_down_accounts_for_the_gap_left_behind() {
        // 2 -> 5: by the time it is inserted, tracks 3-5 have shifted up one,
        // so landing after the old 5 means inserting before 6.
        assert_eq!(insert_before(2, 5), 6);
        // 5 -> 2: nothing before 2 has moved, so it inserts where it says.
        assert_eq!(insert_before(5, 2), 2);
        // Staying put is not a move, and must not drift.
        assert_eq!(insert_before(3, 3), 3);
    }

    #[test]
    fn a_service_shortcut_is_told_apart_from_a_favorite() {
        // Straight from this household: FV:2 carries Sonos Radio's navigation
        // entries beside real favorites. They have an empty <res>, so they can
        // be neither enqueued nor played, and the Sonos app does not show them
        // as favorites either.
        let didl = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
              xmlns:dc="http://purl.org/dc/elements/1.1/"
              xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/">
            <item id="FV:2/0"><dc:title>Trending Now</dc:title><res></res>
              <r:type>shortcut</r:type></item>
            <item id="FV:2/9"><dc:title>Real Favorite</dc:title>
              <res>x-sonosapi-stream:s24940</res><r:type>instantPlay</r:type></item>
            <item id="FV:2/8"><dc:title>No Type At All</dc:title>
              <res>x-sonosapi-stream:s111</res></item>
          </DIDL-Lite>"#;
        let items = Upnp::new("127.0.0.1".parse().unwrap())
            .items_from_didl(didl)
            .unwrap();
        assert_eq!(
            items.len(),
            3,
            "parsing keeps everything; filtering is the caller's"
        );
        assert!(items[0].shortcut, "marked shortcut");
        assert!(!items[1].shortcut, "a real favorite with a type of its own");
        assert!(!items[2].shortcut, "no r:type is not a shortcut");
        // An empty <res> parses as no text at all, so `uri` is None rather than
        // Some(""). Worth pinning: it means `uri.is_none()` cannot distinguish a
        // shortcut from a favorite the service resolves, which is exactly why
        // the r:type marker is what decides.
        assert!(items[0].uri.is_none());
    }

    #[test]
    fn soap_arguments_are_escaped() {
        assert_eq!(escape(r#"a&b<c>"d""#), "a&amp;b&lt;c&gt;&quot;d&quot;");
    }

    fn item(uri: Option<&str>, art: Option<&str>) -> BrowseItem {
        BrowseItem {
            id: "Q:0/1".into(),
            title: "t".into(),
            uri: uri.map(str::to_string),
            metadata: String::new(),
            art_url: art.map(str::to_string),
            shortcut: false,
        }
    }

    #[test]
    fn serials_come_off_plain_and_percent_encoded_urls() {
        // A queue item's own URI, and the art URL that wraps the same query
        // percent-encoded - both seen on this household.
        let items = [
            item(Some("x-sonos-http:podcast.mp3?sid=6&flags=8&sn=15"), None),
            item(
                None,
                Some(
                    "/getaa?s=1&u=x-sonosapi-hls-static%3acloudcast%3fsid%3d181%26flags%3d8232%26sn%3d17",
                ),
            ),
        ];
        let got = serials_in(&items);
        assert!(got.contains(&("6".to_string(), "15".to_string())));
        assert!(got.contains(&("181".to_string(), "17".to_string())));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn one_service_can_hold_several_serials() {
        // sid 6 held sn_5 and sn_15 at once, which is the whole reason this
        // returns pairs rather than a map keyed by service.
        let items = [
            item(Some("x:a?sid=6&sn=5"), None),
            item(Some("x:b?sid=6&sn=15"), None),
        ];
        assert_eq!(serials_in(&items).len(), 2);
    }

    #[test]
    fn items_naming_no_account_are_skipped() {
        let items = [
            item(Some("x-rincon-queue:RINCON_1#0"), None),
            item(Some("x:c?sid=333"), None),
            item(None, None),
        ];
        assert!(serials_in(&items).is_empty());
    }
}
