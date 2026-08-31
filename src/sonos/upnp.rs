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
}

impl Service {
    fn path(self) -> &'static str {
        match self {
            Self::AvTransport => "/MediaRenderer/AVTransport/Control",
            Self::ContentDirectory => "/MediaServer/ContentDirectory/Control",
            Self::MusicServices => "/MusicServices/Control",
        }
    }

    fn urn(self) -> &'static str {
        match self {
            Self::AvTransport => "urn:schemas-upnp-org:service:AVTransport:1",
            Self::ContentDirectory => "urn:schemas-upnp-org:service:ContentDirectory:1",
            Self::MusicServices => "urn:schemas-upnp-org:service:MusicServices:1",
        }
    }
}

pub struct Upnp {
    ip: IpAddr,
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
            // Mutation has its own pair, and they are worth telling apart:
            // 800 is a position that does not exist, 1028 is a queue that
            // moved since the version we were given - someone else editing it.
            "800" => "no such position in the queue",
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
    pub async fn list_services(&self) -> Result<(String, String)> {
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
        Ok((descriptors, version))
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
    async fn update_id(&self) -> Result<String> {
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
}
