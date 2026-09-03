---
name: x2rock
description: Control Sonos speakers from the command line with the `x2rock` CLI — play, pause, skip, per-room and whole-house volume, mute, shuffle and repeat, the queue, favorites, music-service search and browse, grouping and party mode, soundbar TV input, and a one-call JSON snapshot of the whole household. Use whenever the user wants to control Sonos or speakers — "play/put on <something> in <room>", "pause", "skip", "turn it up/down", "quieter/louder everywhere", "mute the kitchen", "shuffle", "what's playing / what's on", "group these rooms", "play everywhere / party", or switch a soundbar to TV.
---

# Driving Sonos with the `x2rock` CLI

`x2rock` controls Sonos speakers on the local network — no account, no cloud. Every command is a
one-shot subprocess; nothing stays running for control. Two contracts hold everything together:

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
    "service": "YouTube Music", "service_id": "284", "art_url": "http://…",
    "volume": 2, "muted": false, "audible": true,
    "repeat": "off", "shuffle": false,
    "on_tv": false, "has_tv": false, "input_format": null, "surround": null,
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
  `duration_ms`), not nested. `position_ms`/`duration_ms` are **milliseconds** (`duration_ms` is
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

`bookmarks --json` and `accounts --json` are also bare arrays. `queue --json` is an **object**:
`{"current": <index>, "items": [{"index","title","artist","album","duration_ms","art_url","current"}]}`
— indices are 1-based, and `play N` plays item `N`.

## Grouping — how `-r` resolves once rooms are joined

This is the highest-stakes thing to get right. When rooms are grouped:

- **`-r <any member>` acts on the whole GROUP.** `-r Kitchen pause` when Kitchen is grouped pauses
  the group; `-r Kitchen vol 20` sets the *group* volume; `-r Kitchen next` skips for the group.
  Addressing by the coordinator name does the same thing.
- **`-r <member> vol --player` scopes to that one speaker** — its own volume, not the group's. Use
  `--player` only when the user means one speaker inside a group (balancing it against the others);
  otherwise a plain `-r <room>` on a grouped room is a group action.
- To act on a group, pass any member's or the coordinator's **real** room name — never the composite
  `"Dining Room + 1"`.

`group`/`ungroup`/`party` change the topology (see the command table). After a group change, the
topology takes a second or two to settle — re-read `status` rather than assuming.

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
| The queue | `x2rock queue --json` / `queue remove N` / `queue clear --yes` |
| Favorites | `x2rock favorites --json` (household-wide) / `x2rock -r <Room> favorite "<name-or-id>"` |
| Search a service | `x2rock search --json` (lists services) / `x2rock search -s <svc> <term> --json` |
| Browse a service | `x2rock browse -s <svc> [container] --json` |
| Play a search/browse hit | `x2rock search -s <svc> <term> --play N -r "<Room>"` |
| Group rooms | `x2rock -r "<Coordinator>" group <Other> …` |
| Ungroup / party | `x2rock ungroup <Room>` (positional, no `-r`) / `x2rock -r "<Room>" party` / `x2rock party off` |
| Soundbar TV input | `x2rock -r "<Room>" tv` (only where `has_tv` is true) |
| Remember & replay | `x2rock keep` / `x2rock bookmarks --json` / `x2rock bookmark "<name>"` |
| Link an account | `x2rock link [service]` / `x2rock accounts --json` |

Two shapes worth noting because they are inconsistent: **`favorite` is name/id-addressed**
(`favorite "Jazz"` or `favorite 37`), while a **search/browse hit is index-addressed** (`--play N`).
And **`group`/`ungroup` are asymmetric**: `group` takes `-r <coordinator>` plus room arguments;
`ungroup` takes the room *positionally* with no `-r`. **`favorites` (listing) is household-wide** —
`-r` is only meaningful on `favorite` (playing), to say which room.

## Worked examples

**"Play something in the kitchen."** `play` only *resumes*; a stopped room needs `favorite`.

```sh
x2rock status --json                      # Kitchen is IDLE, volume 2, audible:true
x2rock favorites --json                   # household favorites; pick a playable one
x2rock -r Kitchen favorite "Lo-Fi for Vampires Only"
x2rock -r Kitchen now --json              # confirm: state PLAYING, position advancing
```

**"Turn it down everywhere."** One call, no room names to derive:

```sh
x2rock --all vol -10
```

**"What's on in the house?"** — `x2rock status --json`, then read state/title/service/volume/audible/
on_tv per entry (a grouped entry covers all its members).

## Errors — act on the code, don't parse the sentence

A failed `--json` command prints to **stderr** and exits non-zero:

```json
{"error":"…human message…","code":"unregistered_network","fix":"x2rock discover"}
```

| `code` | meaning | `fix` |
|---|---|---|
| `unregistered_network` | on a network with no known speakers | `x2rock discover` |
| `unknown_room` | the `-r` name is not a room (or is a group's composite label) | `x2rock rooms` (and see `did_you_mean`) |
| `needs_link` | the music service needs an account | `x2rock link <service>` |
| `no_player` | no reachable speaker to act on | `x2rock discover` |
| `too_many_rooms` | several `-r` on a command that takes one | — (`fix` null; re-run with one `-r`) |
| `unknown` | no known remedy — including `pause` on an already-idle room | — (`fix` null; read `error`) |

**When `fix` is non-null, run it and retry. When `fix` is null (`too_many_rooms`, `unknown`), do
not retry — read the `error` and change the request.** `unknown_room` carries extra detail so you
need not re-fetch:

```json
{"code":"unknown_room","error":"no room named \"bedoom\"…","fix":"x2rock rooms",
 "did_you_mean":["Bedroom"],"rooms":["Bedroom","Living Room","Dining Room","Guest TV","Kitchen"]}
```

## When a field is a trap — what to do about it

- **`audible:false`** (muted, or volume 0): the room makes no sound even though a play succeeds. Say
  so and ask whether to unmute / raise the volume (and to what) — do **not** silently unmute; in a
  shared house that surprises people. If the intent is already loud ("blast it to wake them"), set
  the volume as asked. Note the flip side: `audible:true` only means *not muted and not zero* — a
  room at `volume:2` is audible but barely, so do not read `true` as "loud enough to hear".
- **`on_tv:true` with `input_format:"No Signal"`**: TV input selected but nothing coming through —
  not "playing". `favorite`/`play` switches the room off TV; offer that. (`surround` is just whether
  the TV format is surround — informational.)
- **`favorites --json` `"playable":false`**: an empty shell (dead service). Do not offer these as
  things to play; if the user names one, note it looks defunct.
- **Favorite drift**: a live service can silently reuse an id (iHeartRadio swaps in seasonal stations
  at the holidays), which nothing detects. After starting a favorite you can `now --json` and compare
  the playing title/artist against the favorite's name; flag a surprising mismatch, don't warn every
  time.

## Ask before you act — it is a shared house

Several commands reach other people. **Confirm first, unless the user's intent is already explicit:**

- **`party`** — groups every room into one; someone may be asleep in a room it captures.
- **A loud volume** — a big jump, or a high absolute like `vol 90` (or `--all vol 90`).
- **`ungroup`** — the room drops back to its *first* track (it loses its place).
- **`queue clear`** — irreversible; Sonos keeps no undo (hence the required `--yes`).

The carve-out is real: "wake her up with music at full volume" *is* the instruction — do it.

## Verifying, and latency

- **Confirm playback with `now --json`.** Expect `BUFFERING` briefly before `PLAYING`; an immediate
  read after starting may still show `IDLE`/`BUFFERING`, so wait a second and re-check rather than
  calling it a failure. `state:"PLAYING"` with `position_ms` advancing between two reads is real
  sound (subject to `audible`).
- **Warn on slow commands.** `discover` sweeps the subnet, and `search`/`browse`/`link` reach the
  internet — any can take many seconds. Say you are working rather than going silent. Everything
  local (transport, volume, status, queue) is fast.

## Addressing a room

- `-r "<Room>"` names the room, **case-insensitively** (`-r kitchen` matches "Kitchen"). Names come
  from `status`/`rooms`; on a grouped entry use a `members`/`coordinator` name, not the composite.
- **Always pass `-r` when the user named a room.** Omit it only when they mean the whole house (and
  even then, omission is unambiguous only in a single-group household — otherwise use `--all`).
- x2rock does no natural-language mapping: "the kitchen", "downstairs", "the office speaker" are
  yours to resolve to a room name first.
- A wrong room means loud music in the wrong place, so treat it as high-stakes: a single
  high-confidence `did_you_mean` is usually safe to accept, but confirm when unsure.
- **`-r` is repeatable** for the per-room commands — `vol`, `repeat`, `shuffle`, and transport:
  `-r Kitchen -r Bedroom vol 10` applies to each. **`--all`** does every room at once. Any other
  command with several `-r` is `too_many_rooms`. A fan-out stops at the first room that fails and
  names it; the rooms before it already applied.
- Volume is **relative** with `+`/`-` (`vol +5`, `vol -10`), or absolute `vol 30`. A relative change
  **clamps at 0 and 100** — it never errors for going out of range.

## What is safe to repeat

Agents retry; know what is idempotent. **Safe (no surprising effect):** `vol` set, `repeat`/`shuffle`
set, `group`. **Safe but not silent:** re-running `favorite "X"` **restarts the track from zero**;
`play` on an already-playing room is a no-op. **Errors, so don't blind-retry:** `pause` on an `IDLE`
room (code `unknown`); `next`/`prev` advance each call.

## `search` lists the searchable set, not the household's services

**`x2rock search` (no term) lists the services searchable *without the household's account* — the
anonymous radio-style ones plus whatever this machine has linked.** It is **not** the household's
real services. YouTube Music, Amazon Music and the like are **not searchable here** — offering to
"search YouTube Music" fails with `needs_link` or finds nothing. The way *into* those is
**`favorites`** and **`bookmark`/`keep`**, not `search`.

## `raw`, and its boundary

`x2rock raw` speaks the Sonos Control API directly and **can mutate state** — high blast radius. Use
it **only** when the user explicitly asks for raw access, or when no first-class command covers the
intent. **Never route around an error with it** — a `needs_link` or an unsupported request should be
*reported*, not bypassed. `x2rock raw --help` documents namespaces and scopes.

## More detail

Every command has `x2rock <command> --help`.
