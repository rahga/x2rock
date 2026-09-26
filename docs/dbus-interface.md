# The daemon's own D-Bus interface — design note

> **Status: proposed 2026-09-25, not built.** Nothing here exists yet. It is written against the
> daemon as it stands and against what its two consumers — the bar widget and the TUI — actually
> shell out for today, so that the first slice can be built and judged against something concrete.
> The protocol facts it leans on are in [`architecture.md`](architecture.md) and are cited rather
> than restated; where this note and that file disagree, that file is the one that was tested.

## Why

The daemon publishes MPRIS, and MPRIS is the right interface for what it covers: transport,
metadata, cover art, repeat and shuffle, and a group's absolute volume. Everything it cannot say
travels a second route. State that MPRIS has no field for rides as extra `x2rock:*` keys on
`Metadata`; commands MPRIS has no method for run the CLI as a subprocess and parse what it prints.
Both front ends do this, and both say why in their own source (`src/tui/action.rs`,
"Rule: talking to a service never enters the daemon" in `architecture.md`).

That second route works, and it has four costs that grow with each new consumer:

- **A subprocess per click.** Every grouping change, member slider, TV switch and volume step
  starts a process, which opens its own sockets to the players and throws them away — while the
  daemon already holds connections to every coordinator and every member.
- **Errors arrive as text.** The TUI takes the last line of stderr and strips `Error: `; the widget
  keys off exit codes and empty stdout. The CLI's `{error, code, fix}` shape only exists under
  `--json`, and not every call the front ends make asks for it.
- **State is keyed by a name that moves.** MPRIS publishes one bus name per *group*, named for its
  coordinator, and the daemon tears every name down and republishes on each topology change
  (`daemon.rs`, "group topology changed; republishing"). A client that wants to follow *a room*
  through a regroup has to rejoin it by matching `x2rock:members` across the new players.
- **Metadata keys are an unversioned side channel.** They are documented in the README and read by
  string, with nothing that can tell a client a key was renamed or what type it should be.

A GNOME front end (gx2rock) would be a third consumer carrying all four, and a sandboxed one could
not use the subprocess route at all. A typed interface on the daemon removes the subprocess route
for everything that stays on the LAN.

## What it is not

- **Not a replacement for MPRIS.** Transport, metadata and cover art stay there, because every
  desktop already reads them. The new interface links each group to its MPRIS name rather than
  duplicating what that name carries.
- **Not a replacement for the CLI.** The CLI still works with no daemon running and remains the
  agent surface. The interface is the daemon's; the CLI does not become a client of it.
- **Not a way into the internet.** Search, browse, stations, `link`, `accounts`, `play-item`,
  `queue-item`, `rate` (SMAPI `rateItem`) and `bookmark` (whose stream fallback asks the service)
  all leave the LAN and are excluded by the standing rule, unchanged. They stay CLI commands, and a
  front end runs them as subprocesses exactly as the widget does now. This is the line the
  interface is not allowed to blur, and the reason it can be put in front of play/pause at all.
- **Not a GUI.** The README's non-goal stands: this is a second public front door on the product,
  and every GUI remains a consumer of it.

## Names

| | |
|---|---|
| Bus name | `io.github.rahga.x2rock` |
| Root object | `/io/github/rahga/x2rock` — `…Household1`, plus `org.freedesktop.DBus.ObjectManager` |
| Rooms | `/io/github/rahga/x2rock/room/<player id>` — `…Room1` |
| Groups | `/io/github/rahga/x2rock/group/<group id>` — `…Group1` |
| Errors | `io.github.rahga.x2rock.Error.<Code>` |

Reverse-DNS on the GitHub namespace is the freedesktop and GNOME convention for a project without
its own domain. `x2rock` as a bare name is not a valid well-known bus name, and `org.x2rock` would
claim a domain nobody here owns. If gx2rock takes an app id, it would be
`io.github.rahga.gx2rock` under the same root, which keeps the two unconfusable.

**The version is in the interface name.** Within `Household1` members may be added, never changed
or removed. A breaking change is `Household2`, served beside `1` for a release. The bus name and
object paths carry no version.

**Player ids in paths are escaped** the D-Bus way. `RINCON_48A6B81853E001400` is already a valid
path element; group ids (`RINCON_…:3406530134`) contain `:`, which is not, and are escaped with
`_3a`-style encoding. Clients read the id from the `Id` property rather than parsing the path.

## The object model: rooms stay put, groups come and go

Grouping is the thing MPRIS models worst, so it is what the object tree is built around.

- **A room object lives as long as the daemon knows the player.** Its path is keyed by player id,
  which is stable across regrouping and renaming — "Grouping over the local API" is explicit that
  a room is resolved through players, never group names. A client holding a room path keeps holding
  it through any regroup.
- **A group object lives as long as the group.** It appears when Sonos forms a group and disappears
  when it dissolves, through `ObjectManager`'s `InterfacesAdded`/`InterfacesRemoved`. That is
  exactly the churn MPRIS republishing produces today, reported as what it is instead of as every
  player vanishing and reappearing.
- **Each points at the other.** `Room1.Group` is the group's path; `Group1.Members` lists room
  paths, coordinator first. `Group1.MprisName` is the bus name of the group's MPRIS player, which
  is how a client joins the two: follow rooms and groups here, read what is playing there.
- **A room is what the Control API calls a player.** A bonded set — soundbar, Sub and surrounds —
  is one room, and the individual speakers stay where `x2rock system` shows them. Nothing in this
  interface addresses a satellite speaker.

`GetManagedObjects` on the root is the whole household in one call, which is the interface's
`status --json`.

### `Household1` (root object)

| Property | Type | |
|---|---|---|
| `Id` | `s` | household id |
| `Connection` | `s` | `connected`, `connecting`, `unreachable`, `unregistered_network`, `no_player` |
| `ConnectionFix` | `s` | the command that resolves it, when one exists (`x2rock discover`) |
| `Party` | `b` | every room is in one group |

`Connection` is what the daemon's retry loop already knows and currently only says to the journal.
A front end needs it to tell "no speakers" from "daemon not running" from "on the wrong network";
the codes are the CLI's own (`unregistered_network`, `no_player`) so a client needs one table.

| Method | |
|---|---|
| `StartParty(o host)` | every room joins `host`'s group |
| `EndParty()` | every room its own group |

### `Room1`

| Property | Type | |
|---|---|---|
| `Id` | `s` | player id |
| `Name` | `s` | the room's name, as every Sonos app shows it |
| `Group` | `o` | the group it is in |
| `IsCoordinator` | `b` | |
| `Volume` | `u` | this room's own level, 0–100 — `playerVolume:1` |
| `VolumeAvailable` | `b` | false while this member's socket is down; the slider freezes rather than lies |
| `FixedVolume` | `b` | a Port or Amp with a fixed line-out |
| `HasTvInput` | `b` | a soundbar |

| Method | |
|---|---|
| `SetVolume(u level)` | this room's level inside its group — the balance, `vol --player` |
| `Leave()` | leave its group, `ungroup` |
| `SwitchToTv()` | soundbars only; fails `not_a_soundbar` otherwise |

**`Room1` has no mute.** "Per-player volume, and who may be asked" decided mute is group-only in
x2rock, because a lone muted member is a puzzle to find later. The interface keeps that decision
rather than reopening it.

**`VolumeAvailable` is the `Loss::Tolerated` rule made visible.** A member socket is best-effort and
its loss is swallowed so one flaky portable cannot tear down the household; today that shows as a
slider that silently stops moving. Publishing it lets a client grey the slider instead.

### `Group1`

| Property | Type | |
|---|---|---|
| `Id` | `s` | group id |
| `Name` | `s` | Sonos's own label, "Dining Room + 1" — for display only, never for addressing |
| `Coordinator` | `o` | |
| `Members` | `ao` | coordinator first |
| `MprisName` | `s` | `org.mpris.MediaPlayer2.x2rock-dining-room` |
| `Volume` | `u` | the group's level regardless of mute — what `x2rock:volumeLevel` carries today |
| `Muted` | `b` | |
| `Balanced` | `b` | every member at the group level — `vol --json`'s `balanced` |
| `Crossfade` | `b` | |
| `CanCrossfade` | `b` | |
| `OnTvInput` | `b` | |
| `InputFormat` | `s` | "Dolby Digital 5.1", empty when not on TV |
| `QueueVersion` | `s` | the `Q:0` `UpdateID`, empty when unknown (phase 2) |

| Method | |
|---|---|
| `Add(ao rooms)` | rooms join this group — `modifyGroupMembers` with `playerIdsToAdd` |
| `Remove(ao rooms)` | rooms leave — `modifyGroupMembers` with `playerIdsToRemove` |
| `SetVolume(u level)` | the group mix, preserving balance — `setVolume` |
| `NudgeVolume(i delta)` | a stateless step, clamped — `setRelativeVolume` |
| `SetMuted(b muted)` | |
| `SetEachVolume(u level)` | every member flat — `vol --each` |
| `Normalize()` | every member to the group level — `vol normalize` |
| `SetCrossfade(b on)` | |

`Add` and `Remove` return nothing. The resulting group arrives as signals, which are the only
account a client should trust; see the next section.

## Rules the interface inherits

These are Sonos's rules and this project's decisions, in `architecture.md` already. The interface's
job is to make them hard to break from the client side.

- **Commands are acknowledged, not reported.** A method returns when the player acks it. The new
  state arrives as `PropertiesChanged`, from the player's own event, and nowhere else. "Volume
  handling" measured a read straight after a write returning the old value for ~260ms, so a method
  that returned "the new volume" would be returning a guess. Clients render what the signals say.
- **The daemon never answers an event with a command.** It publishes the settled `groupVolume`
  event and stops. A client that sets volume from a `PropertiesChanged` handler has built the
  feedback loop Sonos documents; the note for client authors is below.
- **Stateless controls use `NudgeVolume`, sliders use `SetVolume`.** A scroll wheel, a key and a
  media key are steps; read-modify-write on them is the bug the rule exists to prevent. This is the
  one reason the widget's scroll still shells out, and it is the first thing phase 1 moves.
- **Group commands go to the coordinator, player commands to the player.** The daemon already
  routes this way; the interface only exposes group volume on `Group1` and player volume on `Room1`
  so a client cannot ask the wrong one.
- **Removing a coordinator moves its queue onto whoever stays.** Sonos's own behaviour, reproduced
  under "Grouping over the local API", and a single click in any members-with-a-leave-button UI.
  The interface does not prevent it — the Sonos app permits it — but `IsCoordinator` is published
  so a client can say so before the click rather than after.
- **Nothing here touches the internet.** Every method maps to the Control API on 1443 or UPnP on
  1400. A method that would need a service is a CLI command instead.

### For client authors

- Throttle what you send to roughly one volume command per 100ms. The player coalesces anyway; the
  throttle is what keeps a dragged slider from queueing a second of stale commands behind it.
- While the user holds a slider, show their value, not the property's. Take the property back on
  release. Never write the property's value back to the daemon.
- Follow rooms by path. Follow groups by `Room1.Group`. Never store a group path or a group name
  past the next `InterfacesRemoved`.

## Errors

A failed method returns `io.github.rahga.x2rock.Error.<Code>`, where `<Code>` is the CLI's code in
CamelCase — `unknown_room` is `…Error.UnknownRoom`, and the set is `hint::Code::ALL` plus the few
this interface adds (`NotASoundbar`, `StaleQueue`, `NotConnected`). The message is the CLI's own
sentence. When the CLI would offer a `fix`, the message ends with one more line:

```
this network is not one x2rock has seen
fix: x2rock discover
```

A `fix:` line rather than a second error argument, because GDBus and GJS hand a client the error
name and the first string and nothing else. The line is easy to split off and stays readable in a
client that does not bother.

## Phases

### Phase 1 — what the front ends shell out for today, LAN-only

Exactly the subprocess list in `src/tui/action.rs` and the LAN-side `Process`es in `BarWidget.qml`:
grouping (`Add`, `Remove`, `Leave`), party, the TV input, a member's own volume, relative volume,
mute, crossfade, `--each` and normalize — plus the `Room1`/`Group1` properties that let a client
stop reading `x2rock:*` keys for them.

**The test is migrating the TUI.** Every one of its subprocess calls moves onto the interface, and
`action.rs` shrinks to the internet-bound ones, of which the TUI has none. If the TUI can drop its
subprocess route entirely, phase 1 is complete; if it cannot, the gap is in the interface.

**The `x2rock:*` metadata keys stay.** The widget and any third-party reader depend on them, and
they cost nothing to keep publishing. The README gains a line saying the typed properties are
preferred.

### Phase 2 — the queue and favorites

Both are LAN (UPnP `Q:0` and `getFavorites`), and both are what the widget runs `queue --json` and
`favorites --json` for.

- `Group1.GetQueue() → (s version, a(ussss) items)` — position, title, artist, album, art URL.
- `Group1.PlayQueueItem(u position)`.
- `Group1.RemoveQueueItems(s expected_version, u first, u last)`,
  `Group1.MoveQueueItem(s expected_version, u from, u to)`,
  `Group1.ClearQueue(s expected_version)`, `Group1.SaveQueue(s name)`.
- `Household1.GetFavorites() → a(sss…)` and `Group1.PlayFavorite(s id)`.

**Every queue edit carries the version it was computed against**, and fails `StaleQueue` when the
queue has moved. "`queueVersion` does not exist" is the reason: rows carry positions, positions
renumber, and acting on a stale list removes a different track from the one on screen. The CLI's
own edits already read `UpdateID` first; the interface makes the client's view part of that check.
The limit that note records still holds — a silent append to a room that keeps playing emits no
event, so `QueueVersion` catches it at the next one.

Favorites the household can no longer play (the ones the Sonos app greys out) are filtered the way
the widget filters them, in the daemon, once.

### Phase 3 — speaker settings, alarms, the sleep timer

`GetTone`/`SetTone` on `Room1` (`a{sv}` so night mode and dialog can be absent on a non-soundbar),
alarms on `Household1` addressed by id as the CLI does, `Snooze` on `Room1`, and the sleep timer on
`Group1`. All UPnP, none evented, so all methods rather than properties — publishing a property the
daemon would have to poll would break the no-polling promise.

### Deliberately never

Search, browse, stations, linking, accounts, `play-item`, `queue-item`, `rate` and `bookmark`, for
the rule above. `rename`, firmware, discovery and `raw`, because they are administration and the
CLI is the right place for them.

## Cost, honestly

The zbus side is cheap: the daemon already serves the MPRIS interfaces for every group through
`mpris_server`, which is zbus underneath, and `#[interface]` is the same pattern. The state is cheap: the daemon already holds
every value phase 1 publishes, because it computes the `x2rock:*` keys from it.

**The expensive part is `main.rs`.** Command logic there prints as it goes — `action.rs` counts 159
`println!` sites against 26 extracted functions — and a method needs the doing without the printing.
Phase 1 only needs the grouping, volume and TV paths separated, which are the smallest and the most
already-factored. That separation is worth having for its own sake; the interface is what finally
makes it pay.

## Open questions

- **Does the daemon run commands over its held sockets, or over a fresh session per call like the
  CLI?** Held sockets are the point, but the CLI's paths are the tested ones. Starting with the
  CLI's session code inside the daemon, then moving to held sockets, keeps phase 1 honest.
- **Is `io.github.rahga.x2rock` right**, or does the project want a domain before it has a public
  interface under a name it cannot easily change?
- **Activation.** A `.service` file under `dbus-1/services` would let a client start the daemon by
  calling it. The systemd user unit is the supported start today, and two ways to start one daemon
  is one too many unless the D-Bus file just names the unit (`SystemdService=x2rock.service`).

## Review and recommendations: GNOME and desktop integration

A review of this interface against the needs of GNOME applications (e.g. a GTK4/Libadwaita client
like `gx2rock`, GNOME Shell extensions, or media applets) highlights several architectural
considerations and opportunities.

### 1. Resolve the sandboxing paradox (Flatpak vs. "subprocess-only" commands)

§ Why correctly notes that sandboxed front ends cannot rely on subprocesses. However, § What it is not
and § Deliberately never consign search, browse, stations, linking, accounts, `play-item`, `queue-item`,
`rate`, and `bookmark` to CLI subprocesses.

- **The Flatpak blocker:** Sandboxed Flatpaks cannot spawn arbitrary host CLI binaries without
  breaking sandbox boundaries (which Flathub rejects for media players). If these operations remain
  CLI-only, a Flatpak front end can never implement a music picker, stream directory, or track rating.
- **Why the standing rule does not preclude D-Bus:** The rule (*"talking to a service never enters the
  daemon"*, [`architecture.md`](architecture.md)) was established so that slow or hanging internet
  lookups never block local playback or volume. In an async Tokio runtime with zbus, D-Bus method
  calls run in their own spawned async tasks with strict timeouts (e.g., 5–8s). A timed-out service
  query fails only that call, leaving held WebSockets and local playback completely unaffected.
- **Recommendation:** Rather than excluding service interactions entirely from D-Bus, isolate them on a
  dedicated interface (e.g. `io.github.rahga.x2rock.MediaService1` or `Directory1` on the root or a child
  object). This preserves process cleanliness while giving sandboxed clients a typed, first-class route.

### 2. End the "two-bus tango": mirror transport and now-playing on `Group1`

Relying entirely on MPRIS for transport, metadata, and track progress forces dedicated GNOME apps into
a cumbersome dual-bus design:
- A client using `GDBusObjectManagerClient` must look up `Group1.MprisName`, establish a separate
  `GDBusProxy` to `org.mpris.MediaPlayer2.x2rock-*`, and synchronize two independent signal streams.
- On regrouping or coordinator handoff, MPRIS bus names are torn down and republished, causing proxy
  invalidation, reconnection races, and UI stutter.
- **Recommendation:** Keep standalone MPRIS bus names for generic desktop widgets (`playerctl`, Waybar,
  GNOME Shell lock screen), but expose core playback properties and controls directly on `Group1`:
  - **Properties:** `PlaybackStatus` (`s`), `Title` (`s`), `Artist` (`s`), `Album` (`s`), `ArtUrl` (`s`),
    `Duration` (`t` ms), `Position` (`t` ms), `CanControl` (`b`), `CanGoNext` (`b`), `CanGoPrevious` (`b`).
  - **Methods:** `Play()`, `Pause()`, `PlayPause()`, `Next()`, `Previous()`, `Seek(t offset_ms)`.
  This allows a GNOME app to manage grouping, volume, transport, and metadata through a single unified
  object tree.

### 3. Adopt D-Bus activation via systemd

In GNOME and freedesktop environments, D-Bus activation is the standard mechanism to launch user daemons
on demand.
- A service definition at `~/.local/share/dbus-1/services/io.github.rahga.x2rock.service` with
  `SystemdService=x2rock.service` delegates activation directly to the systemd user manager.
- This creates no duplicate lifecycle paths: systemd remains the single process manager, but opening
  a front end or logging into a desktop session can start the daemon automatically if it is not already
  running.
- **Recommendation:** Formalize `SystemdService=x2rock.service` as the activation strategy and install
  the D-Bus service file during `x2rock service install`.

### 4. Cache cover art locally

Currently, `ArtUrl` points to UPnP URLs served on port 1400 of the individual speaker (`http://<ip>:1400/getaa?...`).
- In GTK4 (`GtkPicture` / `GdkTexture`), loading remote HTTP images asynchronously requires custom networking
  scaffolding and forces sandboxed Flatpaks to request full LAN network permissions (`--share=network`).
- **Recommendation:** Have the daemon cache active album art into `$XDG_CACHE_HOME/x2rock/art/<hash>.jpg`
  and expose a local `ArtFile` property (`file://...`) alongside the remote `ArtUrl`.

### 5. Use extensible signatures (`aa{sv}`) instead of fixed tuples

Phase 2 proposes rigid tuple signatures such as `GetQueue() → (s version, a(ussss) items)` and
`GetFavorites() → a(sss…)`.
- Fixed tuples are brittle: adding fields (e.g. track duration, service identifier, track ID) changes the
  type signature and breaks wire compatibility for existing compiled bindings.
- In GIO/GDBus, dictionaries map cleanly to `GVariantDict` and bind naturally to UI list models (`GListModel`).
- **Recommendation:** Use dictionary arrays:
  - `GetQueue() → (s version, aa{sv} items)` with keys like `"id"`, `"position"`, `"title"`, `"artist"`, `"album"`, `"art_url"`, `"duration_ms"`.
  - `GetFavorites() → aa{sv}`.

### 6. Fill core CLI capability gaps needed for desktop and IoT integration

Several LAN features present in the CLI and used in desktop and IoT integrations are missing from the
current D-Bus proposal:
- **Audio Chimes & Notifications:** `x2rock chime` and `x2rock notify "<url>"` are essential for doorbell
  events, home automation alerts, and desktop notification sounds.
  - Add `Room1.PlayChime()` and `Room1.Notify(s uri, u volume)`.
- **Playlists:** The CLI provides `x2rock playlist "<name or id>"`. Phase 2 lists `SaveQueue` and `PlayFavorite`,
  but lacks playlist retrieval and playback.
  - Add `Household1.GetPlaylists() → aa{sv}` and `Group1.PlayPlaylist(s id)`.
- **Track Rating:** The CLI supports `x2rock rate up|down`, and the existing bar widget already surfaces rating
  badges when `x2rock:hasTrackId` is true. Rating belongs alongside transport controls in any modern player UI.
  - Add `Group1.Rate(s direction)` gated on a `CanRate` property.
- **Line-In / Optical Switching:** Generalize `Room1.SwitchToTv()` to support line-in on non-soundbars
  (Port, Amp, Five): `Room1.SwitchInput(s source)`.

### 7. GNOME Shell desktop integration opportunities

Beyond application windows, a first-class D-Bus service enables native desktop extensions:
- **GNOME Shell Search Provider (`org.gnome.Shell.SearchProvider2`):** Enables typing room names, favorites,
  or radio stations into the GNOME Shell overview search bar to immediately trigger playback.
- **Quick Settings Audio Panel:** A GNOME Shell extension or Quick Settings toggle can track household
  grouping and adjust per-room balance directly from the system top bar without launching a full app.

