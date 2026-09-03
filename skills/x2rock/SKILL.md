---
name: x2rock
description: Control Sonos speakers from the command line with the `x2rock` CLI — play/pause, per-room volume, grouping, favorites and music-service search, and a one-call JSON snapshot of the whole household. Use when the user wants to control Sonos, play or pause music in a room, change speaker volume, group or ungroup rooms, or see what is playing where.
---

# Driving Sonos with the `x2rock` CLI

`x2rock` controls Sonos speakers on the local network — no account, no cloud. Every command is a
subprocess; there is nothing to keep running for control. **Prefer `--json` on every command that
has it, and read the fields rather than parsing the human prose.**

## Start here

- **`x2rock status --json`** — the whole household in one call: every room with its state,
  now-playing (title, artist, service, position/duration), volume, grouping, and TV capability
  (`has_tv`). This is almost always the first thing to run.
- `x2rock rooms --json` — just the room list and playback state; cheaper than `status`.
- `x2rock status --json --full` — the same rooms wrapped in a household envelope: `{household,
  network, total, reachable, warnings, rooms}`. Use it to confirm *which* household/network you are
  on and whether every room answered (`warnings` names any that did not). Plain `--json` stays a
  bare array.

### Reading a room's fields

- **`state`** is one of `PLAYING`, `PAUSED`, `IDLE`, `BUFFERING` — the Sonos Control-API set, *not*
  UPnP, so `STOPPED`/`TRANSITIONING` never appear. `IDLE` means nothing is loaded or a stream has
  stopped.
- **`audible`** (in `status` and `vol --json`) is `false` when the room is muted *or* at volume 0 —
  the two silent states rolled into one. Check it before "play something here", or the room stays
  quiet and the play still succeeds.
- **`on_tv`** is `true` when a soundbar is on its TV input; prefer it over matching the `"TV Audio"`
  title. `has_tv` (in `status`) is the *capability* — whether the room *can* take a TV input.
- **`service`** is the music service's name when the player reports one, else `null`; **`service_id`**
  carries the raw id even when the name is missing, so the source is never fully lost.
- If either fails with `"code":"unregistered_network"`, run **`x2rock discover`** once — the machine
  is on a network x2rock has not seen. After that it reconnects on its own; discovery is never
  automatic (it will not scan unfamiliar WiFi unasked).

## Errors are structured — act on them, don't parse prose

With `--json`, a failed command prints a JSON object to **stderr** and exits non-zero:

```json
{"error":"…human message…","code":"unregistered_network","fix":"x2rock discover"}
```

`code` is a stable identifier for the *kind* of failure; `fix`, when present, is the exact command
that resolves it — run it, then retry. Codes you will meet:

| `code` | meaning | `fix` |
|---|---|---|
| `unregistered_network` | on a network with no known speakers | `x2rock discover` |
| `unknown_room` | the `-r` name is not a room here | `x2rock rooms` — but see below |
| `needs_link` | the music service needs an account | `x2rock link <service>` |
| `no_player` | no reachable speaker to act on | `x2rock discover` |
| `error` | no known remedy (`fix` is null) | — |

Some codes carry extra detail so you need not re-fetch. **`unknown_room`** includes `did_you_mean`
(typo-tolerant suggestions) and `rooms` (the full list), so a mistyped `-r` is fixable from that one
reply — no `x2rock rooms` round trip:

```json
{"code":"unknown_room","error":"no room named \"bedoom\"…","fix":"x2rock rooms",
 "did_you_mean":["Bedroom"],"rooms":["Bedroom","Living Room","Dining Room","Guest TV","Kitchen"]}
```

## Addressing a room

- `-r "<Room>"` picks the room: `x2rock -r "Kitchen" pause`. Names come from `x2rock rooms`.
- A household with a single group needs no `-r`.
- Volume is **relative** with `+`/`-`: `x2rock -r Kitchen vol +5`. A bare number sets it: `vol 30`.
- **`-r` is repeatable for the per-room commands** — `vol`, `repeat`, `shuffle`, and transport
  (`play`/`pause`/`toggle`/`next`/`prev`). `x2rock -r Kitchen -r Bedroom vol 10` applies to each
  (topology resolved once), printing one result line per room. Other commands take a single `-r`;
  giving several is an error, not a silent act on the first. A fan-out stops at the first room that
  fails, and the error names it — the rooms before it already applied.

## Commands, by intent

| Intent | Command |
|---|---|
| See everything | `x2rock status --json` |
| What's playing (one room) | `x2rock now --json` |
| Transport | `x2rock play` / `pause` / `toggle` / `next` / `prev` |
| Play queue track N | `x2rock play N` |
| Volume | `x2rock vol --json` (read) / `vol 30` / `vol +5` / `vol mute` / `vol unmute` |
| Repeat / shuffle | `x2rock repeat [all\|one\|off] --json` / `x2rock shuffle [on\|off] --json` |
| The queue | `x2rock queue --json` / `queue remove N` / `queue clear --yes` |
| Favorites | `x2rock favorites --json` / `x2rock favorite "<name>"` |
| Search a service | `x2rock search --json` (lists services) / `x2rock search -s <svc> <term> --json` |
| Browse a service | `x2rock browse -s <svc> [container] --json` |
| Play a search/browse hit | `x2rock search -s <svc> <term> --play N -r "<Room>"` |
| Group / ungroup / party | `x2rock -r "<Room>" group <Other> …` / `x2rock ungroup <Room>` / `x2rock party` |
| Soundbar TV input | `x2rock -r "<Room>" tv` (only where `has_tv` is true) |
| Remember & replay | `x2rock keep` / `x2rock bookmarks --json` / `x2rock bookmark "<name>"` |
| Link an account | `x2rock link [service]` / `x2rock accounts --json` |

## Rules that avoid mistakes

- **Use `--json` and read the fields.** The prose wording can change; the JSON contract does not.
- **`vol`, `repeat` and `shuffle` are dual-mode** — with no argument they *report* the current
  state, with an argument they *change* it. `--json` gives the resulting `{room, …}` either way, so
  reach for it both to read a value (`x2rock -r Kitchen vol --json`) and to confirm a change. Their
  fields match a room's entry in `status --json`, which is where to read all rooms at once.
- **`favorite` is the only way to start a stopped, empty room** — `play` only *resumes* something.
- **`discover` runs once per new network**, and only when an error asks for it.
- Grouping: `group` adds rooms to `-r`'s group, and a room that joins plays that group's music;
  `ungroup` returns a room to its own queue (stopped at the first track).
- Talking to a music service (`search`, `browse`, `link`) reaches the internet; everything else is
  local. A slow service never blocks transport or volume.

## More detail

Every command has `x2rock <command> --help`. For raw Sonos Control-API access there is
`x2rock raw --help`, which documents namespaces, scopes, and worked examples — it is written for an
agent to read.
