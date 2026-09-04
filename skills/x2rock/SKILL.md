---
name: x2rock
description: Control Sonos speakers from the command line with the `x2rock` CLI — play, pause, skip, per-room and whole-house volume, mute, shuffle and repeat, the queue, favorites, music-service search and browse, internet radio by URL, grouping and party mode, soundbar TV input, alarms, the sleep timer, per-speaker tone controls (bass, treble, loudness, TruePlay), saved playlists, and a one-call JSON snapshot of the whole household. Use whenever the user wants to control Sonos or speakers — "play/put on <something> in <room>", "pause", "skip", "turn it up/down", "quieter/louder everywhere", "mute the kitchen", "shuffle", "what's playing / what's on", "group these rooms", "play this stream/radio URL", "find me a country/jazz/ambient station", "put on some free radio", "play everywhere / party", "set an alarm", "turn off my alarm", "sleep timer / stop in 30 minutes", "turn up the bass", "is loudness on", or switch a soundbar to TV.
---

# Driving Sonos with the `x2rock` CLI

`x2rock` controls Sonos speakers on the local network, with **no Sonos account**. Control never
leaves the LAN; `search`, `browse` and `stations` do reach out to music services and a radio
directory, none of which want a Sonos login. Every command is a
one-shot subprocess. A **background daemon may also be running** (it publishes now-playing to the
Linux desktop over MPRIS) — it is *not* needed for anything you do from the CLI. When speakers seem
missing, `x2rock status` diagnoses it (see "When no speakers are available"). Two contracts hold
everything together:

1. **Run `x2rock status --json` first.** It is the whole household in one call, and you cannot write
   a correct room- or group-aware response without it.
2. **Read fields, never prose.** Every data command takes `--json`; a failure prints a JSON
   `{error, code, fix}` on stderr. The wording changes; the JSON shape does not.

`x2rock --version` prints the version. This skill ships embedded in that binary, so it matches the
CLI it came from — if the version has moved since you installed the skill, re-run `x2rock skill`.

## The shapes you will read

### `status --json` — a bare array, one object per group

```json
[
  {
    "room": "Kitchen", "state": "PLAYING", "title": "Solitude",
    "artist": "…", "album": null, "position_ms": 41000, "duration_ms": null,
    "queue_position": 3, "explicit": false, "crossfade": false,
    "next_title": "Blue in Green", "next_artist": "Miles Davis",
    "service": "YouTube Music", "service_id": "284", "art_url": "http://…",
    "volume": 2, "muted": false, "audible": true,
    "repeat": "off", "shuffle": false,
    "on_tv": false, "has_tv": false, "input_format": null, "surround": null,
    "stream_info": null,
    "members": ["Kitchen"], "coordinator": "Kitchen"
  },
  {
    "room": "Bedroom", "state": "PLAYING", "title": "TV Audio",
    "on_tv": true, "has_tv": true, "input_format": "Dolby Digital 2.0", "surround": false,
    "service": null, "service_id": null, "volume": 12, "muted": false, "audible": true,
    "members": ["Bedroom"], "coordinator": "Bedroom", "repeat": "off", "shuffle": false
  },
  {
    "room": "Dining Room + 1", "state": "PLAYING", "title": "Señorita",
    "service": "Plex", "service_id": "212", "volume": 1, "muted": false, "audible": true,
    "members": ["Dining Room", "Kitchen"], "coordinator": "Dining Room",
    "repeat": "off", "shuffle": false, "on_tv": false, "has_tv": false
  }
]
```

- Now-playing is **flat** on the room object (`title`, `artist`, `album`, `position_ms`,
  `duration_ms`, `next_title`, `next_artist`), not nested.
- **`queue_position` is 1-based and has no total.** It is `null` whenever the queue is not what is
  driving - a radio stream has no position in a queue. For the length, read `queue --json`, which
  carries both. `explicit` is the content flag every controller shows as a badge, `null` when the
  source does not say. `crossfade` is a third play mode beside repeat and shuffle and is settable.
- **`stream_info` is a live stream's own "now playing" text**, verbatim and unparsed - typically
  `"Artist - Title"`, but a station may put a show name or a slogan there instead, so do not split
  it. `null` for anything that is not a live stream. For a stream started by `play-url` it is the
  *only* track information there is: `title` is then the station's name and `artist`, `album`,
  `duration_ms` and `service` are all `null`. Report it as what is playing; do not present it as a
  parsed artist and title.
- `next_title`/`next_artist` are what plays after this, `null` at the end of a queue and on a
  stream. `position_ms`/`duration_ms` are **milliseconds** (`duration_ms` is
  `null` for a live stream). Any value can be `null` when the player does not supply it.
- **Grouping**: two grouped rooms appear as **one** entry — the third above. Its `room` is a display
  label like `"Dining Room + 1"`, `members` is the real room names, `coordinator` is the room the
  group is named for, and `volume` is the group's mix. A lone room is its own only member.
- **The `room` value of a grouped entry is NOT a valid `-r` argument** — `-r "Dining Room + 1"`
  fails with `unknown_room`. Address the group by its `coordinator` or any `members` name.
- `status --json --full` wraps the array in `{household, network, total, reachable, warnings,
  rooms}`. `rooms --json` is a cheaper room-and-state list — **also per-group**, so for a flat list
  of every room name, flatten the `members` arrays (or read `rooms` from an `unknown_room` error,
  which is flat).

### `favorites --json` — a bare array

```json
[
  {"id":"3","name":"90s90s - Christmas","service":"TuneIn (New)","type":"STREAM","playable":true,"art_url":"…","description":"…"},
  {"id":"19","name":"37. 100 Greatest Classic Country Songs","service":null,"type":null,"playable":false,"art_url":"…","description":"Amazon Music Playlist"}
]
```

`now --json` is a **single bare object** — one room's *now-playing* fields (state, title, artist,
album, service, service_id, position_ms, duration_ms, repeat, shuffle, on_tv, input_format,
surround, art_url), with no `-r` picking the household's one group (and erroring if there are
several). It is the confirm-step after a play. It is a **subset** of a `status` entry: `volume`,
`muted`, `audible`, `members`, `coordinator` and `has_tv` are **not** in it — read those from
`status --json` (or audibility from `vol --json`).

`bookmarks --json` and `accounts --json` are bare arrays. `queue --json` is an **object**:
`{"current": <index>, "items": [{"index","title","artist","album","duration_ms","art_url","current"}]}`
— indices are 1-based, and `play N` plays item `N`.

## Grouping — how `-r` resolves once rooms are joined

This is the highest-stakes thing to get right. When rooms are grouped:

- **`-r <any member>` acts on the whole GROUP.** `-r Kitchen pause` when Kitchen is grouped pauses
  the group; `-r Kitchen vol 20` sets the *group* volume; `-r Kitchen next` skips for the group.
  Addressing by the coordinator name does the same thing.
- **`--player` reads *and* writes one speaker.** `-r Kitchen vol --player` (no number) **reads** that
  single speaker's own volume; with a number it sets it. This is how you observe the balance inside a
  group — the `volume` on a grouped `status` entry is the group mix, and per-member volumes are not
  in `status`; read them all in one call by repeating `-r`:
  `x2rock -r Kitchen -r "Dining Room" vol --player`. `--player` does **not** apply to `mute` — it is
  refused always (code `unknown`), grouped or not: group mute is what people mean, and on a lone room
  plain `vol mute` already is that one speaker.
- **`--all` fans over groups, not raw rooms**, so a grouped pair is moved **once**, correctly:
  `--all vol -10` takes each group down 10, not each member (a grouped Kitchen+Dining does not go
  down 20). Read "every room" as "every group". `--all` is exclusive with `-r` (clap rejects both),
  and on a command that does not fan out it errors (code `unknown`) — except `bookmarks`, where
  `--all` means "include daemon-noticed history" instead.
- To act on a group, pass any member's or the coordinator's **real** room name — never the composite
  `"Dining Room + 1"`.

**`accounts` is this machine's tokens, and `accounts --household` is not the household's account
list either** - it reports the serials named by favorites and queue content, which is a *proxy*.
Measured against the Sonos app in one household it recovered **three of eight accounts and named
one that had been removed**: an account that has never played anything in the current favorites or
queue is invisible, and a serial outlives the account in content that still names it. The real
registry is not readable over the LAN. So treat it as evidence about content, never as "these are
the household's services", and never tell a user an account is missing on this basis.

**`update` reads firmware and never applies it.** It reports each speaker's installed version, the
version being offered and whether anything is pending - `up_to_date` comes from `download_bytes`
being 0 rather than from comparing version strings, because a current player is offered its own
version back. **x2rock will never install an update and must not claim it might**: that reboots
speakers, and the Sonos app gates it behind a dialog warning against unplugging anything - a warning
no command line carries. This is a permanent decision rather than a missing feature, so do not offer
to find a way round it and do not suggest `raw`, which speaks the Control API and cannot reach a
UPnP action anyway. Point the user at the Sonos app.

**Alarms are household-wide and addressed by id, not by room.** `alarms` lists every one with the
room it belongs to, so it takes no `-r`; `alarm <id> on|off` arms and disarms; `alarm <id> remove
--yes` deletes one, and `alarms add <time>` creates one. Turning an alarm to the state it is
already in is a no-op that says so. `recurrence` is `ONCE`, `WEEKDAYS`, `WEEKENDS`, `DAILY` or
`ON_<digits>` for named days with Sunday 0; `program` is a URI, and `x-rincon-buzzer:0` is the
built-in chime, which is what `alarms add` uses unless `--program` names a favorite or playlist.

Three things about alarms that will otherwise surprise a user:

- **The time is the household's, not the user's, and not this machine's.** `StartLocalTime` is
  local to the Sonos system. A household with no timezone configured runs on **UTC**, so
  `alarms add 07:00` can mean 07:00 UTC - the middle of the night several hours from the person who
  typed it. `alarms add` therefore prints the household's own clock on stderr:

  - `note: alarm times are the household's; its clock reads <time>` - compare that to the user's
    own clock. If they agree, the alarm is set for the hour they meant.
  - `note: this household has no timezone set, so 07:00:00 is UTC. Its clock reads <time>` - **say
    so plainly and do not silently convert.** The alarm will fire at that hour UTC. Setting the
    timezone fixes it for every controller at once, and is done in the Sonos app; x2rock has no
    command for it. Offer that rather than doing arithmetic on the user's behalf, because an alarm
    an agent quietly shifted is worse than one that is honestly wrong.

  Never relay "your alarm is set for 7" without having read that line: on an unset household it is
  the one claim most likely to be hours out.
- **An alarm sets the room's volume and leaves it there.** After one fires, the room stays at the
  alarm's level, not the level it had before. Say so if a user wonders why a room went quiet.
- **Removing a running alarm does not stop it**, and neither does disarming it. Use `pause`.
- **An alarm needs about two minutes of lead time.** One created less than that before its own
  start misses its scheduling slot and fires roughly two minutes late, so setting an alarm "for one
  minute from now" to demonstrate it will look broken. Its `--duration` also runs from the
  scheduled time rather than from when it actually started, so a late alarm plays for less than
  asked. When it ends the room is left `IDLE` on the program - unlike the sleep timer, which leaves
  it `PAUSED` keeping its place in the queue.

**The sleep timer stops the room when it runs out**, and is per group like transport. `sleep`
reads what is left, `sleep 30m` arms it, `sleep off` cancels. Bare digits are **minutes** (`sleep
45`), and `2h`, `1h30m`, `90s` and `HH:MM:SS` all work; a trailing number after a unit (`1h30`) is
refused as ambiguous rather than guessed at. `sleep --json` gives `{room, sleep_ms}`, null when
none is set, and the number is what the player reports rather than what was asked for - it starts
counting on acceptance, so a timer just set reads a second or two under. **`sleep_ms: 0` is not the
same as null**: zero means the timer has expired and the room is seconds from pausing - it was
observed reading zero while still playing for about seven seconds - while null means no timer is
set. When it fires the room **pauses**, keeping its place, so `play` resumes it.

**`eq` is per speaker, like `vol --player` and unlike everything else.** Bass and treble run
-10..10 (0 flat) and loudness is on/off; `-r` names the *speaker*, so a grouped room gets its own
tone rather than its group's, and `--all` does not apply to it. **Loudness is on from the factory**,
so a household nobody has adjusted is not flat - it is a low-frequency lift that does most of its
work at low listening levels, which is worth knowing before concluding a room is simply too loud at
volume 1. Reading takes four round trips and setting one per control; it is local either way.

**`--trueplay` is a fourth, different thing.** TruePlay is the room correction the iPhone app
measures and stores per speaker, applied *underneath* bass and treble - so a room can read flat
while it is being reshaped, and turning loudness off does not touch it. `eq --json` reports
`trueplay` beside `trueplay_available`, and both are needed: `trueplay` alone is a toggle that
reads on with nothing measured behind it. Turning it **on** when nothing is available is refused
rather than silently accepted. Worth knowing that a speaker which has moved rooms since it was
measured is applying a curve for the room it used to be in.

`group`/`ungroup`/`party` change the topology (see the command table). After a group change, the
topology takes a second or two to settle — re-read `status` rather than assuming. These are
**idempotent**: `party` on an already-partied house, `party off` when nothing is grouped, `ungroup`
a lone room, and `tv` on a room already on TV are all safe no-ops, not errors. Idempotent is not
consequence-free when the state *does* change — `party` and `ungroup` reach other people's rooms;
see "Ask before you act".

## Commands, by intent

| Intent | Command |
|---|---|
| See everything | `x2rock status --json` |
| What's playing (one room) | `x2rock now --json` |
| Transport | `x2rock play` / `pause` / `toggle` / `next` / `prev` |
| Play queue track N | `x2rock play N` |
| Volume | `x2rock vol --json` (read) / `vol 30` / `vol +5` / `vol mute` / `vol unmute` |
| Volume, one speaker in a group | `x2rock -r <Room> vol 20 --player` |
| Everywhere at once | `x2rock --all vol -10` (per-room commands only) |
| Repeat / shuffle | `x2rock repeat [all\|one\|off] --json` / `x2rock shuffle [on\|off] --json` |
| Crossfade | `x2rock crossfade [on\|off] --json` |
| Sleep timer | `x2rock sleep --json` (read) / `x2rock sleep 30m` / `x2rock sleep off` |
| Firmware check (read-only) | `x2rock update --json` |
| Alarms | `x2rock alarms --json` (list) / `x2rock alarm <id> on\|off` / `x2rock alarm <id> remove --yes` |
| Create an alarm | `x2rock -r <Room> alarms add 07:00 [--program "<favorite>"] [--recurrence daily] [--volume 25] [--off]` |
| Tone: bass, treble, loudness, TruePlay | `x2rock eq --json` (read) / `x2rock -r <Room> eq --bass 2 --loudness off --trueplay off` |
| Play a saved Sonos playlist | `x2rock playlist "<name-or-id>"` (replaces the queue) / `x2rock queue add` appends |
| The queue | `x2rock queue --json` / `queue remove N` / `queue clear --yes` (irreversible — see "Ask before you act") |
| Favorites | `x2rock favorites --json` (household-wide) / `x2rock -r <Room> favorite "<name-or-id>"` |
| Search a service | `x2rock search --json` (lists services) / `x2rock search -s <svc> <term> --json` |
| Browse a service | `x2rock browse -s <svc> [container] --json` |
| Play a stream by URL | `x2rock play-url "<http url>" [--title "<name>"] -r "<Room>"` |
| Find a radio station | `x2rock stations "<name>" --json` / `--tag jazz` / `--country GB` / `--play N -r "<Room>"` |
| Play a search/browse hit | `x2rock search -s <svc> <term> --play N -r "<Room>"` |
| Group rooms | `x2rock -r "<Coordinator>" group <Other> …` |
| Ungroup / party | `x2rock ungroup <Room>` (positional, no `-r`) / `x2rock -r "<Room>" party` / `x2rock party off` |
| Soundbar TV input | `x2rock -r "<Room>" tv` (only where `has_tv` is true) |
| Remember & replay | `x2rock keep` / `x2rock bookmarks --json` / `x2rock bookmark "<name>"` |
| Link an account | `x2rock link [service]` / `x2rock accounts --json` |

**A saved playlist is not a favorite.** `queue sources` lists both (playlists carry `SQ:` ids),
`queue save "<name>"` makes one from what is queued now, `queue add` appends one, and
`playlist "<name>"` *replaces* the queue with it and plays - the analogue of `favorite`. Playing a
playlist is idempotent: it replaces rather than appends, so running it twice does not double the
queue.

**`stations` is not a Sonos service and reaches past all of them.** `x2rock search` and
`x2rock browse` cover the 108 services the household's player knows about; `x2rock stations`
searches a community directory of tens of thousands of internet radio stations that wants no
account from anybody. See "Free radio" below - it is the answer to most "put something on" requests
that the household's own services cannot serve.

**`browse` reaches more services than `search` does.** `x2rock search` lists only the services that
publish a search category; `x2rock browse` lists every service reachable at all, which on this
household is twelve more. A service that answers `no_search_categories` is browse-only — walk it
with `x2rock browse -s "<service>"` rather than reporting it as unavailable.

Two shapes worth noting because they are inconsistent: **`favorite` is name/id-addressed**
(`favorite "Jazz"` or `favorite 37`), while a **search/browse hit is index-addressed** (`--play N`).
And **`group`/`ungroup` are asymmetric**: `group` takes `-r <coordinator>` plus room arguments;
`ungroup` takes the room *positionally* with no `-r`. **`favorites` (listing) is household-wide** —
`-r` is only meaningful on `favorite` (playing), to say which room.

## Free radio: find a station and play it

**There are tens of thousands of internet radio stations that need no account, no key and no
registration, and x2rock can play any of them.** `x2rock stations` searches a community directory
(Radio Browser) and `--play N` plays a hit; `x2rock play-url <url>` plays a stream URL directly.
Neither touches a Sonos login. This is usually the shortest path from "put something on" to sound,
and it works even when the household's own services have nothing.

**Treat almost any description of music as a search you should just run.** A genre, a mood, a
decade, a country, a city, a language, a format - all of it is a `stations` query or `--tag`. Do not
answer "the household has no country station" when eleven thousand of them are one command away.

```sh
x2rock stations --tag country --limit 5        # a genre
x2rock stations --tag "classic country"         # narrower; tags are free-form, so guess
x2rock stations "WFMU"                          # a station by name
x2rock stations --tag jazz --country FR         # jazz, from France
x2rock stations --tag ambient --limit 10 --json # to pick from with reasons
```

Then play it and **check that it actually played**:

```sh
x2rock stations --tag country --limit 5 -r "Media Room" --play 1
x2rock -r "Media Room" now                      # must say PLAYING
```

### Stations worth knowing, all verified working

Good defaults when someone wants something on and has not said what, and good anchors for "more
like this". Each plays with `play-url` as-is:

| Station | What it is | URL |
|---|---|---|
| SomaFM Groove Salad | Chilled ambient beats and grooves. The safest "just put something nice on". | `https://ice5.somafm.com/groovesalad-128-mp3` |
| SomaFM Drone Zone | Atmospheric ambient space music. Background, focus, sleep. | `https://ice2.somafm.com/dronezone-128-mp3` |
| Radio Paradise (Main Mix) | Hand-curated eclectic rock, world and jazz, 320k AAC. | `http://stream.radioparadise.com/aac-320` |
| WFMU | Freeform independent radio, East Orange NJ. Unpredictable by design. | `http://stream2.wfmu.org/freeform-128k` |
| FIP (Radio France) | Eclectic French public radio, famously genre-hopping. | `http://icecast.radiofrance.fr/fip-hifi.aac` |

```sh
x2rock play-url "https://ice5.somafm.com/groovesalad-128-mp3" --title "SomaFM Groove Salad" -r "Media Room"
```

These five are a starting point, not the catalogue. SomaFM alone has around thirty channels, Radio
Paradise has Mellow/Rock/Global mixes, FIP has Jazz/Reggae/Electro/Groove - so when one of these
lands well, **searching for its neighbours is the obvious next move**: `x2rock stations "SomaFM"`,
`x2rock stations "FIP"`, `x2rock stations "Radio Paradise"`.

### Offer more than one, and keep going if one fails

- **Two or three options beat one.** `--json` gives `codec`, `bitrate`, `country`, `tags` and
  `votes`; `votes` is the directory's popularity signal and a reasonable tiebreak. Naming why you
  picked one ("highest-voted, 320k, from France") is more useful than a bare confirmation.
- **A listed station can still fail to play, and the command now checks.** The player accepts a URL
  it cannot play and then sits at `IDLE` without erroring, so `play-url` and `stations --play` wait
  for the room to reach `PLAYING` before saying anything. Three outcomes, and they are worded apart:
  - `Media Room — X` means it is playing. Believe it.
  - `Media Room — X (starting)` means it was still buffering when the wait ran out. Not a failure -
    re-check with `x2rock now` before deciding anything.
  - a `stream_did_not_play` error, exit non-zero, means the room is still idle. **Try the next
    result**; do not report success and do not conclude radio is broken. The room is fine.

  A good station costs about four seconds of waiting and a dead one about ten. `--no-wait` skips
  the check and returns in milliseconds, but then the confirmation means nothing - it reports
  `"started": "starting"` and you own the verifying.
- **A null `stream_info` is normal, not a failure.** Many stations send no ICY metadata: `.977
  Country` plays perfectly and never names a track, so `title` is the station and `stream_info` is
  `null`. Others take a few seconds to send the first one, so a null immediately after starting
  means "not yet".
- **`hls: true` is the directory's claim, not a verdict.** Some HLS stations play and some do not.
  Treat it as one more thing to try, not a reason to skip a row.
- **It needs the internet**, not just the LAN - unlike the rest of x2rock. A `stations` failure on a
  network with working speakers usually means no route out.
- **A stream plays alongside the queue and leaves it alone**, so putting radio on does not disturb
  what was queued. Stopping the radio is `x2rock pause`.

## Worked examples

**"Play something in the kitchen."** `play` only *resumes*; a stopped room needs `favorite`.

```sh
x2rock status --json                      # Kitchen is IDLE, volume 2, audible:true
x2rock favorites --json                   # household favorites; pick a playable one
x2rock -r Kitchen favorite "Lo-Fi for Vampires Only"
x2rock -r Kitchen now --json              # confirm: state PLAYING, position advancing
```

**"Find and play a free country station."** No account needed and no reason to ask which service -
go and look. This is the real transcript of doing it:

```sh
$ x2rock stations --tag country --limit 5
  1  MP3 128k  US  .977 Country
  2  MP3 128k  GB  Radio Caroline
  3  MP3 128k  US  181.FM - Highway 181
  4  MP3 256k  CH  1.FM - Absolute Country Hits Radio
  5  AAC       CZ  Country Radio

$ x2rock stations --tag country --limit 5 -r "Media Room" --play 1
Media Room — .977 Country

$ x2rock -r "Media Room" now
PLAYING  .977 Country
```

The `--play` line only prints after the room reaches `PLAYING`, so it *is* the confirmation - a
dead stream errors with `stream_did_not_play` instead, and the answer to that is `--play 2`. Then
offer the neighbours rather than stopping: `--tag "classic country"`, `--tag bluegrass`,
`--tag "country pop"`, or `--country US --tag country` for American stations specifically.

**"Turn it down everywhere."** One call, no room names to derive:

```sh
x2rock --all vol -10
```

**"What's on in the house?"** — `x2rock status --json`, then read state/title/service/volume/audible/
on_tv per entry (a grouped entry covers all its members).

## Errors — act on the code, don't parse the sentence

A failed `--json` command prints to **stderr** and exits non-zero:

```json
{"error":"…human message…","code":"unknown_room","fix":"x2rock rooms"}
```

| `code` | meaning | `fix` |
|---|---|---|
| `unknown_room` | the `-r` name is not a room (or is a group's composite label) | `x2rock rooms` (and see `did_you_mean`) |
| `needs_link` | the music service needs an account | `x2rock link <service>` |
| `no_search_categories` | the service publishes no search categories — it is browse-only, not broken | `x2rock browse -s "<service>"` |
| `bad_stream_url` | `play-url` was given something that is not an `http`/`https` URL | null (only an http(s) URL can be a stream) |
| `stream_did_not_play` | the player took the stream URL and the room is still idle 10s later — the stream is almost certainly dead, the room is fine | null (try a different stream) |
| `no_player` | speakers were known here but none answered — a rescan already ran and found nothing | **null** (likely powered off; see below) |
| `unregistered_network` | this network has no known speakers — normal away from home | **null** (do *not* auto-scan; see below) |
| `too_many_rooms` | several `-r` on a command that takes one | null (re-run with one `-r`) |
| `unknown` | no known remedy — e.g. `pause` on an already-idle room, `--all` on a command that does not take it | null (read `error`) |

**When `fix` is non-null, run it and retry.** **When `fix` is null, do not — read the `error` and
change the request.** The two null network codes matter most: neither `unregistered_network` nor
`no_player` carries a fix, because the remedy people reach for — `x2rock discover` — must never be
run reflexively. Why, and what to do instead, is "When no speakers are available".

`unknown_room` carries extra detail so you need not re-fetch:

```json
{"code":"unknown_room","error":"no room named \"bedoom\"…","fix":"x2rock rooms",
 "did_you_mean":["Bedroom"],"rooms":["Bedroom","Living Room","Dining Room","Guest TV","Kitchen"]}
```

## When no speakers are available

A roaming laptop is often on a network with no Sonos — a café, an office, a guest network. **That is
normal, not a fault**, and almost always the answer when a command fails with `unregistered_network`
or the user is surprised nothing responds.

- **`x2rock status` diagnoses it:** `unregistered_network` (an unfamiliar network — the household is
  simply elsewhere) vs `no_player` (a *known* network where a rescan already ran and found nothing —
  the speakers are likely powered off, and another `discover` just repeats that scan, so re-check
  later rather than looping it).
- **A background daemon may be running** (Linux/MPRIS): it withdraws and reconnects on its own as the
  laptop moves networks, and logs the state — `journalctl --user -u x2rock.service` shows
  `x2rock: Kitchen -> org.mpris.MediaPlayer2.x2rock-…` when connected, or an hourly
  `unregistered network (gateway …)` when away. It is not required for anything you do from the CLI.
- **`discover` is offered, never reflexive** — it scans the local network, so run it only when the
  user confirms this is their own. Away from home, the answer is "your speakers aren't on this
  network", not a scan of it.

## When a field is a trap

- **`audible:false`** (muted or volume 0): a play succeeds but makes no sound. Say so; ask before
  unmuting/raising (never silently unmute in a shared house) — unless the intent is already loud.
  `audible:true` only means *not muted, not zero* — a room at `volume:2` is barely audible, not
  "loud enough".
- **`on_tv:true` + `input_format:"No Signal"`**: TV input selected, nothing playing. `favorite`/
  `play` switches it off TV — offer that. (`surround` is just whether the TV format is surround.)
- **`favorites --json` `"playable":false`**: an empty shell (dead service) — don't offer it.
- **Favorite drift**: a live service can silently reuse an id (iHeartRadio's holiday stations),
  undetectable. After a favorite, `now --json` and compare the title to the favorite name; flag a
  surprising mismatch, don't warn routinely.

## Ask before you act — it is a shared house

Several commands reach other people, so the *unrequested* ones deserve a check — but **a user who
names the action has already decided: run it, with no confirmation and no pros-and-cons.** "Ungroup
the kitchen" means ungroup the kitchen; "wake her up with music at full volume" *is* the
instruction. Confirm — one short question, never a debate — only when the risky part is your own
inference from a vague request:

- **`party`** as your reading of "play it everywhere"-ish — it captures every room; someone may be
  asleep in one.
- **A loud volume the user did not name** — a big jump or high absolute you derived (`vol 90`).
- **`queue clear`** as your reading of "clean it up" — irreversible; Sonos keeps no undo (hence the
  required `--yes`).
- **`ungroup` as your means to some other end** — the room drops back to its *first* track (it loses
  its place). Asked for directly, just do it; mention the lost place only when it clearly matters
  (mid-audiobook).

## Verifying, and latency

- **Confirm a play with `now --json`**: expect `BUFFERING` before `PLAYING`; an immediate read may
  still show `IDLE`/`BUFFERING`, so wait a second and re-check. `PLAYING` with `position_ms`
  advancing between two reads is real sound — subject to `audible`, which `now --json` does **not**
  carry: read it from the room's `status --json` entry or `vol --json`.
- **Warn on slow commands**: `discover` sweeps the subnet, `search`/`browse`/`link` reach the
  internet — seconds, not instant. Everything local (transport, volume, status, queue) is fast.

## Addressing a room

- `-r "<Room>"` names the room, **case-insensitively**; names come from `status`/`rooms`, and on a
  grouped entry use a `members`/`coordinator` name, not the composite label.
- **Always pass `-r` when the user named a room**; omit only for the whole house (unambiguous only
  in a single-group household — otherwise `--all`). x2rock does no natural-language mapping: resolve
  "the kitchen", "downstairs" to a room name yourself.
- A wrong room means loud music in the wrong place — accept a single high-confidence `did_you_mean`,
  confirm when unsure.
- **`-r` is repeatable** (per-room commands); **`--all`** does every room; any other command rejects
  several `-r` (`too_many_rooms`). A fan-out stops at the first failure, naming it — **the rooms
  before it already applied**, the ones after did not. Never re-run the whole batch after a partial
  failure (a relative `vol -10` would hit the finished rooms twice); redo only the rooms not reached.
- Volume is **relative** (`vol +5`/`-10`) or absolute (`vol 30`); a relative change **clamps at
  0/100**, never errors.

## What is safe to repeat

Agents retry; know what is idempotent. **Safe (no surprising effect):** `vol` set, `repeat`/`shuffle`
set, `group`. **Safe but not silent:** re-running `favorite "X"` **restarts the track from zero**;
`play` on an already-playing room is a no-op. **Errors, so don't blind-retry:** `pause` on an `IDLE`
room (code `unknown`); `next`/`prev` advance each call.

## Getting *into* the household's services: favorites, keep, bookmark

**`x2rock search` (no term) lists only the services searchable *without the household's account* —
radio-style anonymous ones plus what this machine has linked. It is not the household's real
services.** YouTube Music, Amazon Music and such are not searchable here (offering to "search
YouTube Music" fails with `needs_link` or finds nothing). Reach them three other ways:

- **`favorites`** — what the household saved in the Sonos app; `favorite "<name-or-id>"` plays one.
- **`keep`** — snapshots the **currently-playing track** (or `--container` for its album/playlist/
  station) into a *local* list, so it can be replayed later without a favorite. It is x2rock's own
  record, not a Sonos favorite.
- **`bookmark "<name>"`** — plays back something `keep` (or the daemon, automatically) recorded.
  `bookmarks --json` is a bare array: `[{id, name, type, service, description, art_url}]`. By default
  it lists only what was kept on purpose; `bookmarks --all` (here meaning "include daemon-noticed
  history", not whole-house) adds what the daemon noticed playing — the answer to "that thing from
  yesterday".

## `raw`, and its boundary

`x2rock raw` speaks the Sonos Control API directly and **can mutate state** — high blast radius. Use
it **only** when the user explicitly asks for raw access, or when no first-class command covers the
intent. **Never route around an error with it** — a `needs_link` or an unsupported request should be
*reported*, not bypassed. `x2rock raw --help` documents namespaces and scopes.

## More detail

Every command has `x2rock <command> --help`.
