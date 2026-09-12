# Findings: track ratings / AutoSkip (from reading openphonos)

Source: `~/openphonos` (amp64/openphonos, C#, cloned + built 2026-09-12; kept around
for reference — not going away). All citations are file:line in that tree unless
noted. x2rock has **zero** existing rating code (an earlier grep hit was a false
positive on `saturating_sub`).

**Scope of this doc, per conversation:** wire this into x2rock core (Rust) and
x2rocktv. Quickshell widget gets a UI for it. TUI explicitly excluded for now.

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

## Important caveat — verify before building UI (this is the part most likely to bite)

Ratings/AutoSkip only make sense for a **personalized, skippable station**
(Pandora-style, or iHeartRadio's "Custom Stations" / "For You" feature) —
**not** for a fixed live broadcast. A live radio stream has no queue and no
per-listener next-track decision; there's nothing to rate or skip *to*.

x2rock already has hard-won iHeartRadio knowledge that's directly relevant
here: iHeartRadio's `live_stations.` item type is explicitly a different
content type from anything ratable, and it's already rejected by
`AddURIToQueue` (`src/main.rs:2087`) and handled specially for account-identity
matching (`src/main.rs:3288`, `src/main.rs:4895`). **If the household is just
tuned to a Live station (the likely case for "iHeartRadio constantly at
night" — a specific station/channel, not a personalized mix), none of this
applies**, and building the UI without checking would mean shipping thumbs
buttons that do nothing or error confusingly.

Action item before implementing: confirm iHeartRadio's presentation map
*does* declare `NowPlayingRatings` for its Custom Station content type (fetch
a real household's iHeartRadio presentation map and check — I haven't done
this, no live household access from this environment). If Custom Stations
aren't in use, this feature has no visible effect until/unless someone starts
one.

## Concrete integration points in x2rock

**`src/sonos/smapi.rs`** (mirrors the existing `search`/`metadata`/`media_uri`
pattern — same `call()` helper at line 931, same
`refreshed: &mut Option<RefreshedToken>` token-refresh threading):

- Extend presentation-map parsing (wherever `categories()` parses it, line
  379) to also pull `NowPlayingRatings`/`NowPlayingRatings_v2` into a
  `Vec<RatingOption>` (id, auto_skip, icon, label) on `Service` or a sibling
  cache — this is the one-time per-service capability check from step 1
  above.
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

- Does iHeartRadio's presentation map actually declare ratings for Custom
  Stations? (untested — needs a live household)
- Exact `getExtendedMetadata` response shape for iHeartRadio specifically
  (openphonos's parsing is generic XML-to-dict; x2rock will want its own
  typed parse once we see a real payload)
- Whether `AutoSkip`-driven skip should be silent or surfaced ("skipped
  because you rated it down") in each client
