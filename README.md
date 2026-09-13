# x2rock

Local-first Sonos control for Linux: a daemon that publishes every room as a standard MPRIS2
media player, and a CLI that reaches everything the speakers will answer to on the LAN — playback,
volume, grouping, the queue, favorites, music services, alarms, tone, and the physical speaker.

**No Sonos login, ever.** x2rock talks to the speakers directly, the way the Sonos app itself does
on your network. Control never leaves the LAN.

**Every Sonos room is a Linux media player.** Media keys, the lock screen, GNOME and KDE media
applets, `playerctl`, Waybar — anything that already speaks MPRIS plays, pauses, skips and shows
what is playing on Sonos, with cover art and no Sonos-specific setup.

**Built for agents as much as for people.** Every command takes `--json` and answers in a stable
shape; every failure is `{error, code, fix}` with a machine-readable code and, where one exists,
the exact command that resolves it. `x2rock skill` installs a skill that teaches an AI assistant
the whole surface.

> **Status.** The daemon and the CLI are complete for daily use and are the product. Everything a
> Sonos household will do over its local network is here or deliberately declined — the only
> features left out are the ones **only Sonos's own apps can perform** (installing firmware,
> onboarding a speaker, editing bonds, running TruePlay); see [What stays with the Sonos
> app](#what-stays-with-the-sonos-app). A terminal UI and an Omarchy bar widget ride on the daemon
> as two of its consumers.
>
> Every feature was exercised against real speakers rather than the protocol documentation, which
> repeatedly turned out to be the only way to learn what is true. The facts that shaped the design
> are in [docs/architecture.md](docs/architecture.md).

**Contents** — [Quick start](#quick-start) · [Why local-first](#why-local-first) ·
[The CLI](#the-cli) · [For scripts and agents](#for-scripts-and-agents) ·
[The daemon and MPRIS](#the-daemon-and-mpris) · [Front ends](#front-ends) ·
[Music services](#music-services) · [The queue](#the-queue) · [Probing and debugging](#probing-and-debugging) ·
[Requirements](#requirements) · [Tested devices](#tested-devices) ·
[What stays with the Sonos app](#what-stays-with-the-sonos-app) · [Non-goals](#non-goals) ·
[Licence](#licence)

## Quick start

Three commands, the same on every Linux:

```sh
cargo install --git https://github.com/rahga/x2rock   # the binary, into ~/.cargo/bin
x2rock discover                     # once per network; every other command reconnects to what this remembers
x2rock service install --enable     # the daemon as a user service - every room is now an MPRIS player
```

`service install` writes the systemd unit pointing at **whichever binary is running it** — from
`cargo install`, from a clone, from a package — so there is no path to get wrong and nothing to copy
by hand. It refuses to overwrite a unit you have edited unless told to with `--force`; `--print`
shows what it would write. Re-run it if you move or reinstall the binary. (`systemd/x2rock.service`
is the same unit as a file, for anyone who prefers to copy it.)

Discover first: the daemon connects only to players it has been told about and will not scan an
unfamiliar network on its own. The other order is not fatal — it re-reads the remembered players
between reconnect attempts, so a later `discover` is picked up within a minute. If `x2rock rooms`
lists your speakers, the CLI is done.

Cloning gets you the desktop entry, which lets MPRIS clients label each room player with a name
and icon, and the bar widget:

```sh
git clone https://github.com/rahga/x2rock && cd x2rock
cargo build --release && install -Dm755 target/release/x2rock ~/.local/bin/x2rock
install -Dm644 desktop/x2rock.desktop ~/.local/share/applications/x2rock.desktop
install -Dm644 desktop/x2rock.svg ~/.local/share/icons/hicolor/scalable/apps/x2rock.svg
```

Needs Rust 1.89 or newer and a C compiler; see [Requirements](#requirements) for what bites on
Ubuntu, for a machine with no desktop, and for the one Sonos setting a few commands need.

## Why local-first

Sonos speakers expose the same JSON Control API on the LAN that Sonos's cloud exposes remotely,
over a WebSocket on port 1443, with no OAuth and no internet round-trip. It is the transport the
official Sonos mobile app has used since 2024. x2rock is built on it directly:

- **No Sonos account, ever.** That is the line, and it is narrower than "no cloud": searching a
  music service talks to that service, and `x2rock stations` talks to a radio directory, because
  neither wants a Sonos login. What x2rock will not do is depend on signing in to Sonos — a speaker
  on the LAN answers to whoever is on the LAN.
- **Push events, not polling.** The LAN API supports real subscriptions; the daemon never polls.
- **Outbound connections only**, which matters on a Linux box with a default-deny firewall and on
  a locked-down office network. That is also why discovery does not use SSDP or mDNS, whose
  multicast replies a default-deny policy silently drops.

Where the Control API stops, x2rock speaks UPnP/SOAP to the same speaker on port 1400: the queue,
alarms, the sleep timer, tone controls, the status light and button lock, room renaming,
soundbar remote settings, and the per-speaker hardware inventory all live there. The Control API
has no view of any of them — cloud or local.

## The CLI

Every command has `x2rock <command> --help`. This is the surface by area; the examples below each
table are the ones worth knowing by heart.

### Rooms and the household

| | |
|---|---|
| `x2rock rooms` | rooms and their playback state |
| `x2rock now [--json]` | what one room is playing |
| `x2rock status [--json] [--full]` | **every room in one call**: now-playing, volume, grouping, TV — the snapshot to start from |
| `x2rock system [--json] [--redact]` | every *speaker*: model, firmware, hardware, bonding, and how it is connected |
| `x2rock update [--json]` | what firmware each speaker has and whether one is offered — read-only |
| `x2rock households [--json]` | every Sonos household on this network; only matters with more than one |
| `x2rock discover` | scan the local subnet once and remember what it finds |
| `x2rock -r <Room> rename "<New Name>"` | rename a room, for every app in the house |

`-r`/`--room` names the room; with one group in the household nothing needs naming. Set
`X2ROCK_ROOM` to make it stick, and `x2rock rooms` offers a ready-to-paste `export` when there is
more than one room and no default. **A grouped room's composite label ("Dining Room + 1") is not a
room name** — address a group by any member.

`system` is the readout the Sonos apps call *About My System*: eleven players here where `rooms`
shows five, because a room backed by a soundbar, a Sub and two surrounds is one room and four
speakers. Its `connection` column — `wired`, `sonosnet`, `satellite` — says how each speaker
reaches the household, which is the first thing to know when several rooms go quiet at once: as
soon as one speaker is wired, the others typically leave your WiFi for SonosNet, Sonos's own mesh
bridged through the wired one. `--redact` masks serials, addresses and uuids for pasting anywhere.

### Playback

| | |
|---|---|
| `x2rock play` / `pause` / `toggle` / `next` / `prev` | transport; `play` **resumes** and confirms the room started |
| `x2rock play N` | play track N of the queue |
| `x2rock favorite "<name or id>"` | play a favorite — **the one way to start a room that has nothing queued** |
| `x2rock playlist "<name or id>"` | play a saved Sonos playlist, replacing the queue |
| `x2rock repeat [all\|one\|off]` · `shuffle [on\|off]` · `crossfade [on\|off]` | play modes; bare, they read |
| `x2rock -r <Room> rate up\|down [--refresh]` | thumbs up/down where the service offers it (Pandora-style radio, iHeartRadio Custom Stations) |
| `x2rock sleep [30m\|off]` | the sleep timer; bare, it reads |
| `x2rock -r <Room> tv` | switch a soundbar to its TV input |
| `x2rock -r <Room> chime` · `notify "<url>" [--volume N]` | a chime or your own clip, ducked over whatever is playing |

`play` only resumes: a room with an empty queue has nothing for it to do, and `favorite`, `playlist`,
`bookmark`, a search hit or a stream is what starts one. `favorite` matches an id exactly or a name
case-insensitively; several matches are reported rather than guessed between, unless one is the
whole name. Loading a favorite replaces the queue, as the Sonos app does.

`now` on a soundbar shows what the TV is actually sending — `TV Audio [Dolby Digital 5.1]`, or
`[Dolby Digital 2.0]` when the source has quietly fallen back to stereo, which is invisible
anywhere else.

### Volume

| | |
|---|---|
| `x2rock vol` | read it (`--json` for `{room, volume, muted, fixed, balanced}`) |
| `x2rock vol 30` · `vol +5` · `vol -10` | set, or nudge; a nudge clamps at 0/100 |
| `x2rock vol mute` · `unmute` | group mute |
| `x2rock -r <Room> vol 20 --player` | one speaker's own level inside its group — the balance |
| `x2rock -r <Room> vol 30 --each` | every speaker in the group to 30, flat |
| `x2rock -r <Room> vol normalize` | every speaker to the group's level — the app's *Normalize Group Volume* |
| `x2rock -r <Room> vol 30 --ramp` | slide there over a few seconds instead of jumping |
| `x2rock --all vol -10` | every room at once |

Inside a group the plain `vol` is the group's mix and preserves the members' balance, the way the
Sonos app's slider does. `--player` reads or sets one speaker; `--each` erases the balance;
`normalize` evens it out at the level the group already has (`vol --json` says whether the members
are `balanced`). `--ramp` is per speaker — there is no group ramp — and composes with several `-r`
and with `--each`, but not with `--all`.

### Grouping

| | |
|---|---|
| `x2rock -r "Living Room" group Kitchen Bedroom` | rooms join Living Room's group and play what it plays |
| `x2rock ungroup Kitchen` | Kitchen leaves; positional, no `-r` — a room is only ever in one group |
| `x2rock -r Kitchen party` · `x2rock party off` | party mode hosted by that room; everyone joins |

Both print the group as it ended up. A room that leaves a group keeps its own queue but comes back
stopped at its first track rather than where it was. `party` reaches every room in the house.

### The queue, favorites and playlists

| | |
|---|---|
| `x2rock queue [--json]` | the queue, current track marked, and whether it is in use |
| `x2rock queue remove 4` · `remove 4-8` · `move 4 1` | edit it |
| `x2rock queue save "Tonight"` | save it as a Sonos playlist |
| `x2rock queue clear --yes` | empty it — Sonos keeps no undo, hence `--yes` |
| `x2rock queue sources` · `queue add "<name>" [--next]` | what can be appended, and appending it |
| `x2rock favorites [query] [--json]` | saved favorites, household-wide |
| `x2rock keep [name] [--container]` · `bookmarks [--all]` · `bookmark "<name>"` | remember what is playing and replay it |

See [The queue](#the-queue) for why it is versioned and what can and cannot be appended, and
[Keeping things you cannot search for](#keeping-things-you-cannot-search-for) for `keep`.

### Music services and radio

| | |
|---|---|
| `x2rock search [-s <svc>] [<term>] [--count N] [--index N]` | search a service; bare, it lists what can be searched |
| `x2rock browse [-s <svc>] [<container>] [--count N] [--index N]` | walk a service's own containers |
| `… --play N` | play the Nth hit of the page returned |
| `x2rock play-item -s <svc> <id>` · `queue-item` | play, or queue, a hit you already have the id for |
| `x2rock stations "<name>" \| --tag jazz \| --country GB [--play N]` | tens of thousands of internet radio stations, no account |
| `x2rock play-url "<http url>" [--title "<name>"]` | any stream URL, no service at all |
| `x2rock link [<svc>] [--no-open]` · `accounts [--json] [--content]` · `unlink <svc>` | link an account so a service can be searched |

Everything under [Music services](#music-services).

### Alarms

| | |
|---|---|
| `x2rock alarms [--json]` | every alarm in the household, with its room |
| `x2rock -r <Room> alarms add 07:00 [--program "<favorite>"] [--recurrence daily] [--volume 25] [--duration 30m] [--off]` | create one |
| `x2rock alarm <id> on\|off` · `alarm <id> remove --yes` | arm, disarm, delete |
| `x2rock -r <Room> snooze [9m]` | **silence the alarm that is going off** — disarming and removing do not |

Alarms are household-wide and addressed by id, not room. Three things surprise people: the time
is the *household's* clock, which runs UTC if the household has no timezone set (`alarms add`
prints the household's clock so you can compare); an alarm sets the room's volume and leaves it
there; and one created with under two minutes' notice fires about two minutes late.

### Speaker settings

| | |
|---|---|
| `x2rock -r <Room> eq [--bass N] [--treble N] [--loudness on\|off] [--trueplay on\|off]` | tone, per speaker; bare, it reads |
| `x2rock -r <Room> eq --night on\|off --dialog on\|off` | night mode and speech enhancement, soundbars only |
| `x2rock -r <Room> remote [--feedback on\|off] [--repeater on\|off]` | a soundbar's TV-remote settings |
| `x2rock -r <Room> led [on\|off]` | the status light |
| `x2rock -r <Room> buttons [lock\|unlock]` | lock the touch controls on the speaker itself |

These are **per speaker**, unlike almost everything else, which is per group: a stereo pair has two
lights, and two rooms playing together keep their own tone. Loudness is on from the factory.
TruePlay here means *know whether it is on, and turn it off* — a speaker carried to another room
is still applying the curve measured for the room it left — not measuring a new one, which needs
the Sonos app and a phone microphone.

### Several rooms at once

`-r` is repeatable for the per-room commands — `vol`, transport, `repeat`, `shuffle`, `crossfade`:
`x2rock -r Kitchen -r Bedroom vol 10` applies to each with the topology resolved once, one line
per room. `--all` fans a per-room command over every group. A fan-out that hits an error stops
there and names the room — the rooms before it already applied, so a relative change must not be
re-run whole.

## For scripts and agents

### `--json` everywhere

Every data command takes `--json` and its shape is stable. `status --json` is a bare array with
one object per **group**, and it is the snapshot to read first: now-playing is flat on the object
(`title`, `artist`, `album`, `position_ms`, `duration_ms`, `next_title`), grouping is `members` and
`coordinator`, and `audible` folds mute and level into the one question "will this make a sound".
`status --json --full` wraps it in `{household, network, total, reachable, warnings, rooms}`.

Two shapes differ from the rest and are worth knowing: `search --json` and `browse --json` answer
an **envelope**, `{total, index, items}`, because one page cannot say how much there is — page with
`--index`. `queue --json` is `{current, in_use, items}`. `favorites`, `bookmarks`, `accounts`,
`alarms` and `rooms` are bare arrays.

### Errors are data

A command that fails with `--json` prints one object to **stderr** and exits non-zero:

```json
{"code":"unknown_room","error":"no room named \"bedoom\" …","fix":"x2rock rooms",
 "did_you_mean":["Bedroom"],"rooms":["Bedroom","Living Room","Guest TV","Kitchen","Dining Room"]}
```

`code` is stable, `error` is the sentence the plain CLI prints, and `fix` is a command that
resolves it — verbatim, runnable — when one exists. **When `fix` is non-null, run it and retry.
When it is null, read `error` and change the request.**

| `code` | meaning | `fix` |
|---|---|---|
| `unknown_room` | `-r` is not a room (or is a group's composite label); carries `did_you_mean` and `rooms` | `x2rock rooms` |
| `too_many_rooms` | several `-r` on a command that takes one | null |
| `needs_link` | no token here for that music service | `x2rock link '<svc>'` — a browser login a person must finish |
| `no_search_categories` | the service is browse-only, not broken | `x2rock browse -s "<svc>"` |
| `bad_stream_url` | `play-url` needs an `http(s)` URL | null |
| `stream_did_not_play` | the player took the URL and is still idle 10 s later — the stream is dead, the room is fine | null (try another) |
| `stream_unverified` | the room's state could not be read for 10 s — *not* a verdict on the stream | null (re-check with `now`) |
| `playback_failed` | `play` reached the room but nothing started; the message says which source failed | null (load a fresh source) |
| `no_player` | known network, remembered speakers, and a rescan found **nothing at all** | **null** — likely powered off; `discover` would only repeat the scan |
| `unregistered_network` | no speakers are known here — normal away from home | **null** — `discover` is *offered*, never auto-run |
| `multiple_households` | more than one household, and nothing said which | `x2rock households` |
| `unknown_household` | the `-r` room or `--household` matched none | `x2rock households` |
| `household_unreachable` | a rescan found *other* households but not this one — it is off or has moved | `x2rock households` |
| `unknown` | no known remedy — e.g. `pause` on an idle room | null |

The two null network codes matter most. Neither carries `x2rock discover` as a fix on purpose: it
scans the local network, and a laptop must not probe a hotel or client WiFi unasked. Away from
home the right answer is "your speakers are not on this network", not a scan of it.

### Environment

| variable | |
|---|---|
| `X2ROCK_ROOM` | the default `-r` |
| `X2ROCK_PLAYER` | a player address, bypassing what is remembered (`--ip`) |
| `X2ROCK_HOUSEHOLD` | which household, when a network carries more than one and no room says (`--household`) |
| `X2ROCK_DUMP_SMAPI=1` | print every music-service request and reply, credentials omitted |
| `X2ROCK_LOG_VERBOSE=1` | daemon: log every retry and the backoff ramp |
| `X2ROCK_LOG_EVENTS=1` | daemon: log every event body as the player sent it |

### The agent skill

`x2rock skill` installs a [Claude Code](https://claude.com/claude-code) skill teaching an assistant
on this machine the whole surface — the `status --json` snapshot, the error contract, grouping
semantics, the traps (`audible:false`, TV input with no signal, favorite drift), what to confirm
before acting in a shared house, and what is safe to repeat:

```sh
x2rock skill              # → ~/.claude/skills/x2rock/ (or $CLAUDE_CONFIG_DIR/skills/)
x2rock skill --dir path   # somewhere else, e.g. a project's .claude/skills
x2rock skill --print      # to stdout, to inspect or to seed a non-Claude agent
```

The skill is embedded in the binary, so it matches the CLI it documents; re-run it after an
upgrade. Its source is [`skills/x2rock/SKILL.md`](skills/x2rock/SKILL.md), and tests hold the
binary to it: every field `status --json` emits must be named there.

### Shell completions

`x2rock completions <shell>` generates scripts for Bash, Zsh, Fish, Elvish and PowerShell. Bash,
Zsh and Fish also complete `-r` from the rooms remembered on this network, `-s` from the cached
service catalogue and `bookmark` names from what was kept — from local state, so `<Tab>` never
waits on a speaker.

```sh
x2rock completions bash --install    # → ~/.local/share/bash-completion/completions/x2rock
x2rock completions fish --install    # → ~/.config/fish/completions/x2rock.fish
x2rock completions zsh --install     # → ~/.local/share/zsh/site-functions/_x2rock
# zsh: put `fpath=(~/.local/share/zsh/site-functions $fpath)` in .zshrc before compinit
```

## The daemon and MPRIS

`x2rock daemon` publishes each group as `org.mpris.MediaPlayer2.x2rock-<room>` — "Media Room"
becomes `x2rock-media-room` — with full metadata, so the track, artist and cover art appear
wherever your desktop already shows what is playing, and anything that speaks MPRIS drives Sonos
with no further setup:

```sh
playerctl -p x2rock-media-room play-pause
playerctl -p x2rock-media-room metadata     # title, artist, cover-art URL
playerctl -p x2rock-media-room next
```

State comes from the players' push events. The daemon keeps one WebSocket per group coordinator,
pings them to survive firewall idle timeouts, and reconnects with backoff. logind says when the
machine wakes and NetworkManager says when it lands on a network, so a socket that did not survive
a suspend or a move is replaced within seconds; both are optional, and without them the keepalive
finds a dead socket a little later. When no player is reachable — a laptop away from home — it
backs off quietly and republishes when one appears. `journalctl --user -u x2rock` is where it says
what it is doing — starting with which binary it is, since a unit can outlive a reinstall by
another route.

### What the daemon publishes beyond MPRIS

MPRIS has no notion of a group, a member's own volume, a TV input or a live stream, so the daemon
carries those as extra keys on each player's `Metadata`. They are what the bar widget and the TUI
render, and any other consumer can read them the same way:

| key | |
|---|---|
| `x2rock:members` | the rooms in this group, so "everything is grouped" can be told from "one room" |
| `x2rock:memberVolumes` · `memberMuted` · `memberFixedVolume` · `memberVolumeLevels` | per-member arrays, in `members` order |
| `x2rock:muted` · `volumeLevel` · `fixedVolume` | group mute; the level regardless of mute (MPRIS volume reads 0 while muted); a Port or Amp with a fixed line-out |
| `x2rock:canRepeat` · `canRepeatOne` · `canShuffle` · `canCrossfade` · `crossfade` | what the current source allows, so a client dims rather than fails |
| `x2rock:noSource` | nothing loaded at all — the app's *No content* |
| `x2rock:hasTvInput` · `onTvInput` · `inputFormat` | a soundbar, whether it is on TV, and the format it is receiving ("Dolby Digital 5.1") |
| `x2rock:isLiveStream` · `stationName` · `streamInfo` | a radio stream, its station, and its own now-playing text |
| `x2rock:hasTrackId` | the current item has a real service track id — the gate for rating |
| `x2rock:queueVersion` | changes whenever the queue changes, including from the Sonos app — read from UPnP, since the Control API has no such field |

## Front ends

The daemon is the product; these are three of its consumers, and none of them is required.

### Any MPRIS client

GNOME's and KDE's media controls, `playerctl`, Waybar's `mpris` module, the lock screen and the
keyboard's media keys already drive Sonos through the daemon. That is most of the value of any
front end, on any Linux with systemd and a session D-Bus, with nothing else installed.

### Terminal UI

`x2rock tui` is the household on one screen, keyboard-driven — the front end for an ssh session, a
bare console, or a terminal already open. Each room is up to three lines: name and group volume,
what is playing, and a context line — station, the TV's audio format, who else is in the group,
repeat/shuffle/crossfade. `space` play/pause, `n`/`p` skip, `←`/`→` volume, `m` mute, `r`/`s`/`x`
the modes, `g` grouping (each member with its own volume; `enter` joins or leaves), `P` party mode
(it asks first), `t` TV input, `?` for the rest, `q` to quit.

It needs the daemon — the only thing here that pushes — and re-reads the daemon every thirty
seconds on top of the events, so a dropped signal repairs itself; if those reads stop, the header
says how long it has been. Reads and most writes go over MPRIS; grouping, party, TV input and one
speaker's volume run this same binary as a subprocess, and an error there is the CLI's own sentence.
Favorites, the queue, alarms and tone stay CLI commands: MPRIS carries none of them.

### Omarchy bar widget

![The x2rock bar popup: every room with per-room transport, volume and TV badges](quickshell/x2rock.sonos/preview.png)

`quickshell/x2rock.sonos/` is a Quickshell plugin for [Omarchy](https://omarchy.org)'s bar: every
room in a popup with now-playing, transport, repeat and shuffle, and the piece nothing else on a
bar has — **per-room volume**. Scroll the pill to change the focused room's volume; middle-click
toggles play; the popup has grouping (each member's own slider, and *Normalize* when they differ),
the queue (click to jump, move or drop), party mode, thumbs up/down where the daemon says the
track is rateable, and a favorites picker that also searches and browses services. Cover art
comes from the speaker itself and falls back to a glyph. It is entirely event-driven off the
daemon and hides itself when there is no daemon.

```sh
cp -r quickshell/x2rock.sonos ~/.config/omarchy/plugins/
omarchy-shell shell rescanPlugins
omarchy plugin enable x2rock.sonos --section right
```

Every glyph, size and behaviour is set on the widget's entry in `~/.config/omarchy/shell.json` —
documented in [`quickshell/x2rock.sonos/README.md`](quickshell/x2rock.sonos/README.md), installed
alongside the plugin. Edit that, not the QML: the plugin installs by copy.

This is the one part of x2rock with a desktop dependency. `grep -ri omarchy src/` finds nothing.

## Music services

```sh
x2rock search                                   # what can be searched here
x2rock search -s tunein                         # that service's categories
x2rock search -s tunein jazz                    # search it
x2rock search -s somafm --play 3 ambient        # play the third hit
x2rock browse -s iheartradio                    # a service's own root
x2rock browse -s iheartradio for_you --play 1   # open a container, play a row
x2rock stations --tag jazz --limit 5 --play 1   # internet radio, no service at all
x2rock play-url https://ice5.somafm.com/groovesalad-128-mp3 --title "Groove Salad"
```

### Searching and browsing

A music service is linked to the **household**, not to a Sonos login, and a speaker hands any
controller on the LAN the service's endpoint and search categories with no credential. Of the 108
services Sonos knows about, **32 declare anonymous access** — most of the radio-shaped ones —
and twenty of those publish a search category; the other twelve are browse-only, and `search`
says so and points at `browse`. `browse` therefore reaches more services than `search` does.

`search` takes a word; `browse` takes a *place* — a personal library, a "For You", a genre tree.
Every service starts at `root`; a row marked as a container can be opened, everything else can be
played. **A container cannot be played whole** — an album, a playlist, a show. This is a limit of
the local API rather than of x2rock: four routes were tried against real hardware and all four
fail, including replaying a player's own stored favorite URI back to it. Saved as a favorite in
the Sonos app, the same container plays fine with `x2rock favorite`, because that hands the player
an id and lets it resolve the thing itself.

Both page. `--count` is the page size and `--index` the 0-based start; `--json` answers
`{total, index, items}`, and there is more whenever `index + items.len() < total`. `--play N`
counts within the page returned.

`--play` plays a hit: a live stream opens a playback session and leaves the queue alone; anything
on-demand is added to the queue, because that is the only way a player resolves a service's own
media. Do not trust a service's `canPlay` flag — iHeartRadio marks an `artist_radio` collection
playable and refuses to play it; what decides is whether the row is a container, which `browse`
reports.

### Internet radio

`x2rock stations` searches a community directory — [Radio Browser](https://www.radio-browser.info),
no key, no account, tens of thousands of stations — by name, `--tag` or `--country`, and `--play N`
plays a hit. `x2rock play-url` plays any HTTP stream URL directly. Both **wait for the room to
actually reach `PLAYING`** before confirming anything, because a player accepts a URL it cannot
play and then sits silently idle: a good stream costs about four seconds, a dead one fails after
ten with `stream_did_not_play`. `--no-wait` returns at once for scripts that check for themselves.

### Linking an account

```sh
x2rock link                     # services that can be linked
x2rock link bandcamp            # link one: a browser login you finish
x2rock link plex                # Plex's own PIN flow
x2rock accounts                 # what is linked here
x2rock unlink bandcamp
```

**A link buys search and browse. It does not, by itself, buy playback of on-demand tracks.**

Fourteen services offer *device linking*: `x2rock link <svc>` opens the service's own login page in
your browser, waits for you to finish, and stores the token the service mints — no Sonos account,
no partner registration, nothing embedded. Over ssh, `--no-open` prints the URL. The remaining
services are *app-link*, and that tier is not uniformly closed: `link` asks any of them for a
browser page and lets the service answer. When last swept, TuneIn (New), Radio Paradise, Amazon
Music, Pandora and Spotify gave one; **YouTube Music, Apple Music and SoundCloud refuse.**
YouTube Music is closed for a reason nothing here can move — it wants an API key Sonos seals
inside its own apps.

**Plex** is linked through Plex's own PIN flow, and the token appears on your Plex account's device
list as `x2rock-<hostname>`, where it can be revoked. On a server without Remote Access,
`link plex --from-player` reads the household's own Plex token off the art URLs your players
already broadcast instead.

**Playing an on-demand track is different.** The track is added to the queue and the *speaker*
fetches it using the **household's own account** for that service, added in the Sonos app — so a
track plays only if the household has one. Without it, x2rock falls back to streaming the item
with its own token, which works where the service hands back a playable URL (Amazon Music, TuneIn
stations) and not where it does not (Spotify, until the household adds it in the Sonos app).
`link` also asks the household to match the account (`--no-match` skips it); it has only ever
matched an account the household already held, and never creates one.

Linking is not always a catalogue: Bandcamp's Sonos interface is *your own collection*, so on a
fresh account `search -s Bandcamp` correctly finds nothing. Check what a service exposes before
assuming a link makes it searchable.

The token lives in `~/.local/state/x2rock/credentials.json` at mode `0600` — deliberately not a
keyring, which would put a locked or missing keyring between you and your music in a tool expected
to work over ssh and inside a widget's subprocess. `unlink` forgets the local copy; revoke it on the
service's own site. An expired token usually heals itself: when a service answers with a
replacement, x2rock retries once and stores it.

**Talking to a music service is the only thing x2rock does that leaves the LAN, and it is confined
to the CLI.** The daemon speaks to nothing but the local network, so a slow or unreachable service
cannot delay play, pause or volume.

### Keeping things you cannot search for

For a service you cannot search — YouTube Music, Apple Music — *replaying* something needs no
credential at all: the id is enough, and the player resolves the account it already holds.

```sh
x2rock keep                  # remember what is playing
x2rock keep "Friday mix"     # under a name of your own
x2rock keep --container      # the album, playlist or station rather than the track
x2rock bookmarks             # what has been kept
x2rock bookmarks --all       # ...plus what the daemon noticed playing, newest first
x2rock bookmark Bodies       # play it again;  --next queues it after the current track
x2rock bookmarks remove Bodies
```

Start something once from the Sonos app, keep it, and it is a command from then on — for every
service the household has linked, since x2rock never sees a token. A kept item lives exactly as
long as the household's account for that service does; disconnect the service in the Sonos app
and the player refuses the same id, re-add it and the same kept item plays again. Kept entries
never expire; the daemon's history keeps the last fifty. Both live in
`~/.local/state/x2rock/bookmarks.json`, on this machine.

## The queue

The queue is not reachable through the Control API, cloud or local, so this goes over UPnP on port
1400 — which needs the Sonos **UPnP** setting on; it is on by default, see [Requirements](#requirements).

Sonos versions the queue and enforces it: a change sent against a version that has moved on is
refused outright rather than applied to the wrong tracks. Each change reads the current version
immediately before sending, and if someone edits the same queue from the Sonos app in that
instant, the change fails and says so. `clear` requires `--yes` because Sonos keeps no undo;
`save` first is cheap insurance.

`queue add` appends a saved Sonos playlist, or any favorite that is a single track. It cannot
append a station or a collection — an album, a playlist, a stream — because Sonos will only play
one of those *in place of* the queue; `queue sources` says which is which, and `favorite` or
`playlist` replaces the queue with it, as the Sonos app does. `queue --json` reports `in_use`, the
app's *Queue (Not In Use)*: a room on a stream or on its TV input has a queue that is not what is
playing.

## Probing and debugging

`x2rock raw` sends one command straight to a player and prints the reply. It exists because both
wires are wider than the CLI covers, and settling what one answers should not need a rebuild. Two
transports, as two subcommands, because they share no grammar:

```sh
x2rock raw api  favorites:1 getFavorites                                 # Control API, WebSocket
x2rock raw api  --scope group playback:1 getPlaybackStatus -r Kitchen
x2rock raw api  --watch 8 musicServiceAccounts:1 subscribe               # read a subscribe's events
x2rock raw upnp DeviceProperties GetZoneAttributes -r Kitchen            # UPnP/SOAP, port 1400
x2rock raw upnp --scope group AVTransport GetCurrentTransportActions InstanceID=0 -r Kitchen
```

**A refusal is a result**: a player-side error prints and exits 0, so a loop over candidate
commands is not stopped by the first unsupported one — while an *unreachable* speaker exits
non-zero, so the two are told apart. `raw api --scope` is the flag a probe gets wrong first
(`playback:1` wants `group`, `playerVolume:1` wants `player`, `favorites:1` wants `household`;
the key travels in the header, not the body). `raw upnp` takes flat `Name=Value` arguments, most
actions want `InstanceID=0`, an unknown service name lists the sixteen there are, and its reply is
an array of `{name, value}` in the player's own order. `raw` can mutate state; it is a probe, not a
feature, and never a way around an error.

`X2ROCK_DUMP_SMAPI=1` prints every music-service request and reply with the credentials header
replaced. For the daemon, `X2ROCK_LOG_VERBOSE=1` restores every retry and the backoff ramp, and
`X2ROCK_LOG_EVENTS=1` logs every event body as the player sent it. Under systemd those go in a
drop-in; `systemd/logging.conf.example` is that drop-in with both lines commented out:

```sh
mkdir -p ~/.config/systemd/user/x2rock.service.d
cp systemd/logging.conf.example ~/.config/systemd/user/x2rock.service.d/logging.conf
systemctl --user daemon-reload && systemctl --user restart x2rock.service
```

## Requirements

- **Linux**, and a Sonos **S2** speaker on the same network. S1 is not supported.
- **Rust 1.89 or newer** to build, and a C compiler (`ring` compiles C and assembly in its build
  script; `cmake` is not needed). No network access at build time beyond fetching crates. Rolling
  and recent distributions package something newer; long-term releases do not — see Ubuntu below.
  **The fix is a newer toolchain, never a smaller number in `Cargo.toml`**: the code is
  edition-2024, and lowering `rust-version` trades one clear refusal for a page of syntax errors.
- **The Sonos UPnP setting**, for the queue, alarms, tone, and the other speaker settings. It is
  **on by default**; if it has been switched off, it is in the Sonos mobile app under
  *Account → Legal and Privacy → Privacy & Security → Connection Security → UPnP*. The same switch
  disables the macOS and Windows Sonos apps. Playback, volume, grouping and favorites need nothing.
- **logind and NetworkManager are optional** — they are how the daemon learns it woke or moved
  networks. Without them it says so once at startup and recovers a little more slowly.
- **No inbound connections.** Everything is outbound TCP, so a default-deny firewall needs no rule.

### On Ubuntu

`apt`'s Rust is too old — 24.04 LTS is on 1.75 — so `apt install cargo` is a dead end; install a
toolchain from [rustup.rs](https://rustup.rs). And `~/.local/bin` may not be on `PATH` yet:
Ubuntu's `.profile` adds it only if it exists when the shell starts, so after the `install` above
log out and back in, or `export PATH="$HOME/.local/bin:$PATH"` for the session at hand — otherwise
the daemon and the service fail with nothing obviously wrong.

### On a headless box

A server, or a box that exists to sit on the speakers' LAN and be reached over ssh, is a natural
home. The unit is tied to the graphical session by default; on a machine that never has one it
would never start, so `--headless` adds the drop-in that points it at the target the user manager
always reaches, and lingering keeps that manager alive with nobody logged in:

```sh
x2rock service install --headless --enable
loginctl enable-linger $USER      # the line that gets missed; sudo it if polkit refuses over ssh
```

Then `x2rock tui` over ssh is the every-room view and `x2rock status --json` the same for scripts.

### More than one Sonos household

An office, a lab, a guest system on the same LAN. Every command works out the household from the
room it was given, so `-r Studio` just works; `--household` (or `X2ROCK_HOUSEHOLD`) is for a
command that names no room — the daemon above all — or for a room name that exists in both
households, where only an id from `x2rock households` can say. The daemon without it logs
`multiple_households` and retries forever; `x2rock --household Studio service install` writes the
unit with the `Environment=X2ROCK_HOUSEHOLD=` line filled in. A household that has been
factory-reset or replaced is forgotten automatically once every one of its old addresses answers
for the new one.

## Tested devices

Everything here was developed against one household — eleven players in five rooms, re-read off
the speakers 2026-09-13:

| Device | Firmware | |
|---|---|---|
| Sonos Beam (S14) ×3 | 97.1-80312 (18.8) | first generation, so no Atmos; one wired, the household's only cable |
| Sonos One SL (S22) ×3 | 97.1-80312 (18.8) | one standalone, two bonded as surrounds |
| Sonos Play:1 (S12) ×2 | 86.10-80260 (17.2.7) | bonded as surrounds |
| Sonos Sub | 86.10-80260 (17.2.7) | in the 5.1 room |
| IKEA SYMFONISK Bookshelf (S21) ×2 | 86.10-80260 (17.2.7) | a stereo pair — a third-party player behaving identically |

Three of the five rooms are bonded sets: a 5.1 home theatre, a 5.0 one, and a stereo pair. The
firmware split is by hardware generation, not vendor, and one room runs both at once. Everything
wireless is on SonosNet through the one wired Beam — which `x2rock system` now shows.

Two devices would be especially useful to hear about, because they are the places the code is
written for a case it has never met:

- **Anything doing Atmos** — an Arc, an Arc Ultra, a Beam gen 2. The audio-format display handles
  height channels and would show `5.1.2`; every soundbar here is a first-generation Beam, which
  cannot.
- **A Port or an Amp**, with a real line-in. There is no `line-in` command yet: `playback:1
  loadLineIn` answers *"player does not have line-in"* on every speaker here, so only its refusal
  path can be verified. The command is a small addition once there is a speaker to test it against.
  An Amp would also settle `SubCrossover`, the one extended EQ type predicted to be Amp-only.

Era, Move, Roam, Playbar, Playbase, Play:3 and Play:5 are untested rather than known-bad; nothing
in the design expects a particular model. Sonos Ace headphones are not a target — they are
Bluetooth, not players on the network. Reports from anything else are welcome: open an issue.

## What stays with the Sonos app

x2rock *operates* speakers and reads their state. It does not provision, reconfigure or physically
re-shape the household — those are the Sonos app's own jobs, gated there behind a confirmation
dialog, a phone microphone or the setup flow, none of which a command line reproduces. The line is
drawn on purpose, not for want of an action:

- **Installing firmware.** `x2rock update` reads what is installed and offered and never writes:
  an update reboots speakers, and the app gates it behind a warning about not unplugging anything.
- **Onboarding a new speaker**, **removing one**, and **factory reset** — Sonos's private setup
  flow, or a physical button.
- **Creating or breaking bonds** — stereo pairs, a soundbar's Sub and surrounds. `system` reports
  them; it does not edit them.
- **Running TruePlay.** The measurement needs the app and a phone microphone. x2rock knows whether
  it is on and can turn it off; it does not measure.
- **Adding a music service to the household.** `link` gives *this machine* search and browse; the
  household's own accounts, which are what play on-demand tracks, are added in the app.

Everything else a Sonos household will do over the LAN is here, or declined for a reason recorded
in [docs/architecture.md](docs/architecture.md) — cloud queues, for one, because they would make
x2rock a server.

## Non-goals

- **Android.** [`x2rocktv`](https://github.com/rahga/x2rocktv) is the Kotlin Android TV app.
- **Cloud OAuth, or control from outside the LAN.** Deliberately cut; the transport seam remains.
- **Sonos S1.** Every supported device runs S2, and no S1 accommodation is carried anywhere.
- **A GUI of its own.** The daemon is the product; front ends are consumers of it.

## Design

The reasoning behind every choice — the local WebSocket over the cloud API, discovery behind a
default-deny firewall, the protocol facts verified against real hardware and the ones that turned
out to be wrong — is in [docs/architecture.md](docs/architecture.md). It is a dated engineering
log rather than a reference; its *Superseded claims* index at the top says which early findings
later ones overturned.

## Credits

Informed by prior reverse-engineering of the Sonos protocols by the community, in particular
[`sonos-websocket`](https://github.com/jjlawren/sonos-websocket) by jjlawren, whose handshake was
the concrete reference for the LAN API, and [Stephan van Rooij's Sonos API
documentation](https://sonos.svrooij.io/) for the UPnP side.

## Licence

[0BSD](LICENSE), Copyright (C) 2026 Richard Hoelscher. Use it for anything, no attribution
required — and that extends to packaging: nobody needs to ask.

Clarifications:

- **Contributions.** Pull requests to this repository must be compatible with 0BSD.
- **The name.** *x2rock* is a trademark retained by the author. 
