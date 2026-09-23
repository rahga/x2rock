---
name: x2rock
description: Control Sonos speakers from the command line with the `x2rock` CLI — play, pause, skip, per-room and whole-house volume, evening out a group's volume, mute, shuffle and repeat, the queue, favorites, music-service search, browse and account linking, rating a track, internet radio by URL, grouping and party mode, soundbar TV input, alarms and snooze, the sleep timer, per-speaker tone controls (bass, treble, loudness, TruePlay), saved playlists, and a one-call JSON snapshot of the whole household. Use whenever the user wants to control Sonos or speakers — "play/put on <something> in <room>", "pause", "skip", "turn it up/down", "quieter/louder everywhere", "even out the volume", "mute the kitchen", "shuffle", "what's playing / what's on", "group these rooms", "play this stream/radio URL", "find me a country/jazz/ambient station", "put on some free radio", "play everywhere / party", "thumbs up this song", "link my Spotify / can you search Amazon Music", "set an alarm", "snooze / turn off my alarm", "sleep timer / stop in 30 minutes", "turn up the bass", "is loudness on", or switch a soundbar to TV.
---

# Driving Sonos with the `x2rock` CLI

`x2rock` controls Sonos speakers on the local network, with **no Sonos account**. Control never
leaves the LAN; `search`, `browse`, `link`, `rate`, `play-item` and `stations` do reach out to music
services and a radio directory, none of which want a Sonos login. Every command is a one-shot
subprocess.

When speakers seem missing, `x2rock status` diagnoses it (see "When no speakers are available"). 
Two contracts hold everything together:

1. **Run `x2rock status --json` first.** It is the whole household in one call, and you cannot write
   a correct room- or group-aware response without it.
2. **Read fields, never prose.** Every data command takes `--json`; a failure prints a JSON
   `{error, code, fix}` on stderr. The wording changes; the JSON shape does not.

x2rock typically installs with a **background daemon that may be running**, used to publish 
'now-playing' to the Linux desktop over MPRIS. It is *not* needed for anything you do from the CLI. 

`x2rock --version` prints the version. This skill ships embedded in that binary, so it matches the
CLI it came from — if the version has moved since you installed the skill, re-run `x2rock skill`.

## The shapes you will read

### `status --json` — a bare array, one object per group

```json
[
  {
    "room": "Kitchen", "state": "PLAYING", "title": "Solitude",
    "artist": "…", "album": null, "podcast": null, "position_ms": 41000, "duration_ms": null,
    "queue_position": 3, "explicit": false, "crossfade": false,
    "next_title": "Blue in Green", "next_artist": "Miles Davis",
    "service": "YouTube Music", "service_id": "284", "art_url": "http://…",
    "volume": 2, "muted": false, "audible": true, "fixed": false,
    "repeat": "off", "shuffle": false,
    "on_tv": false, "has_tv": false, "input_format": null, "surround": null,
    "stream_info": null,
    "members": ["Kitchen"], "coordinator": "Kitchen"
  },
  {
    "room": "Bedroom", "state": "PLAYING", "title": "TV Audio",
    "on_tv": true, "has_tv": true, "input_format": "Dolby Digital 2.0", "surround": false,
    "service": null, "service_id": null, "volume": 12, "muted": false, "audible": true, "fixed": false,
    "members": ["Bedroom"], "coordinator": "Bedroom", "repeat": "off", "shuffle": false
  },
  {
    "room": "Dining Room + 1", "state": "PLAYING", "title": "Señorita",
    "service": "Plex", "service_id": "212", "volume": 1, "muted": false, "audible": true, "fixed": false,
    "members": ["Dining Room", "Kitchen"], "coordinator": "Dining Room",
    "repeat": "off", "shuffle": false, "on_tv": false, "has_tv": false
  }
]
```

- Now-playing is **flat** on the room object (`title`, `artist`, `album`, `position_ms`,
  `duration_ms`, `next_title`, `next_artist`), not nested.
- **A podcast episode has a show, not an album or an artist.** `podcast` carries the show's name,
  and `album` repeats it so anything reading `album` still has something to show; `artist` stays
  `null`. A non-zero `podcast` is how to tell an episode from a track.
- **`queue_position` is 1-based and has no total.** It is `null` whenever the queue is not what is
  driving - a radio stream has no position in a queue - and `0` when the queue is the source but
  empty. For the length, and for whether the queue is the source at all (`in_use`), read
  `queue --json`. `explicit` is the content flag every controller shows as a badge, `null` when the
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

`now --json` is a **single bare object** — one room's *now-playing* fields (room, state, title,
artist, album, podcast, service, service_id, position_ms, duration_ms, queue_position, next_title,
next_artist, explicit, stream_info, repeat, shuffle, crossfade, on_tv, input_format, surround,
art_url), with no `-r` picking the household's one group (and erroring if there are several). It is
the confirm-step after a play. It is a **subset** of a `status` entry: `volume`, `muted`,
`audible`, `fixed`, `members`, `coordinator` and `has_tv` are **not** in it — read those from
`status --json` (or audibility from `vol --json`).

`bookmarks --json` is a bare array of `{id, name, type, service, description, art_url}`.
`accounts --json` is a bare array of `{service, service_id, account_id, nickname, linked,
household}` — see "Linking a music service". `queue --json` is an **object**:
`{"total": <n>, "current": <index>, "in_use": <bool>, "items": [{"index","title","artist","album","duration_ms","art_url","current"}]}`
— indices are 1-based, and `play N` plays item `N`. **`in_use` is whether the queue is the group's
source** — the Sonos app's "Queue" versus "Queue (Not In Use)". When it is `false` the group is on a
stream, TV or line-in, `current` is `0` and no item is current, but the items are still listed and
`play N` switches back to the queue. An empty queue can still be in use (`total: 0`,
`in_use: true`). A change to the queue reports what it became rather than the whole queue:
`queue add --json` gives `{room, added, source, total}`, `queue remove` `{room, removed, total}`,
`queue clear` `{room, total}`, `queue move` `{room, from, to}` and `queue save` `{room, name, id}`.

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
- **`--ramp` slides to the new level instead of jumping** — `vol 30 --ramp`, about a second and a
  half per ten steps, and the room is left at the new level. It **slides one speaker at a time and
  implies `--player`**, because the group volume service has no ramp action at all — but it composes
  with the fan-outs that are themselves per speaker: `-r Kitchen -r Bedroom vol 20 --ramp` slides
  both, and `--each` slides every member of one group. **`--all` is the one refusal**, since that
  fans over group coordinators and would slide one speaker per group while calling it the group.
  `mute` is refused too — there is no level to slide to. It does **not** unmute the way a plain set
  does, so a muted speaker slides silently; the command says so on stderr.
- **`--each` sets every speaker in one group individually** — `-r "Living Room" vol 30 --each`
  puts every grouped room at 30, flat. The plain group `vol 30` scales instead, preserving the
  members' balance the way the Sonos app does, so `--each` is the way to *erase* that balance in one
  call (the manual equivalent is setting the group to 0 and back up, which the flag replaces). It
  reads the members from the current grouping, takes one group only (no `--all`, one `-r`), is
  exclusive with `--player`, and is refused for `mute`. Distinct from `--all`, which is per-group
  across the household, and from repeating `-r … --player`, which is the same effect but needs every
  member named.
- **`vol normalize` evens a group out at the level it already has** — the Sonos app's "Normalize
  Group Volume". The group volume is the rounded average of its members, so members at 5, 5, 5 and
  0 read as a group at 4, and `normalize` sets all four to 4, leaving the group level unchanged. An
  exact half rounds *down* (6 and 1 read as 3; 9 and 2 as 5), so do not predict the level - read it. It
  is the `--each` flatten with no number to pick, and the right reading of "even out the volume" or
  "make the grouped rooms match". To know whether it would do anything, read the group: a group
  `vol --json` carries **`balanced`** — `false` when some member differs (the prose line says so
  too), `true` for a lone room, and `null` after a set or with `--player`. Members already at the
  level are left alone; fixed-volume members are skipped. `--json` adds `members`, each
  `{room, volume, previous_volume}`. `--all vol normalize` normalizes every group. It refuses
  `--player`, `--ramp` and `--each`.
- **`--all` fans over groups, not raw rooms**, so a grouped pair is moved **once**, correctly:
  `--all vol -10` takes each group down 10, not each member (a grouped Kitchen+Dining does not go
  down 20). Read "every room" as "every group". `--all` refuses a typed `-r` (code `unknown`), but
  an exported `X2ROCK_ROOM` is set aside rather than fought, so `--all vol -10` works in a shell
  that took `x2rock rooms` up on its `export`. On a command that does not fan out it errors (code
  `unknown`) — except `bookmarks`, where `--all` means "include daemon-noticed history" instead.
- To act on a group, pass any member's or the coordinator's **real** room name — never the composite
  `"Dining Room + 1"`.

**`accounts` is this machine's tokens** (see "Linking a music service"), **and `accounts --content`
is not the household's account list either** - it reports the serials named by favorites and queue
content, which is a *proxy*.
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
to find a way round it. `raw upnp` could technically send `BeginSoftwareUpdate`; do not suggest it
and do not run it, even when asked in passing - point the user at the Sonos app.

**`system` is the only command that speaks in speakers rather than rooms**, and it is what to run
for "what speakers do I have", "what model is X", "how is the living room set up" or anything about
firmware, hardware or bonding. Everything else here hides bonding deliberately - a room is a room
whether one speaker or four back it - so `status` and `rooms` cannot answer those. It is the same
readout the Sonos apps call **About My System**, and it is read-only and local.

Each entry is one *player*: `room`, `model`, `model_number`, `role`, `channels`, `bonded`,
`satellite`, `hidden`, `serial`, `uuid`, `sonos_os`, `display_version`, `build`,
`software_version`, `hardware_version`, `series_id`, `ip`, `connection`, `connection_type`,
`eth_link`. Four of those need care:

- **`connection` is how that speaker reaches the household**, and it is the first thing to read when
  several rooms drop out at once: `wired`, `sonosnet`, `satellite`, or `unknown`. As soon as one
  speaker has an ethernet cable the others typically leave your WiFi for **SonosNet**, Sonos's own
  mesh bridged through the wired one - so a single `wired` player can be what four other rooms
  depend on, and "the WiFi is fine" is not the same question. `satellite` is the private link a
  home-theatre surround or Sub holds to its soundbar. **It is a link, not a bond**: a stereo-pair
  half is bonded and still reads `sonosnet`, so `bonded`/`role` remain the fields for that.
  `connection_type` is the raw number beside it, because the words cover only values seen on real
  hardware - a speaker joined to home WiFi has never been observed here, so an unseen number reads
  `unknown` rather than being guessed at.

- **`role` is the app's bonding label** - `LS`/`RS` for surrounds, `L`/`R` for the halves of a
  stereo pair, and `null` for a Sub, for a soundbar carrying both front channels, and for a speaker
  bonded to nothing. Do not read `null` as "not bonded"; read `bonded` for that.
- **A room can be on two firmwares at once.** A satellite's version appears here and nowhere else,
  so a Sub or surround left behind by an update is invisible to `x2rock update`, which walks rooms
  and reports only the coordinator. If someone asks why a room sounds wrong after an update, this
  is the command that can see it.
- **`serial`, `ip` and `uuid` identify hardware** - a RINCON uuid embeds the speaker's MAC
  verbatim. Before putting this output anywhere public - a bug report, an issue, a paste site -
  use `--redact`, which masks all three. The household id is never printed either way.

**Alarms are household-wide and addressed by id, not by room.** `alarms` lists every one with the
room it belongs to, so it takes no `-r`; `alarm <id> on|off` arms and disarms; `alarm <id> remove
--yes` deletes one, and `alarms add <time>` creates one. Turning an alarm to the state it is
already in is a no-op that says so. `recurrence` is `ONCE`, `WEEKDAYS`, `WEEKENDS`, `DAILY` or
`ON_<digits>` for named days with Sunday 0; `program` is a URI, and `x-rincon-buzzer:0` is the
built-in chime, which is what `alarms add` uses unless `--program` names a favorite or playlist.
Beside `room`, each entry carries **`room_id`, the raw `RINCON_...` uuid** - the only thing naming
the room when `room` is `null` because that speaker is off the network. It embeds the speaker's MAC,
and `alarms` has no `--redact` the way `system` does, so strip it yourself before putting alarm JSON
in a bug report or a paste.

Several things about alarms that will otherwise surprise a user:

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
- **An alarm sets the room's volume and leaves it there, and ramps up to it.** After one fires,
  the room stays at the alarm's level, not the level it had before. Say so if a user wonders why a
  room went quiet. It *reaches* that level by ramping over **roughly ten to fifteen seconds**, so a
  volume read in the first seconds after firing is really lower than the alarm's setting - measured
  climbing 6 -> 16 for a `--volume 16` alarm. Never report that as `--volume` being ignored; wait
  and re-read.
- **Removing a running alarm does not stop it**, and neither does disarming it - `alarm <id> off`
  stops it scheduling *again*, not the one already sounding. **`x2rock -r <Room> snooze` is the
  command for an alarm that is going off**, silencing it for nine minutes by default (`snooze 20m`
  for longer); `pause` stops it outright without it coming back. Snooze leaves the room `PAUSED`
  keeping its place, and a snoozed alarm still reads as running, so snoozing again is allowed and
  is how someone hits the button twice.
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

**`led` and `buttons` are the physical speaker, not what it is playing.** `led` is the status
light - the reason to want it off is a speaker in a bedroom - and `buttons` locks the touch controls
on the box, which is the Sonos app's "Button Control" and does **not** affect playback over the
network. Both are **per speaker** like `eq`, so `-r` names the speaker and a stereo pair has two of
each. `buttons` takes `lock`/`unlock` rather than on/off on purpose: "buttons on" means both "they
work" and "the lock is on", and the wire and the app disagree about which. Note the player accepts
*any* string for these and treats everything but `Off` as on, so x2rock validates and a typo is
refused rather than quietly meaning "on".

**`remote` is a soundbar's relationship with the TV remote**, and is refused on anything without a
TV input. `--feedback` is the acknowledgement flash when a remote command lands - **a different light
from `led`**, which is the speaker's own status LED, so do not treat the two as the same setting.
`--repeater` is infrared pass-through to the TV behind the bar, which is what to reach for when a
soundbar parked in front of the TV swallows its remote. Reading also reports whether a remote has
been taught to the bar at all; teaching it one is an interactive press-the-button flow that stays in
the Sonos app. `repeater` reports a **word**, not a boolean - `On`, `Off`, or the `Disabled` the
service documents (a Beam refuses to be set to it).

**`rename` changes the room's name for everyone** - every Sonos app in the house, every controller,
and every script addressing it by name. It is reversible (rename it back) and disturbs nothing that
is playing, but it is not a local preference, so treat an unrequested one as the kind of change to
confirm first. Two guards are worth knowing: a name another speaker already has is **refused**,
because `--room` could not then tell them apart, and the room's icon and configuration are preserved
rather than guessed at - a Dining Room was found here carrying the `living` icon, so deriving one
from the new name would have quietly changed it.

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

**On a soundbar, `eq` also reads and sets night mode and dialog enhancement.** They show as
`night_mode`, `dialog_enhancement` and `dialog_level` in `--json`, and `night` / `dialog` in the
prose line; `-r "Living Room" eq --night on` and `--dialog on|off` set them. Reads come over the
Control API (`settings:1 getPlayerSettings`) and writes go over UPnP `SetEQ` - the Control API
carries these but refuses to write them, so the two halves use different transports, which agree.
Both are on/off here. They apply **only to a room with a TV input**: a non-soundbar omits them from
the read and *refuses* `--night`/`--dialog` with a clear message rather than an opaque UPnP error.
So `eq` both answers "is night mode on in the living room?" and turns it on.

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
| Flatten a group: every member to one level | `x2rock -r <Room> vol 30 --each` |
| Even a group out at its own level (app's "Normalize") | `x2rock -r <Room> vol normalize` (check `balanced` in `vol --json` first) |
| Fade instead of jumping | `x2rock -r <Room> vol 30 --ramp` — composes with several `-r` and with `--each`, not with `--all`; `ramp_seconds` is null when the player does not say |
| Everywhere at once | `x2rock --all vol -10` (per-room commands only) |
| Repeat / shuffle | `x2rock repeat [all\|one\|off] --json` / `x2rock shuffle [on\|off] --json` |
| Crossfade | `x2rock crossfade [on\|off] --json` |
| Rate the current track up/down | `x2rock -r <Room> rate up\|down [--refresh] [--json]` — only where the service offers it (Pandora-style radio, iHeartRadio Custom Stations); see "Rating a track" |
| Sleep timer | `x2rock sleep --json` (read) / `x2rock sleep 30m` / `x2rock sleep off` |
| Silence an alarm that is sounding | `x2rock -r <Room> snooze [9m] [--json]` - nine minutes by default; it *acts* rather than reads |
| Firmware check (read-only) | `x2rock update --json` |
| What the household is made of | `x2rock system --json` (add `--redact` to paste it anywhere) |
| Which Sonos household(s) are reachable | `x2rock households --json` — only matters with more than one; see "Addressing a household" |
| Alarms | `x2rock alarms --json` (list) / `x2rock alarm <id> on\|off` / `x2rock alarm <id> remove --yes` |
| Create an alarm | `x2rock -r <Room> alarms add 07:00 [--program "<favorite>"] [--recurrence daily] [--volume 25] [--off] [--json]` — `--json` returns the created alarm as the same object `alarms --json` lists, so keep its `id` for `alarm <id> off` |
| Tone: bass, treble, loudness, TruePlay (+ night/dialog on a soundbar) | `x2rock eq --json` (read) / `x2rock -r <Room> eq --bass 2 --loudness off --trueplay off` / `eq --night on --dialog on` |
| Rename a room | `x2rock -r <Room> rename "<New Name>"` — `--room` is required; changes it for every app in the house |
| Speaker status light | `x2rock -r <Room> led [on\|off] [--json]` |
| Soundbar TV-remote settings | `x2rock -r <Room> remote [--feedback on\|off] [--repeater on\|off] [--json]` |
| Lock the buttons on the speaker itself | `x2rock -r <Room> buttons [lock\|unlock] [--json]` |
| Play a saved Sonos playlist | `x2rock playlist "<name-or-id>"` (replaces the queue) / `x2rock queue add` appends |
| The queue | `x2rock queue --json` / `queue remove N` / `queue clear --yes` (irreversible — see "Ask before you act") |
| Favorites | `x2rock favorites --json` (household-wide) / `x2rock -r <Room> favorite "<name-or-id>"` |
| Search every service at once | `x2rock search "<term>" --json` — no `--service`; `--only-linked` for the good tier, `-c artists,tracks` to choose categories, `--per-service N` to cap each |
| Search one service | `x2rock search -s <svc> <term> --json` / `x2rock search --json` (lists services) |
| Browse a service | `x2rock browse -s <svc> [container] --json` |
| Page through either | add `--count N --index N` — `--json` answers `{total, index, items}`. A merged search (no `-s`) adds `asked`, `searches`, `answered`, `slow`, `refused`: read `slow`/`refused` before concluding a service has nothing, and expect those services named on stderr beside the JSON |
| Play a stream by URL | `x2rock play-url "<http url>" [--title "<name>"] -r "<Room>"` |
| Find a radio station | `x2rock stations "<name>" --json` / `--tag jazz` / `--country GB` / `--play N -r "<Room>"` |
| Play a search/browse hit | `x2rock search [-s <svc>] <term> --play N -r "<Room>"` — `N` counts the merged list |
| Play or queue a hit you already have the id for | `x2rock -r "<Room>" play-item -s <svc> <id> --title "<name>" --kind <type>` / `queue-item` (same arguments; adds without playing, refuses a stream) |
| Queue a whole album or playlist | the same commands with `--kind album` or `--kind playlist` — the player expands it into the queue. An `artist` is refused: it holds albums and playlists rather than tracks, so browse it and queue what is inside |
| Group rooms | `x2rock -r "<Coordinator>" group <Other> …` |
| Ungroup / party | `x2rock ungroup <Room>` (positional, no `-r`) / `x2rock -r "<Room>" party` / `x2rock party off` |
| Soundbar TV input | `x2rock -r "<Room>" tv` (only where `has_tv` is true) |
| Chime / announce over playback | `x2rock -r "<Room>" chime` / `x2rock -r "<Room>" notify "<http url>" [--volume N]` |
| Remember & replay | `x2rock keep` / `x2rock bookmarks --json` / `x2rock bookmark "<name>"` / `bookmarks pin|rename|prune|remove` |
| Link a music service (a person finishes a browser login) | `x2rock link '<Service>' [--no-open]` / `x2rock accounts --json` / `x2rock unlink '<Service>'` — see "Linking a music service" |
| Link with no browser at all, from what the household already holds | `x2rock link --from-household ['<Service>']` — the only route to Qobuz, Apple Music and Amazon; needs inbound TCP 3401 from the player |
| See what the household holds, without keeping it | `x2rock link --from-household --dry-run` — every account record and attribute, tokens shown only as byte lengths |
| Forget tokens | `x2rock unlink '<Service>'` (every household, every account) / `--household <id>` (one) / `--account "<nickname>"` (one account) / `x2rock unlink --all` (wipe) |
| Choose which account a service uses | `x2rock accounts --prefer '<Service>' "<nickname>"` — only where a household holds two accounts for it; `x2rock accounts` marks the current one `*` |
| Shell completions | `x2rock completions [shell] [--install\|--uninstall]` — auto-detects shell when omitted |
| Systemd user service | `x2rock service [status\|install\|uninstall] [--json]` |
| Desktop integration (.desktop & icon) | `x2rock desktop [status\|install\|uninstall] [--json]` |

**A saved playlist is not a favorite.** `queue sources` lists both (playlists carry `SQ:` ids),
`queue save "<name>"` makes one from what is queued now, `queue add` appends one, and
`playlist "<name>"` *replaces* the queue with it and plays - the analogue of `favorite`. Playing a
playlist is idempotent: it replaces rather than appends, so running it twice does not double the
queue.

**`stations` is not a Sonos service and reaches past all of them.** `x2rock search` and
`x2rock browse` cover the music services the household's player knows about - about a hundred, and
that list is Sonos's ceiling. `x2rock stations` searches a community directory of tens of thousands
of internet radio stations that wants no account from anybody. See "Free radio" below - it is the
answer to most "put something on" requests the household's own services cannot serve.

**A term with no `--service` searches everything at once.** `x2rock search "travis scott"` asks
every service that can answer, concurrently, and merges the results; the rows name their service and
their category, and `--play N` counts down the merged list. **Linked services sort first, and prefer
them**: that tier has real albums, metadata the service vouches for, and content a player will
*queue* rather than stream. The anonymous tier is radio stations and blog aggregators - Hype Machine
carries no albums at all by construction, its titles come from the blog post rather than the file,
and its links rot. `--only-linked` skips the tail.

**Each service is asked in several categories at once, and its answers are interleaved**, so three
rows from one service are a track, an artist and an album rather than three tracks. Unasked, that is
the service's `all` where it declares one (Sonos's Universal Search marker, which few services set),
else `tracks`/`artists`/`albums` where it has them, else whatever it lists first - which is what
keeps a stations-only service answering. `--category` takes a list in priority order
(`-c artists,tracks`) and **skips a service that has none of those names** rather than substituting
one, so `-c albums` asks only services that really have albums. `--all-categories` asks each service for
*everything* it publishes, which is the only way to reach a category Sonos never
standardised - Hype Machine searches blogs, Sveriges Radio searches radio shows, and those
are named by the service so no fixed list can name them. `--per-service N` caps the rows one
service contributes after interleaving (default 3, the number Sonos's mobile app shows under a
service heading; `0` keeps everything), and `--count` is per service *per category* (5 merged, 20
for one). First run is slower: services never searched before are asked for their categories once,
and the answer is cached.

**`search --json` and `browse --json` answer an envelope, not a bare array**: `{total, index,
items}`. Read `items`. **`total` is the service's whole count and `items` is one page of it**, so
there is more whenever `index + items.len() < total` - ask for it with `--index`, which is 0-based
(`--count 20 --index 20` is the second page). Without this a caller that got exactly `--count` rows
could not tell a full container from a truncated one. Two commands that look similar do **not**
share this shape: `favorites --json` and `accounts --json` are still bare arrays.

**`--play N` counts within the page, not the whole result set.** `--index 20 --play 1` plays the
21st item overall. This is the one place paging can surprise: the number to pass is the row's
position in `items`, exactly as printed.

**`browse` reaches more services than `search` does.** `x2rock search` lists only the services that
publish a search category; `x2rock browse` lists every service reachable at all, which is a dozen or
so more - run both rather than assuming they match. A service that answers `no_search_categories` is
browse-only, not broken: walk it with `x2rock browse -s "<service>"` rather than reporting it as
unavailable.

**A container cannot be played whole, and this is a limit of Sonos rather than of x2rock.** An album,
a playlist, a show - anything `browse` marks `"container": true` - can be *opened* and its tracks
played one at a time, and `--play N` on the container itself is refused. Do not go looking for a
flag that plays it: there is no way to do it over the local network at all. Four routes were tried
against real hardware and all four fail, including replaying a player's own stored favorite URI back
to it verbatim (see docs/architecture.md). **The way through is to say so and name the workaround:
saved in the Sonos app as a favorite, the same container plays fine with `x2rock favorite "<name>"`,
because that hands the player an id and lets it resolve the thing itself.** The error message says
this too; do not promise to find a way round it.

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
answer "the household has no country station" without looking: the directory holds roughly 59,000
stations under some 12,000 tags, and `--tag country` returns about **800** of them. Measured
2026-09-04 by running the query this command runs - `jazz` ~1400, `ambient` ~450, `classic country`
~65, `bluegrass` ~44. (A tag matches loosely, so these are larger than the count of stations
carrying that exact tag; what matters is what the command returns.) Tags are free-form, so a narrow
guess that comes back empty is a reason to widen it, not to give up.

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

- **Two or three options beat one - but make them different stations, and the obvious way of doing
  that does not work.** The directory lists every bitrate and codec as its own row *and puts them
  in the name*, so `SomaFM Secret Agent (128k MP3)` and `SomaFM Secret Agent (32k AAC)` are two
  names for one channel. Measured: `SomaFM` returns **122 rows, 119 distinct names, and 54 actual
  channels**; `FIP` returns 90 rows, 53 names, 41 channels. **De-duplicating on the name collapses
  122 to 119 and achieves nothing** - strip a trailing `(...)` qualifier first, then group. Also
  raise `--limit` (it defaults to 20) before concluding a station has few variants, or you are
  de-duplicating a page rather than the results. `--json` gives `codec`, `bitrate`, `country`,
  `tags` and `votes`; `votes` is the directory's popularity signal and a reasonable tiebreak, and
  naming why you picked one ("highest-voted, 320k, from France") beats a bare confirmation.
- **A listed station can still fail to play, and the command now checks.** The player accepts a URL
  it cannot play and then sits at `IDLE` without erroring, so `play-url` and `stations --play` wait
  for the room to reach `PLAYING` before saying anything. **Four outcomes**, worded apart because
  the right next move differs:
  - `Media Room — X` means it is playing. Believe it.
  - `Media Room — X (starting)` means it was still buffering when the wait ran out. Not a failure -
    re-check with `x2rock now` before deciding anything.
  - a `stream_did_not_play` error, exit non-zero, means the room is still idle. **Try the next
    result**; do not report success and do not conclude radio is broken. The room is fine.
  - a `stream_unverified` error means the room's state could not be established, so whether it is
    playing is unknown. **Do not try another station** - this is not a verdict on the stream, and
    swapping it wastes another ten seconds. The message says which of two things happened: the room
    never answered (find out why before anything else), or it answered every poll without naming a
    state (the room is reachable - just re-check with `x2rock now`). (Distinct from a plain
    `no_player`, which means no speaker answered *before* anything was loaded.)

  With `--json`, both commands emit `{room, title, url, started}` on success (`started` is
  `"playing"` or `"starting"`) and the standard `{error, code, fix}` on either failure. A good
  station costs about four seconds of waiting and a dead one about ten - worth saying if someone is
  watching a prompt. `--no-wait` skips the check and returns in milliseconds, but then the
  confirmation means nothing: it reports `"started": "starting"` and you own the verifying.
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

## Addressing a household — only when there is more than one

**An ordinary home has exactly one Sonos household, and everything above this section already
covers it.** Do not run `x2rock households` or reach for `--household` speculatively "to be safe" -
it costs a network scan for a case that essentially never applies at home. This section is for the
one setting it actually comes up: an office, lab, or showroom running two or more separate Sonos
systems on the same network (test households included).

- **A room lives in exactly one household, never merged.** Reaching one player is enough for its
  *own* household's topology (`getGroups` reports the rest) - but a second, unrelated household on
  the same LAN is invisible to that call. If two households happen to share a room name (both have a
  "Kitchen"), nothing about `-r Kitchen` alone can tell them apart.
- **You will only ever see this as an error, never as something to check for up front.** Every
  command that resolves a household surfaces `multiple_households` (nothing was said which one) or
  `unknown_household` (something was said and it did not match) exactly like `unknown_room` -
  `{error, code, fix, households: [{id, rooms}, ...]}`. Read `data.households` rather than re-running
  `x2rock households` yourself when the error already handed you the list.
- **`-r <room>` already picks the household.** A room name belongs to exactly one household unless
  the two systems share it, so `x2rock -r Studio play` works on a two-household network with nothing
  else typed - the room *is* the selector, and this is enough whenever the two systems' rooms are
  named differently (the common case). `--household` is only needed in two places: a command that
  names no room at all (the daemon, `link`, `accounts`), where `--household <room>` names one for
  it; and a room name that is itself what collides (both households call something "Kitchen"),
  where only `--household <id>` can say - `x2rock households` prints the ids (`--redact` masks
  them). Two other outputs carry a household id too: `status --json --full` (`household`) and
  `accounts --json` (`household`, the one a token was minted against). Treat all three as
  identifying before pasting them anywhere public.
- **A machine that moves between households holds a token per household.** The credential store is
  keyed by household, so the office's Qobuz and the home system's are separate accounts and neither
  is used on the other's network. A service that reads as unlinked after moving is that working, not
  a lost token — see "Linking a music service".
- **A household that has been replaced is forgotten on its own.** After a factory reset or a
  replaced system, the old household id would sit beside the new one with the same room names and
  make every command ask which. So a scan that finds *every* remembered address of a household now
  answering for a different household forgets the old one; a household that merely did not answer
  (powered off) is kept. There is nothing to hand-edit, and no need to tell the user to.
- `x2rock households [--json] [--redact]` always scans fresh (like `discover`, unlike `status`),
  because the whole point is telling two systems apart *right now*.
- `--household` is a **global** flag, same footing as `-r`/`--all`/`--ip`, and env-settable as
  `X2ROCK_HOUSEHOLD`. It is ignored alongside an explicit `--ip`, which already names one player
  unambiguously.

## Rating a track: `x2rock -r <Room> rate up|down`

**Only a Pandora-shaped radio feature offers this, and only on a track that has one.** A **Live**
broadcast station cannot be rated **at all**, structurally - not "usually," not "rarely," not a
matter of the service being unhelpful. Verified live against a real household (2026-09-12): a Live
station's current track (`x-sonosapi-stream:live_stations.…`, `upnp:class
object.item.audioItem.audioBroadcast`) carries **no per-track id whatsoever** - the *station* has
one, the song currently airing on it does not, because a live simulcast has an ID3-derived
now-playing label, not a per-listener SMAPI identity. iHeartRadio's **Custom/Artist-Radio** stations
(a different content type, reached via `browse -s iHeartRadio artist_radio.<id>`, not a station
favorite) do give each track a real id and were rated successfully in the same session - both
directions, confirmed against the real service (`{"message":"THUMBS_UP_SUCCESS", "should_skip":
false, ...}`, matching iHeartRadio's own presentation map). **Do not tell a user rating "should"
work on whatever is currently playing without checking** - a `rate` refusal naming "no id to rate"
is the room correctly reporting a Live station, not a bug to work around.

- **The rating id is not a fixed constant per direction.** iHeartRadio hands out a *different* id
  for "thumbs up" depending on whether the track is currently unrated, already up, or already down
  - `x2rock` reads the track's live rating state from `getExtendedMetadata` and looks up the
    matching id every time; never assume "5 means up."
- **`AutoSkip` is the service's declared policy, not what happened this time.** iHeartRadio
  declares `AutoSkip="NEVER"` on every rating it offers, and its real responses confirm it:
  `should_skip` comes back an explicit `false`, not absent. A different service may set it and mean
  it - check the live `should_skip` field in the response, which `x2rock rate` already acts on
  (skipping the track immediately when `true`), not the declared policy.
- No account, no favorite and no fixed id list is involved: this reaches the *music service's* own
  server (SMAPI), the same way `search`/`browse` do, and needs whatever token the service already
  requires (`needs_link` if the service is not linked at all).
- **"<service> publishes no ratings" can be a stale answer**, and the message says so when it came
  from cache. Whether a service offers ratings is learned once from its presentation map and kept;
  that cache is otherwise cleared only when the *player's* service-list version moves, which a
  service switching the feature on does not touch. `x2rock rate up --refresh` re-reads it - the
  same flag `search` and `browse` carry, for the same reason. Do not reach for it speculatively: a
  freshly-learned "no" is a real no, and the message only suggests the flag when the answer was
  cached.

## Chimes and announcements: `chime` and `notify`

Different from playing a stream: these **overlay** a short sound on a room and then hand it back.
`chime` plays the player's built-in notification sound; `notify "<url>"` plays a clip of your own -
an announcement, a doorbell, anything short. Use them for "chime the kitchen", "announce dinner",
"play this sound in the bedroom", not for putting music on.

- **They duck rather than replace.** The current playback dips under the clip and resumes, the queue
  is untouched, and the room returns to exactly what it was doing. So they are safe over music a
  stream command would interrupt.
- **They are per *speaker*, not per group.** `-r` names the room, and the clip lands on that room's
  own player - `-r Kitchen chime` chimes the kitchen even while it is grouped into a party. There is
  no `--all` and no repeated `-r`; one room at a time.
- **`--volume` is the clip's own level, 0-100.** It is independent of the room's volume and is *not*
  remembered after - the room's normal level is unchanged. Omit it to use the player's own setting.
  This is the safe way to make an announcement audible without leaving the room turned up.
- **`notify`'s URL is fetched by the player**, so it must be a public `http`/`https` address the
  speaker can reach, not a file on this machine - the same rule as `play-url`, and the same
  `bad_stream_url` refusal for anything else.
- **The confirmation is that it was *sent*, not heard.** Like a fire-and-forget: the player accepts
  the clip and returns before it sounds, and there is no state to poll (unlike a stream). A success
  line means it was accepted; it does not prove audio came out.

## Worked examples

**"Play something in the kitchen."** `play` only *resumes* what the room already holds; to start
something, use `favorite`, a search, or a station.

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
| `needs_link` | this machine holds no token for that music service | `x2rock link '<service>'` — **a browser login a person must finish**, so run it only with the user present and say what they will be asked to do; see "Linking a music service" |
| `no_search_categories` | the service publishes no search categories — it is browse-only, not broken | `x2rock browse -s "<service>"` |
| `bad_stream_url` | `play-url` was given something that is not an `http`/`https` URL | null (only an http(s) URL can be a stream) |
| `stream_did_not_play` | the player took the stream URL and the room is still idle 10s later — the stream is almost certainly dead, the room is fine | null (try a different stream) |
| `stream_unverified` | the stream was loaded but the room's state could not be established for 10s — unknown, and *not* a verdict on the stream | **null** (the message says whether the room answered; act on that, do *not* try another stream) |
| `playback_failed` | `play` or `play N` reached the room but it did not start — the player raised a playback error (an expired direct-stream URL, or a queued track its service will no longer serve; the message says which) or sat idle with nothing loaded | null (load a fresh source: `favorite`, `bookmark`, or a search; for a dead queued track, `play` another or `queue remove N`) |
| `no_player` | speakers were known here but none answered — a rescan already ran and found **nothing at all** | **null** (likely powered off; see below) |
| `unregistered_network` | this network has no known speakers — normal away from home | **null** (do *not* auto-scan; see below) |
| `too_many_rooms` | several `-r` on a command that takes one | null (re-run with one `-r`) |
| `multiple_households` | more than one Sonos household is reachable and nothing said which one — see "Addressing a household" | `x2rock households` (and see `data.households`) |
| `unknown_household` | the `-r` room or the `--household` selector matched no household — a stale id, a moved room, a typo | `x2rock households` (and see `data.households`) |
| `household_unreachable` | a rescan found **other** households but not this one — it is off, or has moved networks. Not `no_player`: the network is fine | `x2rock households` (and see `data.households` for what did answer) |
| `unknown` | no known remedy — e.g. `pause` on an already-idle room, `--all` on a command that does not take it | null (read `error`) |

**When `fix` is non-null, run it and retry** — except `needs_link`, whose fix opens a login page for
a person. **When `fix` is null, do not — read the `error` and change the request.** The two null network codes matter most: neither `unregistered_network` nor
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
  later rather than looping it) vs `household_unreachable` (the rescan found players, just none of
  *this* household's — so the network is fine and this system is the thing that is gone). Only the
  third has a fix worth running: `data.households` already lists what did answer.
- **A background daemon may be running** (Linux/MPRIS): it withdraws and reconnects on its own as the
  laptop moves networks, and logs the state — `journalctl --user -u x2rock.service` shows
  `x2rock: Kitchen -> org.mpris.MediaPlayer2.x2rock-…` when connected, or an hourly
  `unregistered network (gateway …)` when away. It is not required for anything you do from the CLI.
  If a user wants it and it is not installed, `x2rock service install --enable` sets it up as a
  user service pointing at this binary (`--headless` on a box with no desktop, `--household` on a
  network with two systems); it refuses to overwrite an edited unit without `--force`.
- **`discover` is offered, never reflexive** — it scans the local network, so run it only when the
  user confirms this is their own. Away from home, the answer is "your speakers aren't on this
  network", not a scan of it.

## When a field is a trap

- **`fixed:true`**: the room's volume is **not yours to change** - a Port or Amp feeding something
  with its own control. **x2rock refuses the command outright** (`"<Room> has fixed volume; adjust
  it on the amplifier"`), so this is a hard error rather than a silent no-op - do not retry it, and
  do not read the error as the room being unreachable. Different from `audible`, which stays `true`:
  a fixed room is loud, just not adjustable. Point at the downstream amp.
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
- **Some commands verify themselves, and the rest do not.** `play-url` and `stations --play` wait for
  the room to reach `PLAYING` and report one of four outcomes (see "Offer more than one, and keep
  going if one fails"). `play` (resume) and `play N` also wait, and fail with `playback_failed` when
  the room does not start. Their confirmation is worth believing and their failure is an error with
  a code. `favorite`, `playlist`, `bookmark`, `play-item` and `search`/`browse --play` do not wait
  — confirm those yourself.
- **`play` (resume) self-heals an expired direct stream.** A room can hold a source that has gone
  stale - most often a **direct stream** whose signed URL has expired. Content the queue refuses is
  played as a direct stream (Amazon Music on a Prime account is the known case, and stderr says so
  when one starts): it **cannot be paused and resumed**, and its URL stops working after a while.
  When `play` hits that, x2rock **re-resolves a fresh URL from the item it remembered and plays it**,
  printing that it refreshed the stream - one room, `--all`, or several `-r` alike. It falls back to
  `playback_failed` only when there is nothing to resume: x2rock did not start the stream, or the
  room has since moved on. The remedy then is not to retry `play` but to **load a fresh source**.
- **Warn on slow commands**: `discover` sweeps the subnet, `search`/`browse`/`rate`/`play-item` and
  `stations` reach the internet — seconds, not instant — a dead stream costs `play-url` ten, and
  `link` waits on a person for up to seven minutes. Everything local (transport, volume, status,
  queue) is fast.

## What is safe to repeat

Agents retry; know what is idempotent. **Safe (no surprising effect):** `vol` set, `repeat`/`shuffle`
set, `group`. **Safe but not silent:** re-running `favorite "X"` **restarts the track from zero**;
`play` on an already-playing room is a no-op. **Errors, so don't blind-retry:** `pause` on an `IDLE`
room (code `unknown`); `next`/`prev` advance each call.

## Getting *into* the household's services: favorites, keep, bookmark

**`x2rock search` (no term) lists what *this machine* can search — the anonymous radio-style
services plus whatever has been linked here with `x2rock link`. It is not the household's list of
services**, and a service the household uses in the Sonos app is not searchable from here until it
is linked (search fails with `needs_link`). Many can be linked; YouTube Music cannot (see "Linking a
music service"). Whether or not a service is linked, three routes reach what the household plays:

- **`favorites`** — what the household saved in the Sonos app; `favorite "<name-or-id>"` plays one.
- **`keep`** — snapshots the **currently-playing track** (or `--container` for its album/playlist/
  station) into a *local* list, so it can be replayed later without a favorite. It is x2rock's own
  record, not a Sonos favorite.
- **`bookmark "<name>"`** — plays back something `keep` (or the daemon, automatically) recorded.
  `bookmarks --json` is a bare array: `[{id, name, type, service, description, art_url}]`. By default
  it lists only what was kept on purpose; `bookmarks --all` (here meaning "include daemon-noticed
  history", not whole-house) adds what the daemon noticed playing — the answer to "that thing from
  yesterday". `bookmarks pin "<name>"` promotes an unpinned history track to permanently kept.
  `bookmarks rename "<name>" "<new_name>"` renames a bookmark, and `bookmarks prune` clears unpinned
  daemon history while preserving all kept bookmarks. `bookmarks remove "<name>"` removes a single
  entry. A kept on-demand track replays through the household's own account for its service,
  so it stops working if the household removes that account and works again if it is re-added.

## Linking a music service: `link`, `accounts`, `unlink`

**A link usually buys search and browse, and never by itself buys playback of on-demand tracks.**
A service may also gate its catalogue behind its own paid tier - Saavn links on a free account and
then refuses every search and browse with `User not Pro` (with Pro, both work), so a linked account
is not proof of a usable one. **And a whole content type can be invisible to `search`**: Saavn
publishes no podcast category, yet its shows browse fine under `TOPSHOWS:…` - when a service
"doesn't have" something, try `browse` before believing it. Keep
those two apart when telling a user what linking will do.

- **Linking needs a person.** `x2rock link '<Service>'` opens the service's own login page in the
  browser and polls until the login is finished, for up to seven minutes. `--no-open` prints the URL
  instead — use it when the user is not at this machine's screen, and hand them the URL. Tell the
  user what they are about to be asked to do, and do not report success before the command exits
  with `Linked <Service>.` The token is stored on this machine only; `unlink` forgets it locally
  (revoking it is done on the service's own site).
- **Which services link.** `x2rock link` with no argument lists the device-link services (plus
  Plex). App-link services are not in that list but can still be named: `link` asks each for a
  browser page, and the answer is the service's own policy. As last swept (2026-09-10): **TuneIn
  (New), Radio Paradise, Amazon Music, Pandora, Pandora CloudCover and Spotify** gave a page;
  **YouTube Music** (`refused getAppLink: HTTP 403`), **Apple Music** and **SoundCloud** refuse.
  A refusal is immediate, changes nothing, and exits 1 — report it plainly. YouTube Music's refusal
  is not something x2rock can get past (its endpoint wants a key Sonos seals in its own apps). For a
  service not named here, just try it rather than predicting. **A refusal is not the end of the
  road**: if the household already holds that account, `--from-household` below takes its token
  without any login page at all.
- **Plex** links through Plex's own PIN flow. `link plex --from-player` needs no browser: it reads
  the token of the household's own Plex integration while Plex is playing or paused in some room,
  and can browse a server's root where a fresh token sometimes cannot.
- **`link --from-household` needs no browser, and reaches what `link` cannot.** Every zone player
  keeps the token the Sonos app minted for each account the household holds, and publishes the set
  encrypted in its initial topology event; this reads it and stores it. Name a service to take just
  that one, or omit it to import every service the household holds a usable token for. It is the
  only route to a service whose own link flow refuses x2rock — **Qobuz, Apple Music, Amazon** — and
  it needs no `match` step, because playback already rides the registration the Sonos app made.
  Three things to tell a user before running it: it is **read-only on the household** (nothing is
  added or changed there, and the app keeps working); it **needs the player to open a connection
  back to this machine**, TCP **3401** by default, so a host firewall must allow that inbound port
  (the failure names it, `--callback-port N` moves it, and `0` takes an ephemeral one where there
  is nothing to open); and it does **not** get past YouTube Music, whose block is the caller key
  rather than the account — the token imports and search still 403s.
- **A household can hold two accounts for one service, and both are kept.** The Sonos app numbers
  the second in its nickname — `iHeartRadio 885ebbcc` beside plain `iHeartRadio`, observed in a real
  household 2026-09-22 — and the import keeps each one. **One of them is what search and playback
  use**, exactly as the Sonos app prioritises one, so results come back as one set rather than two
  interleaved; `x2rock accounts` marks it with a `*` and names the others. Change it with
  `x2rock accounts --prefer <service> "<nickname>"`, which takes a nickname, a unique nickname
  prefix, or the account key, and refuses an ambiguous one by naming the candidates. **Two accounts
  can carry the same nickname** - a household that rotated a service's token leaves two records with
  the name the app gave the service - and then the nickname names neither: the listing shows each
  account's key instead (`YouTube Music (sn15)`), and that key is what to pass. The preference
  survives a re-import. This is the case that matters for a service whose
  accounts hold different catalogues — two Audible libraries, say — where which account is in use
  decides what a search can even find.
- **`link --from-household --dry-run` shows what a household holds and keeps none of it.** Every
  account record and every attribute, with each token replaced by its byte length. Use it to see
  what an import would take before taking it, and to answer questions about a household's accounts
  that `x2rock accounts` cannot - that one lists what *this machine* holds, this lists what the
  *household* holds. Same firewall requirement as the import, since it is the same event capture.
- **`unlink <service>` forgets *every* account that service has**, in every household unless
  `--household` narrows it; `unlink <service> --account "<nickname>"` forgets just one and leaves
  its siblings. The plain form says how many it dropped, so a service that held two says so.
- **The per-service scoreboard below is a summary.** What has been tested, with dates and the
  household each result came from, is the table at the top of `docs/architecture.md`; when the two
  disagree, that one is right. The *live* answer for a household is `x2rock search` (bare) and
  `x2rock link` (bare).
- **How playback works once linked.** A **stream** (`type: "stream"`, a station) is streamed with
  this machine's token and usually plays with nothing else. **Anything else** — a track, an episode
  — is added to the queue, and the *player* resolves it with **the household's own account for that
  service**, added in the Sonos app. So an on-demand track plays only if the household has that
  account. Without one, the queue refuses and x2rock falls back to streaming the item (stderr says
  "would not go in the queue; streaming it"), which works only when the service hands back a
  playable URL: **Amazon Music does** (a presigned HLS playlist), and **Deezer and TIDAL do**
  (a signed, seekable FLAC file) — the last two only since 2026-09-19, when the reason they used to
  stall turned out to be x2rock's own: every fallback URL was handed to the player as a *station*,
  and a file cannot be played that way. **Spotify does not** (an unsupported-scheme error until the
  household adds Spotify in the Sonos app, after which it plays normally), and Radio Paradise's
  programs do not (it implements no `getMediaURI` at all). Two things to know about a fallback that
  is a file: it **replaces what the room was playing**, queue included, where a station-shaped
  stream plays alongside the queue; and `now --json` reports its `duration_ms` (read over UPnP, since
  the Control API carries none for a transport-set URI) while `status --json` leaves it null - the
  sweep does not make that extra call. **A fallback that "started" is still not a fallback that played**:
  confirm with `now --json` twice and require `position_ms` to have moved. A service container
  (album, playlist) cannot be played whole either way; see "A container cannot be played whole".
- **Read `link`'s last line for which case you are in.** `The household knows this account as
  sn_22` means the household already holds this very account, so on-demand playback works. `did not
  match` or `sent no userIdHashCode` means only that this account was not matched; the household may
  still hold its own account for the service (on-demand tracks then play from *that* account) or
  may hold none. The honest move is to try a track and read the result, not to predict.
- **Tokens are held per household, not just per machine.** A laptop that moves between two Sonos
  systems — home and an office — keeps a separate account per service for each, and every search,
  browse and play resolves the household it is standing in before looking a token up. So a service
  linked at the office reads as **unlinked at home** until it is linked or imported there too, and
  that is correct rather than a fault: say so and offer `link --from-household` instead of
  re-running a browser login. An auto-refresh on one network never touches the other's token.
  `x2rock accounts` lists every household it holds, under a header only when there is more than one.
- **`accounts --json`**: `{service, service_id, account_key, serial, preferred, account_id,
  nickname, linked, household}` per token this machine holds — **one row per account**, so a service
  with two accounts is two rows with the same `service`. `preferred` marks the one in use and
  `account_key` is what `--prefer` and `--account` accept. `account_id` is the household serial when the account was matched, otherwise
  `null` (prose: `no registration from this machine`), and `null` is not an error. `linked` is a
  Unix timestamp. `household` is the household the token was minted against, and it **is** the key
  it is filed under — a token is used on that household's network and not on another's. None of
  this is the household's own account list, which no command can read (see the `accounts --content`
  note above).
- **`unlink` is scoped by what you give it, and never revokes anything.** A service alone forgets it
  in *every* household that holds it ("stop using this service" rather than "on this network");
  `--household` narrows that to one; `--all` wipes every stored token; `--all --household <id>`
  wipes one household. Because `unlink` reaches no player, its `--household` is matched against the
  **stored** household ids that `accounts` shows — by exact id or a unique fragment — not against
  room names, so a room name there is an error rather than a selector. The tokens stay valid at
  their services, and `link --from-household` re-imports what a wipe cleared.
- **An expired token usually heals itself.** When a service answers with a replacement token,
  x2rock retries once and stores the new one, silently. If a linked service starts failing with a
  plain refusal instead, `x2rock link '<Service>'` again.
- A linked service is not always complete: Pandora's free tier searches but refuses to open its
  stations (`Unsupported action for account type`), and a service with no search categories is
  browse-only (`no_search_categories`).

## `raw`, and its boundary

`x2rock raw` speaks a player directly and **can mutate state** — high blast radius. Use it **only**
when the user explicitly asks for raw access, or when no first-class command covers the intent.
**Never route around an error with it** — a `needs_link` or an unsupported request should be
*reported*, not bypassed.

**Two transports, as two subcommands**, because they share no grammar:

- `x2rock raw api <namespace> <command> [JSON]` — the Control API over the player's WebSocket.
  `--scope household|group|player|none` (default `household`), plus `--watch <seconds>` to read what
  a `subscribe` delivers afterwards and `--session <id>` for `playbackSession:1`.
- `x2rock raw upnp <Service> <Action> [Name=Value ...]` — UPnP/SOAP on port 1400, the older and much
  wider surface: line-in, the physical speaker, soundbar IR, the local music library. Arguments are
  flat `Name=Value` pairs, not JSON, and most actions need `InstanceID=0`. `--scope player|group`
  only (default `player`), because UPnP addresses one speaker and never a group. An unknown service
  name lists the sixteen there are. Output is an **array of `{name, value}` pairs** in the player's
  own order, not an object — a probe is reading a shape it does not know yet.

**A refusal is a result on both**: a player-side error prints and still exits 0, so a loop over
candidate actions is not stopped by the first unsupported one. An **unreachable** speaker is a real
failure and exits non-zero — the two are told apart, so `|| handle_failure` means what it says.

`x2rock raw api --help` and `x2rock raw upnp --help` document the rest.

## More detail

Every command has `x2rock <command> --help`.
