# Findings: track ratings / AutoSkip (from reading openphonos)

Source: `~/openphonos` (amp64/openphonos, C#, cloned + built 2026-09-12; kept around
for reference — not going away). All citations are file:line in that tree unless
noted. x2rock has **zero** existing rating code (an earlier grep hit was a false
positive on `saturating_sub`).

**Scope of this doc, per conversation:** wire this into x2rock core (Rust) and
x2rocktv. Quickshell widget gets a UI for it. TUI explicitly excluded for now.

**Status (2026-09-12): core and quickshell are done.** `x2rock rate up|down`
is implemented (`src/sonos/smapi.rs`: `ratings`/`extended_metadata`/
`rate_item`/`RatingsMatch::find`; `src/main.rs`: `run_rate`; caching in
`src/catalogue.rs`) and verified against real hardware both ways - refused
correctly on the household's actual Live iHeartRadio station, rated
successfully both directions on a real Custom/Artist-Radio track, matching
the presentation map's own success strings exactly. The quickshell widget has
thumbs up/down buttons live in the bar (`quickshell/x2rock.sonos/
BarWidget.qml`), screenshot-verified.

**x2rocktv is still open, and is a bigger job than it first looked.** It does
not wrap the `x2rock` CLI - it has its own independent Kotlin Sonos client
(`~/x2rocktv/core/src/main/kotlin/`: `SonosSocket.kt`, `SonosModels.kt`,
`Upnp.kt`, `Discovery.kt`, `SonosHousehold.kt`, `PlayModes.kt`,
`TvSoundbar.kt`, `LanHttp.kt`, `MulticastGate.kt`, `PlayerNames.kt`,
`AppColorTheme.kt`, `Frame.kt`, `SeedStore.kt`). Wiring ratings there means a
second real implementation of everything above - presentation-map fetch and
parse, `getExtendedMetadata`, `rateItem`, the state-dependent id lookup - in
Kotlin against whatever HTTP client x2rocktv already uses, plus a Leanback-
shaped up/down affordance on its now-playing screen. Not yet scoped past that:
next session should start by reading `SonosModels.kt` (for whatever track-id
shape it already carries, the Kotlin analogue of `MusicObjectId`/`Track.id`
in x2rock's own `sonos/proto.rs`) and `SonosSocket.kt` (for the HTTP/WebSocket
machinery already available to build the SMAPI calls on top of).

## The mechanism

Sonos exposes ratings as a SMAPI extension per music service, not a Sonos-side
feature. Three moving parts:

1. **Discovery**: a service's *presentation map* (the same document x2rock
   already fetches for search categories — see `categories()` in
   `src/sonos/smapi.rs:379`) can carry a `NowPlayingRatings` block (or
   `NowPlayingRatings_v2` — openphonos matches on a `StartsWith` because
   Pandora uses the `_v2` form: `Sonos/MusicService.cs:1010`). It lists 1-2
   `Rating` entries (thumbs-down/thumbs-up, or a single one), each with an id,
   an `AutoSkip` flag, a display icon, and success-message text
   (`Sonos/MusicService.cs:826-849`). A service with no such block in its
   presentation map doesn't support ratings at all — this is a **per-service
   capability check you do once**, not a runtime property of a track.

2. **Rating a track**: `rateItem` SOAP call directly to the *music service's*
   endpoint (not Sonos) — `id` of the currently playing item + the rating id
   from step 1 (`Sonos/MusicService.cs:914-952`). The response carries
   `shouldSkip: bool?`. **This is the AutoSkip enforcement, live**: the
   service is telling the client whether to advance immediately, as a direct
   consequence of the rating (thumbs-down on a Pandora-style station usually
   means "and don't finish this one either"). `AutoSkip` on the `Rating`
   object from step 1 is the service's *declared* policy per rating tier;
   `shouldSkip` in the `rateItem` response is the actual per-call answer to
   obey. Use the response field, not the declared policy, as the source of
   truth for whether to skip.

3. **Confirming the station reacted**: openphonos polls `getLastUpdate`
   (`Sonos/MusicService.cs:883-905`) after rating, comparing a `favorites`
   token before/after, up to 10 times, before re-fetching extended metadata
   (`PhonosAvalon/ViewModels/NowPlayingViewModel.cs:1052-1074`). This exists
   because the station's future track selection mutates server-side
   asynchronously and there's no push notification for it. **x2rock likely
   doesn't need this loop** — if the UI just reflects `shouldSkip` and moves
   on, there's nothing further to wait for. Only relevant if we ever want to
   show "this station's playlist updated" feedback.

To rate the *currently playing* track you need its SMAPI item id, which comes
from `getExtendedMetadata` (not implemented in x2rock today — x2rock's
now-playing data comes from AVTransport's track metadata, not a SMAPI call
back to the service). **This is the main new plumbing**: a call to fetch
extended metadata for the current track, which also returns the current
`Rating` state alongside the id needed for `rateItem`.

## Verified against the real household (2026-09-12)

The caveat below turned out to be exactly right, and is no longer a guess —
this household's own iHeartRadio manifest was fetched and read directly.

**iHeartRadio (`service_id: 6`) does declare `NowPlayingRatings`.** Its
manifest (`https://cf.ws.sonos.com/p/m/36760215-347b-405c-a07e-90b4dff74b92`,
cached in `~/.local/state/x2rock/services.json`) points at a presentation map
(`https://cf.ws.sonos.com/p/p/<same id>`) containing a
`<PresentationMap type="NowPlayingRatings">` block. Two things in it change
the plan:

- **`AutoSkip="NEVER"` on every single `Rating` iHeartRadio declares** — both
  thumbs up and thumbs down, in all three state variants below. For this
  service specifically, rating a track **never** auto-skips it, unlike the
  Pandora-style assumption in the original write-up. Still implement
  `shouldSkip` from the live `rateItem` response as the source of truth (a
  different service may answer differently) — just don't expect to ever see
  it fire against iHeartRadio.
- **The rating `Id` to send is not a fixed constant per up/down — it depends
  on the track's *current* rating state.** The block has three `<Match
  propname="..." value="...">` children - `thumbs_up_selected`/`5`,
  `thumbs_down_selected`/`1`, `unselected`/`0` - and each carries its *own*
  pair of up/down `Rating` ids (`555`/`111` when unselected, `5`/`1` in the
  up-selected state, `55`/`11` in the down-selected state). Read whichever
  property `getExtendedMetadata` reports for the current track, use *that*
  Match's `Ratings`, and send the `Id` from there - not a hardcoded "5 means
  up." This is the FindRating dictionary-lookup openphonos's `Ratings`/
  `RatingsMatch` (`MusicService.cs:851-871`) exists to handle; x2rock needs
  the same lookup, not a fixed pair of ids.

**This household is currently proof of the caveat, and the open question above
is now answered - negatively, and more fundamentally than expected.**
`x2rock -r Bedroom raw upnp AVTransport GetMediaInfo InstanceID=0` right now
returns `CurrentURI: x-sonosapi-stream:live_stations.6790?sid=6...`,
`upnp:class: object.item.audioItem.audioBroadcast`, title "Love Songs Radio" -
a live broadcast, playing via iHeartRadio, at the exact moment this was
checked - this is not a hypothetical, it is tonight's actual listening.

The Control API's own `playbackMetadata:1 getMetadataStatus` for that group
was read directly (`--scope group`, since that namespace is group-scoped):
the **container** (the station, "Love Songs Radio") carries a real id -
`{"accountId":"sn_15","objectId":"live_stations.6790","serviceId":"6"}` - but
`currentItem.track` (the actual song airing right now, "Take My Breath Away"
by Berlin) has **no `id` field of any kind**: only `name`, `artist`, `images`,
`quality`, `type`. There is nothing to hand `getExtendedMetadata` or
`rateItem` for the track itself - not "an id that turns out unratable," no id
at all. A live simulcast has an ID3-derived now-playing label, not a
per-track SMAPI identity; the only addressable object is the station, and
rating a station is a different (and, per the presentation map, unoffered)
thing from rating the song currently on it.

**So: this specific, real, tonight's-listening case cannot be rated by
construction, independent of anything still to build.** The feature remains
real and worth having - iHeartRadio's *Custom Stations* ("Perfect For You"
personalized mixes, a different content type from a Live station) do give
each track a real `currentItem.track.id`, and Pandora-style services
generally do - but it will show or do nothing for a household simply tuned to
a Live station, which per tonight's check is the household's actual iHeartRadio
usage.

**Confirmed directly, same evening, on Dining Room.** `x2rock search -s
iHeartRadio "Berlin" --category artists` finds `artist_radio.2648` (a
container); browsing it (`x2rock browse -s iHeartRadio artist_radio.2648`)
returns a real `artist_radio_track.<...>` leaf with `queueable: true`.
Queueing and advancing to it, then reading `playbackMetadata:1
getMetadataStatus` directly, showed a full `track.id:
{"accountId":"sn_15","objectId":"artist_radio_track.artist-2648-...-761331","serviceId":"6"}`
- the same shape a normal YouTube Music/Spotify queue item's id has, and
nothing like the Live station's bare `"-1"`. **The split is real, not
theoretical: Live stations cannot be rated (no id to rate); Custom/Artist-Radio
stations can (a real, addressable id).** Room restored to its prior state
afterward (test track removed from Dining Room's queue).

## Concrete integration points in x2rock

**`src/sonos/smapi.rs`** (mirrors the existing `search`/`metadata`/`media_uri`
pattern — same `call()` helper at line 931, same
`refreshed: &mut Option<RefreshedToken>` token-refresh threading):

- Extend presentation-map parsing (wherever `categories()` parses it, line
  379) to also pull `NowPlayingRatings`/`NowPlayingRatings_v2` into a lookup
  table keyed by `(propname, value)` — **not a flat up/down pair**: verified
  against iHeartRadio's real map, the `Id` to send depends on the track's
  *current* state (three `Match` blocks, three different id pairs - see
  "Verified against the real household" above). Each match's value is a small
  `Vec<Rating>` (id, auto_skip, label); this is the one-time per-service
  capability check from step 1 above, and the per-track lookup happens at
  rating time against whichever property `getExtendedMetadata` reported.
- New `pub async fn extended_metadata(service, token, id, refreshed) -> Result<ExtendedMetadata>`
  wrapping `getExtendedMetadata`, returning at least the current rating state
  and the id needed for `rate_item`.
- New `pub async fn rate_item(service, token, id, rating_id, refreshed) -> Result<RateResult>`
  wrapping `rateItem`, `RateResult { should_skip: Option<bool>, message: Option<String> }`.

**`src/main.rs`**: a room-scoped command, e.g. `x2rock -r <Room> rate up|down`
(or `thumbs-up`/`thumbs-down` — match whatever verb x2rock uses elsewhere for
two-state toggles). On `should_skip == Some(true)`, call whatever internal
function `next` already calls (x2rock already has skip/next plumbing) rather
than shelling out to itself. Surface current rating availability in
`now --json` / `status --json` so a client (TV app, quickshell) can decide
whether to show the buttons at all — this doubles as the fix for the caveat
above: don't show the UI when the service/content-type doesn't support it.

**x2rocktv**: same core call, surfaced as up/down affordance on the
now-playing screen (Leanback-shaped — a D-pad-focusable pair of buttons, not
a menu). Depends on whatever x2rocktv's `core` module (Kotlin) uses to talk
to x2rock/Sonos — I didn't dig into `~/x2rocktv/core/src/main/kotlin` in this
pass, just confirmed the module layout exists to receive this.

**Quickshell widget** (`~/x2rock/quickshell/x2rock.sonos/BarWidget.qml`): add
thumbs up/down icons next to existing transport controls, conditionally shown
per the `now`/`status` capability flag above, calling the new `rate`
subcommand.

**TUI**: explicitly out of scope per this conversation — don't add it to
`src/tui/*`.

## Open questions

- Whether iHeartRadio's `getExtendedMetadata` reports a rating-state property
  for a *live broadcast* track specifically (answered automatically once it's
  implemented - see above; no separate investigation needed).
- Exact `getExtendedMetadata` response shape for iHeartRadio specifically
  (openphonos's parsing is generic XML-to-dict; x2rock will want its own
  typed parse once we see a real payload).
- Whether `AutoSkip`-driven skip should be silent or surfaced ("skipped
  because you rated it down") in each client - low priority now that
  iHeartRadio itself never sets it.
