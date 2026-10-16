# x2rock — architecture

The current reference for how x2rock works and why: the transports it uses, what the speakers
answer, and the decisions that follow. It states what is true now, and dates hardware observations,
since those depend on firmware. The investigation that produced it, about 10,000 lines of dated
log, is in git history: `git log -p -- docs/architecture.md` before the commit that replaced it.

Every protocol claim here was measured on a real speaker unless it says otherwise. Two households
were used. Home has eleven players in five rooms on firmware 97.1-80312 (display 18.8) for Beam and
One SL, and 86.x for the older generation. The office has a single One SL, build 97180312. Claims
about third parties (a vendor's menu, a live catalogue) go stale; claims about what the speakers
answer have held.

Where something is technically possible but outside the Sonos Platform Terms of Service, see
[the terms boundaries note](sonos-terms-boundaries.md). This document records conclusions, not how
they were reached.

**Reading it from another platform.** Most of this is protocol and transfers anywhere: the
integration path, the namespaces, volume, grouping, soundbars, the queue, favorites and SMAPI. Two
sections are host-specific, *The firewall problem* and *Discovery*, because both follow from
Omarchy's default-deny `ufw`. The Android TV sibling, [`x2rocktv`](https://github.com/rahga/x2rocktv),
keeps its own porting guide in `docs/porting-from-x2rock.md`.

## Scope

### The axiom, stated precisely

**x2rock never requires a Sonos account, unless the household has Authentication switched on.**

That is narrower than "never uses the internet". Searching a music service calls that service's
server, and `x2rock stations` calls a radio directory. Neither needs a Sonos login. The line is a
Sonos login, because depending on one would make x2rock a client of Sonos's cloud instead of a
client of the speakers on the LAN.

A household with **Authentication** on (Sonos app: Account > Privacy and Security > Connection
Security) refuses every unauthenticated Control API command. For that household only, x2rock has an
opt-in signed-in mode; see *Authentication on, and signing in*. Control still stays on the LAN.

Two rules follow:

- **Control never leaves the LAN.** The local transport is better at it: push events, one
  connection, no round trip.
- **Talking to a service never enters the daemon.** See the rule of that name.

### In scope and out

- **In:** local control over the LAN Control API, UPnP on :1400 for what that API lacks (the queue
  above all), an MPRIS2 daemon, a CLI built for scripts and agents, a terminal UI, and an Omarchy
  Quattro bar widget.
- **S2 only.** `<swGen>` in `http://<player>:1400/xml/device_description.xml` reads `2` on every
  supported device. S1 and S2 cannot share a household, so supporting S1 would mean a second content
  and grouping model.
- **Not wanted:** control from outside the LAN; Android, which is x2rocktv's job; a GUI of x2rock's
  own beyond its front ends.

### What stays with the Sonos app

x2rock operates speakers and reads their state. It does not provision, reconfigure or physically
re-shape a household. These are decisions, not gaps:

- **Installing firmware.** `ZoneGroupTopology BeginSoftwareUpdate` exists and x2rock will never
  offer it. The app gates it behind a dialog warning against unplugging speakers, and a flag cannot
  carry that moment of reading. `x2rock update` uses `CheckForUpdate`, which is read-only. This is
  scope, not a security boundary: `raw upnp` reaches every action.
- **Onboarding, removing or factory-resetting a speaker.** No public action exists for it.
- **Editing bonds.** `RemoveBondedZones` and `RemoveHTSatellite` exist, but they reconfigure the
  household. `system` reports bonds and does not edit them.
- **Measuring TruePlay.** That needs the app and a phone microphone. x2rock reads whether TruePlay
  is on and can turn it off.
- **Adding a music service to the household**, which only the app (or the desktop controller) does.
- **Editing favorites.** `favorites:1` offers only `getFavorites` and `loadFavorite`. Deleting one
  is possible through `ContentDirectory DestroyObject` on `FV:2/<id>` and stays a `raw` escape
  hatch. The rule: if the Control API does not sanction an edit, x2rock does not add it as a command.
- **Cloud queues.** `loadCloudQueue` is a permanent no. Sonos scopes the Cloud Queue API to content
  integrations, and it would make x2rock an inbound server the player polls.

## Integration path

Sonos speakers serve the same JSON Control API on the LAN that Sonos's cloud serves remotely, over a
WebSocket, with no OAuth and no internet. The official app has used it since the 2024 rewrite.
Sonos calls it not available for wide release; it is durable because Sonos's own app depends on it.

```
URL        wss://<player-ip>:1443/websocket/api
Headers    X-Sonos-Api-Key: 123e4567-e89b-12d3-a456-426655440000   (the community's sample key)
           Sec-WebSocket-Protocol: v1.api.smartspeaker.audio
           no Origin header
Wire       a two-element JSON array, [header, body], for commands, replies and events
```

- **The header rules are enforced.** An `Origin` header gets 403, and a missing API key gets 400.
  That is why a browser or WebView cannot connect: a browser always sends `Origin`.
- **The player's certificate is CA-signed, not self-signed.** It is a leaf-only chain, `CN=<MAC>`
  issued by "Sonos Device Authentication Root CA" (the root is not sent), RSA-2048, valid about six
  months. Its SAN carries `urn:sonos:udn:RINCON_…`, `sonos-<MAC>.local` and
  `sonos-<MAC>.smartspeaker.audio`, and no IP. The `.local` name derives from the player id: strip
  `RINCON_` and the trailing `01400`. x2rock accepts any certificate on the player socket, because
  a LAN already concedes active MITM. A music service gets real roots.
- **A reply can span several WebSocket frames.** `tokio-tungstenite` reassembles them.

Unauthenticated, these work for reads and writes: `groups:1`, `playback:1`, `playbackMetadata:1`,
`groupVolume:1`, `playerVolume:1`, `favorites:1`, `playlists:1`, `audioClip:1`, `playbackSession:1`
and `history:1`. `settings:1` reads, and refuses writes except the volume limit.

### Where the Control API stops, UPnP on :1400 takes over

The Control API has no view of the local queue: `queue:1`, `playbackQueue:1` and `cloudQueue:1` are
unsupported namespaces, and `skipToItem`/`loadQueue` are unsupported commands. It also has no alarms,
no sleep timer, no tone writes, no group-preserving TV switch, no room rename and no hardware
inventory. UPnP SOAP on :1400 has all of them.

| purpose | transport |
|---|---|
| control, state, events | WebSocket :1443, the spine |
| queue, content browsing, alarms, sleep, tone, TV handoff | UPnP SOAP :1400, **calls only** |
| remote (off-LAN) control | not built |

**Never UPnP eventing (GENA).** A subscription needs the player to connect back, which a default-deny
firewall drops silently. Read a service's own description (`/xml/<Service>1.xml`) before calling a
new action: it states argument names and ranges, and costs nothing.

### Namespaces, scopes and how to probe them

The target key (`groupId`, `playerId`, `householdId`, `sessionId`) travels in the **header**, never
the body.

| scope | namespaces |
|---|---|
| group | `playback:1`, `playbackMetadata:1`, `groupVolume:1`, `playbackSession:1` (by `sessionId`) |
| player | `playerVolume:1`, `homeTheater:1`, `audioClip:1`, `settings:1`, `info:1`, `hdmi:1`, `hardwareStatus:1`, `trueplay:1`, `ircontrol:1`, `effectiveSettings:1` |
| household | `groups:1`, `favorites:1`, `playlists:1`, `musicServiceAccounts:1`, `history:1`, `zones:1`, `devices:1`, `areas:1`, `alarms:1` |

- **A group command answers only on the group's coordinator, and a player command only on that
  player.** Anything else gets `ERROR_INVALID_OBJECT_ID` ("Incorrect playerId"). The wrong scope
  answers `ERROR_MISSING_PARAMETERS` naming the key it wanted.
- **Namespaces resolve by prefix and alias.** `home:1` answers as `homeTheater:1`, and
  `musicService:1` as `musicServiceAccounts:1`. A real namespace rejects an unknown *command*
  (`ERROR_UNSUPPORTED_COMMAND`, the canonical name echoed in the header). A nonsense one rejects the
  *namespace* (`ERROR_UNSUPPORTED_NAMESPACE`, namespace `global`). Read the echoed namespace before
  concluding one is missing. Walking prefixes found 41 namespaces. `positioning:1` is declared and
  has no implementation.
- **An empty body runs a parameterless action.** `togglePlayPause`, `pause`, `skipToNextTrack` and
  `loadHomeTheaterPlayback` all executed on `{}`. A command that needs a parameter refuses an empty
  body harmlessly. So probe with care, or with `--scope none`.
- **`x2rock raw`** is the probe. A refusal prints and exits 0, while an unreachable speaker exits
  non-zero. It prints the reply header, `--watch` attaches before sending (a `subscribe` reply can be
  overtaken by its event), and `--header KEY=VALUE` covers keys `--scope` cannot derive.

### Protocol facts the transport is built on

- **`cmdId` is echoed** in replies, so replies match callers out of order. A command the player
  cannot parse at all gets a reply with no `cmdId`, matched by arrival order.
- **Replies carry `success`; events never do.**
- **Subscribe, then listen.** The snapshot arrives as events immediately after the `subscribe`
  reply, one per namespace, so the event receiver has to be attached first. Subscriptions do not
  survive a reconnect and there is no replay: resubscribe and take the snapshot as truth.
- **`groups` events omit `playbackState`** and carry a `partial` flag. Only `getGroups` replies
  include playback state. The snapshot after `subscribe` repeats known topology; republishing MPRIS
  for it would flap every bus name.
- **An event body that omits a field does not set it to false.** `playback:1` sends `playbackError`
  bodies beside `playbackStatus`, told apart only by `_objectType`, and a `homeTheater:1`
  confirmation carries no options at all. Any type deserialized from both a reply and an event needs
  `Option` fields meaning "unchanged". All-false defaults once published a room as unable to play
  after every failed stream.
- **Liveness:** the player answers WebSocket Ping. The daemon pings every 30 s and treats 90 s of
  silence as a dead socket.
- **`track.imageUrl` is served by the player** (`http://<ip>:1400/getaa?…`) and works directly as
  `mpris:artUrl`. Its `u=` parameter carries the player's own playback URI, which is how the player's
  preferred enqueue scheme for a service can be read.
- **`playback:1 loadContainer` answers `success` and does nothing**, even for a garbage id. Like
  `loadStreamUrl` accepting an unplayable URL, a `success` here has to be checked by reading state
  back.

## The firewall problem

Omarchy's installer runs `ufw default deny incoming`, and every stock install blocks inbound. The
consequences are silent:

- **SSDP and mDNS discovery find nothing.** Replies come from the speaker's unicast address, do not
  match the multicast conntrack entry, and are dropped.
- **UPnP GENA never delivers.** `SUBSCRIBE` returns 200 with a valid SID, and then no event arrives.
- **The LAN WebSocket is unaffected**, because it needs no inbound connection.

Both failures were reproduced and fixed with one `ufw allow` from the speaker, which places the
cause on the host firewall, not the network.

**So x2rock is outbound-only, with two exceptions.** Both bind a fixed TCP port, 3401 by default, so
a host firewall needs one rule written once, and both are short-lived and opt-in:

- `link --from-household`, which has to receive the player's initial topology event;
- `say`, whose speaker fetches the generated clip from this machine.

Anything that ever relies on inbound traffic should detect the silent case (accepted, then nothing)
and name the host firewall.

## Discovery and network identity

1. **Steady state:** remembered players per network, connected outbound on :1443. No scan.
2. **Bootstrap:** `x2rock discover` runs a TCP connect sweep of port 1443 on the local subnet, a
   rolling window, every responder kept. A network wider than /24 sweeps only the local /24 and
   says so. One reachable player per household is enough: `groups:1 getGroups` returns every player's
   `websocketUrl`, `householdId` and capabilities.
3. **Never scan a network nobody chose.** An unrecognised network gets `unregistered_network` and no
   scan. The only automatic rescan is the one `connect` runs when a *known* network's remembered
   players all stop answering. On a first run the message says nothing has been discovered yet, and
   elsewhere that this is normal away from home. Neither message carries `discover` as a `fix`.
4. **`--ip` registers too.** `x2rock --ip <speaker> <command>` remembers that household for the
   current network, which is the way past a subnet the sweep cannot see (another VLAN, a guest
   network, a VPN).

**Network identity is the default gateway's MAC** (from `/proc/net/route`, then the neighbour
table). It is stable per site, where SSIDs and RFC 1918 subnets collide. A default route whose
gateway is `0.0.0.0` or that lacks `RTF_GATEWAY` (a dummy or VPN interface) is skipped.

**A household is a property of the players.** `householdId` is stable unless speakers move networks.
`forget_superseded_households` drops a remembered household only when *every* remembered address now
answers for a different id (a reset, a replacement, DHCP reuse). A room name picks its household. A
network with two households needs `--household` (or `X2ROCK_HOUSEHOLD`) only for a command that names
no room, such as the daemon, or for a room name both share. `x2rock households` always scans, so it
is offered rather than run, like `discover`.

Away from every household, commands that need a speaker fail at once with `unregistered_network`.
Bookmarks, the radio directory, `accounts`, `service status`, `desktop status` and `art` still work.
Offline, a household is resolved from remembered room names (`State::household_rooms`).

## Connection lifecycle and network mobility

Built for a laptop that moves between home, office and hotels, where no speaker is the normal state.

- **No player is a steady state, not an error.** The daemon withdraws its MPRIS names, logs once and
  retries with **capped exponential backoff**. A network change or a resume resets the backoff and
  retries at once (`wait_to_retry`).
- **Wake means assume dead.** A socket frozen across suspend accepts writes into a dead TCP session.
  On resume, reconnect from scratch. logind's `PrepareForSleep` and NetworkManager are on the
  *system* bus, so the daemon holds separate connections for them. Both are optional; without them
  the keepalive finds a dead socket a little later.
- **NetworkManager:** fire on the *transition into* connected (it climbs `CONNECTED_LOCAL`, `SITE`,
  `GLOBAL`), and watch `PrimaryConnection`, since docking or a VPN changes the route without changing
  the state. Losing the network is not a reason to act; only arrival is. A zbus `PropertyStream`
  opens by yielding its current value, so compare rather than count.
- **Re-resolve addresses on reconnect.** A cached IP is a hint.
- **Guest networks with client isolation** block the WebSocket entirely. Fail fast and quietly.
- **A firmware update** looks to the daemon like the speaker switched off for about 85 seconds.

**The daemon's log stays quiet.** `StatusLog` coalesces an unchanged state on a key that includes the
network fingerprint, re-logs once an hour with a count, and resets after a successful connect.
`X2ROCK_LOG_VERBOSE` restores every retry; `X2ROCK_LOG_EVENTS` logs every event body.

## Rule: talking to a service never enters the daemon

Losing a name lookup must never cost the household its transport or its volume. The daemon speaks
only to the LAN: the WebSocket on :1443, and UPnP and cover art on :1400. Every call to a music
service, the radio directory, a speech provider or an image CDN is a CLI command, and the widget runs
each as its own `Process`, so a failure is a line in one picker rather than a fault in the bar.
The check is structural: nothing reachable from `daemon.rs` or `mpris.rs` builds an internet request.

**One exception:** a household with Authentication on needs its Sonos sign-in refreshed about once a
day, and a daemon that could not refresh would lose control of that household. So the daemon makes
that one call to Sonos's cloud. A household with Authentication off never has a sign-in to refresh.

What the split implies for front ends: state reaches them over MPRIS from the daemon's events, and
anything MPRIS cannot express shells out to the CLI (relative volume, member volume, grouping, the
TV input, queue edits, ratings). The CLI may fail bluntly with an error code; the widget presents
the failure gracefully. Its pattern is a `Process` per kind of call, a non-zero exit shown as a
status line, empty stdout treated as failure rather than as an empty list, and existing rows kept
while a refresh runs.

## Volume

Sonos's own rules ([docs.sonos.com/docs/volume](https://docs.sonos.com/docs/volume)), followed:

- **Never send a volume command in response to a volume event.** That is the feedback loop.
- **About one command per 100 ms at most.**
- **Group volume for groups.** Per-player commands would destroy the relative levels a person set.
  x2rock fans out only when balance is the request: `vol --player`, `vol --each` and
  `vol normalize`.
- **`setVolume` for controls with known state (a slider), `setRelativeVolume` for stateless controls**
  (buttons, a scroll gesture, MPRIS volume steps, the TUI's keys). Never read-modify-write.

Measured on a One SL:

- **The player coalesces.** A command is applied in about 21 ms. A second command within about
  260 ms is deferred until that window closes, and `getVolume` returns the old value meanwhile. So
  the CLI reports the requested change, never a read straight after a write.
- **Both `setVolume` and `setRelativeVolume` unmute the group.** The volume bridge depends on this;
  re-checked on hardware 2026-10-13.
- **A group's volume is derived from its members'.** Rooms at 0 and 14 report 7, so the group slider
  cannot reach one room.
- **`playerVolume:1` is player-scoped**, and only that player answers it. `getVolume` returns
  `{volume, muted, fixed}` in both namespaces.
- **Mute is group-only in x2rock.** A single muted member is a puzzle to find later.
- **The volume limit** (the app's Volume Limit) is `settings:1 volumeScalingFactor`, 0.0-1.0. It is
  the one field `setPlayerSettings` accepts from this connection. It scales rather than clamps.
  `vol --limit` sets it per speaker.
- **An alarm sets a room's volume lastingly**, ramping to it over about ten seconds.

### The computer's volume keys, driving a room

`scripts/follow-sink.sh` keeps one room's volume and mute in step with the desktop's default sink.
It is a script, not a daemon feature, so the daemon does not supervise a child process or grow a
policy layer for something few people want. It waits on `pactl subscribe` (pipewire-pulse or
PulseAudio), re-reads the sink on each sink or server event, and sends `x2rock -r <room> vol N` only
when the mapped level changes; the sink's 100 % maps to a cap, 50 by default. It runs one way only:
the speaker never moves the computer.

Because a volume write unmutes, the bridge defers volume while the sink is muted and applies the
latest level on unmute. Only successful commands update its dedupe state, so a failed write is
retried on the next event. `python3 scripts/test-follow-sink.py` runs it against a fake `pactl` and a
fake CLI.

## Grouping over the local API

- **`modifyGroupMembers` is the primitive in both directions** (`playerIdsToAdd`,
  `playerIdsToRemove`). It keeps the group and what it plays, moves only the named players, and
  answers with the resulting group, whose member order differs from `getGroups`. An empty request is
  a no-op that still returns the group. `createGroup` builds a new group and picks the coordinator
  itself. `evictPlayers` is unsupported.
- **A group is named after its coordinator and renamed as it grows** ("Dining Room + 2"), so rooms
  resolve through players, never group names. `-r` with any member addresses the group; `--player`
  reaches one speaker inside it.
- **A room keeps its own queue across grouping**, and gets it back on leaving, at the first track
  and `IDLE`.
- **Removing the coordinator hands its queue to whoever stays, and their own queue is lost.** That
  is Sonos's behaviour and the app allows it, so it is documented rather than prevented. Scenes are
  built never to do it.
- **A soundbar on its TV input can stall a group change** for about 20 seconds. A change the
  coordinator does not answer in time is checked against a fresh `getGroups` and reported as done
  with a note when it landed (`household::change_group`).

### Bonded sets

A stereo pair is two `ZoneGroupMember`s sharing a `ChannelMapSet`, the second `Invisible="1"`. A
home-theatre bond is one member with `HTSatChanMapSet` and a `<Satellite>` per Sub or surround.
`rooms` hides both. The firmware keeps a bond in lockstep: volume and tone on the visible player
reach every member. Satellites do not answer `GetBass`. A room can mix firmware generations (a 97.1
Beam with 86.x satellites), and `update` reports only the coordinator, which is a known blind spot.

`zones:1` adds what the topology lacks: a `disconnected` flag per bonded member and its `gainTrimDB`,
shown by `system`.

### Scenes

`scene save/apply` (`src/commands/scene.rs`, `scenes.json` per household, rooms by player id) keeps
the groups, each room's volume, each group's mute and an optional soundtrack (a favorite or a
bookmark). Applying never removes a coordinator from its own group: `next_step` plans one
`modifyGroupMembers` at a time against a fresh `getGroups`, detaching a wanted coordinator first,
and changes only what differs. Mute is set after levels, because a level unmutes. Multi-room apply
has not yet run on real speakers.

## Soundbars and the TV input

- **`getGroups` capabilities say which players are soundbars:** `HT_PLAYBACK` and `HDMI`. A
  soundbar's HDMI belongs to the player, not the group, so "does this room have a TV" is
  `any(members)`.
- **`x2rock tv` hands the group to the TV over UPnP:** `SetAVTransportURI` to
  `x-sonos-htastream:<playerId>:spdif` (which covers ARC as well as optical), sent to the
  coordinator. The soundbar then takes over coordination, and for about 13 seconds neither player
  answers. The original request never returns, so the switch is waited out (14-20 s) and then
  reported. Sent to the soundbar directly, the same call makes the soundbar leave its group instead.
- **`homeTheater:1 loadHomeTheaterPlayback`** (player-scoped, no parameters) switches a soundbar to
  TV over the Control API, but for the bar alone. It is the fallback only when UPnP is off and the
  bar's group is just the bar. `playback:1 loadLineIn` is analog line-in and refuses a soundbar.
- **Leaving TV is any transport change**, even a failed `loadFavorite`. `pause` on TV input is
  `ERROR_PLAYBACK_NO_CONTENT`.
- **What the TV sends arrives as a push**, in `playbackMetadata:1` as `container.htInputFormat`:
  `numGroundChannels`, `numLFEChannels` (an explicit rename is needed, since camelCase would derive
  `numLfeChannels`), `numHeightChannels` and `streamDescription`. The channel counts matter more
  than the codec name: a degraded source keeps its name and drops channels. A dark TV, or an
  unplugged HDMI, reads `No Signal`. `GetPositionInfo` answers `NOT_IMPLEMENTED` on an HT stream.
- **Ask `x2rock:onTvInput`, never `inputFormat`.** It is the presence of `htInputFormat`; the format
  string can be empty while on TV.
- **`hdmi:1`** (player-scoped `subscribe`) reports `connection` (`NO_CONNECTION` when nothing is
  plugged in, which is what "TV Audio [Unavailable]" means), `tvPowerStatus`, and ARC/eARC/CEC state.
- **`homeTheater:1 setTvPowerState`** (`ON`/`STANDBY`) is fire-and-forget, unreadable, and was
  ignored by a TV with the Beam on its ARC port. It stays `raw`-only, so x2rock never reports a TV
  off that is still on.
- **Night sound and speech enhancement** read through `settings:1 getPlayerSettings` and write over
  UPnP `SetEQ`. Nothing pushes their changes: `homeTheater:1` and `settings:1` accept subscriptions
  and send nothing, and the app sees them through GENA. So clients re-read after a write.

### `DialogLevel`

On a first-generation Beam, `DialogLevel` is the speech-enhancement switch, written `0` or `1`, and
the app shows a plain toggle. Newer soundbars reportedly take levels 1-4 and move on/off to
`SpeechEnhanceEnabled`. That type answers `0` on a Beam while enhancement is on, so the split is not
operative there. The SCPD gives no range for either, so the player will not say what a level above 1
means. Untested: a write above 1 needs hardware not present.

## Tone, EQ and TruePlay

Writes are UPnP `RenderingControl:1` only; `setPlayerSettings` refuses them. Reads can come from
either side: `settings:1 getPlayerSettings` carries `eq` (bass, treble, loudness) and `homeTheater`
(night, dialog).

- **Bass and treble are -10 to 10.** Both are validated before either is sent, so a rejected treble
  never leaves an accepted bass behind.
- **`SetLoudness`/`GetLoudness` need `Channel=Master`**; omitted, the answer is UPnP 402.
- **Booleans are `1`/`0` on the wire.** Setters answer with an empty body, so the CLI reads back what
  the speaker holds.
- **Tone is per speaker, never per group.** `eq` resolves `--room` to the speaker; `--all` does not
  apply.
- **Loudness is on by default.** At low volume, turning it off changes what volume 1 sounds like.

**The extended set** (`GetEQ`/`SetEQ` with an `EQType`): `SurroundEnable`, `SurroundLevel`,
`MusicSurroundLevel`, `NightMode` and `DialogLevel` answer only on a soundbar (UPnP 402 elsewhere),
so they separate a soundbar from a non-soundbar exactly. `SubEnable`, `SubGain`, `SubCrossover`,
`SubPolarity` and `HeightChannelLevel` answer on every speaker, so their presence means nothing;
only `SubGain`'s value tracked a real Sub. Capability comes from `getGroups`, not from probing EQ.

**TruePlay** is `Get/SetRoomCalibrationStatus` (`Enabled`, `Available`). Disabling is a clean,
reversible toggle, and the stored curve survives it. There is no action to list or delete a curve.
Whether `Available` means "a curve is stored" or "the model supports one" cannot be told from the
LAN; it read 1 on every player, including a Sub. x2rock reads both booleans, disables, and refuses
to enable where nothing is available. The case for the disable: a speaker moved between rooms keeps
applying the curve measured for the room it left.

## The queue

- **Read:** `ContentDirectory Browse Q:0`, which also returns the queue's `UpdateID`.
- **Navigate:** `AVTransport Seek` with `Unit=TRACK_NR` (711 is out of range, 701 means the queue is
  not the current source). The CLI points the group at `x-rincon-queue:` first, since after a stream
  the queue is not current.
- **Mutate:** `AVTransport:1` carries everything: `AddURIToQueue`, `AddMultipleURIsToQueue`,
  `ReorderTracksInQueue`, `RemoveTrackFromQueue`, `RemoveTrackRangeFromQueue` (native, with
  `StartingIndex` + `NumberOfTracks`), `RemoveAllTracksFromQueue`, `SaveQueue`. The argument is
  `UpdateID`, not `UpdatedID`.
- **`UpdateID` is enforced.** A stale one answers 1028 and changes nothing. `queue --json` reports
  `version`, and `remove`/`move --at VERSION` quote the version the positions came from; without
  `--at`, a fresh read is used. `clear` takes no version. Faults: 800 bad position, 1028 stale
  version, 711 navigation, 402 for `NumberOfTracks: 0`.
- **Always send metadata.** An `AddURIToQueue` with empty `EnqueuedURIMetaData` is accepted and then
  blanks the titles of the whole queue.
- **What can be enqueued is decided by the kind of thing.** Tracks yes; stations and
  container-of-containers no (800 or 804). For a favorite, the discriminator is the `upnp:class`
  inside its `r:resMD`, since the favorite's own class is always `sonos-favorite`.
- **`AddURIToQueue` validates nothing about playability.** It accepts ids that never play, and some
  live stations it then refuses at play. An empty title on a queued row is the tell.
- **"Play next"** needs `DesiredFirstTrackNumberEnqueued` set to the current track + 1;
  `EnqueueAsNext` alone appends.
- **`AddURIToQueue` returns `FirstTrackNumberEnqueued` and `NumTracksAdded`.** Playing a container
  seeks to the first, and a refusal after an add removes the whole added range.
- **`playbackStatus.itemId` is the 1-based queue position** while the queue drives (`"0"` for an
  empty queue), and an opaque hash otherwise, so `queue_position` is `null` on a stream. It
  renumbers on edits, so it is a place, not a handle.
- **Queue changes:** the daemon publishes `x2rock:queueVersion` from the `Q:0` `UpdateID`, fetched
  off the event loop on each playback event (no polling). A pushed `playbackStatus.queueVersion`
  exists on recent firmware, but it does not move for edits to a queue that is not the room's source,
  so it cannot replace the `UpdateID`. A silent append to a room still playing something else is seen
  only on the next playback event.
- **Rows with no metadata** come from the Sonos app's "Play Now" menu; x2rock prints them as
  "(no title from the player)".
- **Saved playlists:** `Browse SQ:` (resources `file:///jffs/settings/savedqueues.rsq#N`).
  `playlists:1` uses a bare id (`"0"`) where UPnP uses `SQ:0`, and the two are not interchangeable.
  `loadPlaylist`'s `action` defaults to `APPEND`, so `x2rock playlist` sends `REPLACE`.
  `SaveQueue` takes `Title` and an `ObjectID` and returns `AssignedObjectID`; ids increment and are
  not reused.

## Favorites, history and bookmarks

- **`favorites:1 getFavorites`** is household-scoped and needs no login. Only `id` and `name` are
  reliable. **`loadFavorite`** is group-scoped, takes `favoriteId` in the body, replaces the queue,
  and resolves everything itself, account included. That makes a favorite the reliable way to play
  someone's personal content in a household with several accounts.
- **`FV:2` over UPnP** also lists shortcuts (`<r:type>shortcut</r:type>`, empty `<res>`), which
  `getFavorites` omits. Some are Sonos Radio navigation entries with nothing to play. But today's
  Sonos app also saves an album, artist or song favorite as a shortcut (x2rocktv, 2026-10-16):
  the item is only in `r:resMD`, an 8-hex prefix before the service's own id with `/` written
  `__UNENCODED_SLASH__`, and the service is the cdudn type over 256 (TIDAL's 44551 is 174). The
  account is not named. `loadFavorite` refuses these with `ERROR_INVALID_OBJECT_ID`; they play
  through the service path like any album. x2rock still drops every shortcut, so it hides these
  real favorites too. Tell them apart by the `upnp:class` inside `r:resMD`, and tell a shortcut
  from a favorite by the marker, not by the missing resource.
- **Current-track favorites can be created over UPnP** (x2rocktv, 2026-10-16). The earlier
  blanket prohibition was wrong: those 803 probes copied empty-resource shortcuts.
  `ContentDirectory CreateObject`, container `FV:2`, accepts an `object.item.sonos-favorite`
  wrapper with `r:type instantPlay`, the real `GetPositionInfo.TrackURI` as its resource and
  `TrackMetaData` escaped inside `r:resMD`. If the inner item lacks its catalogue id or cdudn,
  fill them from the current Control API track id (`00032020` + percent-encoded objectId) and
  advertised service type (`SA_RINCON<type>_X_#Svc<type>-0-Token`); preserve existing fields.
  Without that identity a save can succeed but remain untyped and hidden in the phone app.
  Proven with TIDAL at home (visible on the phone) and Saavn on the office One SL via the TV
  emulator; `x2rock favorites --json` lists the new Saavn entry as `TRACK`, playable. Playback
  was unchanged. This is a protocol finding, not a new Rust command; the CLI scope above
  still stands. Captures, implementation and regression tests are in sibling x2rocktv's
  `TrackFavorites.kt` and `docs/lan-transport.md` (T-31).
- **A favorite's `resource.id` carries the player's container prefix** (`1004206cALBUM:79488223`),
  which the service does not know. `browse --favorite` strips it from the favorite's own id, or
  retries without it after a refusal. All-digit ids look the same, so it is never stripped on sight.
- **`history:1 getHistory`** is the household's recently played list, with ids as
  `{serviceId, accountId, objectId}`. It is `x2rock recent`, `replay` and the widget's Recently
  played. With the app's Personalization Services off it answers `ERROR_DISALLOWED_BY_POLICY`, and
  nothing is recorded retroactively.
- **Bookmarks** (`bookmarks.json`): `keep` stores the playing item's `universalMusicObjectId`,
  readable from `playbackMetadata:1` with no credential, and `bookmark` rebuilds the enqueue URI and
  DIDL. The daemon also records every track with a real id as history (the last 50); `keep` pins an
  entry. The daemon's write never fails a room: errors are logged and swallowed. It writes once per
  track. `keep --container` is meaningful only while a real container plays. `keep` never blanks a
  stored field with an empty one. A bookmark depends on the household still holding an account for
  the service: the player validates the service at enqueue time (800 if none) and resolves the
  current registration, so a stored serial is provenance, not a pin.

## Alarms, the sleep timer and the household clock

**Sleep timer** (`AVTransport`, group-scoped):

- `ConfigureSleepTimer` takes `HH:MM:SS` only; seconds alone are 402. An empty string cancels.
- An unset timer reads as an empty element, not zero.
- It counts from acceptance, so the CLI reads it back.
- `00:00:00` lingers for about seven seconds while still playing, so `sleep_ms == 0` (about to stop)
  differs from `null` (no timer).
- It ends `PAUSED`, keeping the queue position.

**Alarms** (`AlarmClock:1`, household-wide; any player answers; rooms by `RoomUUID`):

- `ListAlarms` returns an escaped XML document, so reading it is two parses.
- `UpdateAlarm` needs all ten fields, so a toggle is read-modify-write; a partial record is a 402
  that changes nothing.
- The field is `StartTime` on read and `StartLocalTime` on write.
- `Recurrence` accepts `ON_<digits>` day bitmaps beyond the SCPD's four values; x2rock passes it
  through.
- `x-rincon-buzzer:0` is the built-in chime. Creating an alarm disabled is how to probe safely.
- An alarm created within about two minutes of its time misses its scheduling slot and fires late.
- `DestroyAlarm` does not stop one that is ringing; `pause` does.
- The duration runs from the scheduled time, and an alarm ends `IDLE`.
- A ringing alarm shows in `playbackMetadata:1` as a container named `x-rincon-buzzer:0` with
  `serviceId: "buzzer"`.

**The household clock** is what alarm times mean. `GetTimeZone` index `-1` means UTC, so
`alarms add` prints the household's own clock. `SetTimeZone(Index, AutoAdjustDst)` fixes it.
`GetTimeZoneRule` returns hex in which the first four digits are a signed offset in minutes,
POSIX-style (positive is *west*), followed by the DST rule. Indices ascend eastward (0 is UTC-12, 25
is UTC); index 9 is US Central with DST, and 13 and 14 are US Eastern. x2rock has no command for
this yet.

## Streams and `play-url`

### A stream URL needs no service

- **`playbackSession:1 createSession` + `loadStreamUrl`** plays a URL alongside the queue, leaving
  it untouched. That is how radio should behave. `createSession` is group-scoped and needs only
  `appId` and `appContext`. `loadStreamUrl` needs only `streamUrl`; `stationMetadata.name` is where
  the room's title comes from. A session outlives the CLI process. Pausing a live stream reports
  `IDLE` or `PAUSED` according to `canPause`, not according to liveness.
- **`SetAVTransportURI` with `x-rincon-mp3radio://<url>`** also plays a bare stream, but it
  *replaces* the room's source. A bare `http(s)://` URI there is refused (714/804), and the player
  rewrites the scheme to wrap the whole URL (`x-rincon-mp3radio://http://…`).
- **First audio takes several seconds** in `TRANSITIONING`, which must not be read as failure.
- **`loadStreamUrl` accepts what it cannot play and then sits `IDLE`.** So `play-url` and
  `stations --play` wait up to ten seconds for `PLAYING`, then fail with `stream_did_not_play` (or
  `stream_unverified`). The speaker fetches the URL itself, so it has to be reachable from the
  speaker. A bare URL omits `stationMetadata.service`; the default title is the host.
- **ICY now-playing text** reaches the Control API as `streamInfo`, which x2rock exposes as
  `stream_info` without parsing it ("Artist - Title" is a convention, not a format). It arrives a few
  seconds after playback starts.
- **Live versus file.** `loadStreamUrl` treats everything as a station: a finite file (a service's
  signed FLAC) sits at 0:00 whatever `type` says. So a URL is probed first with a one-byte range
  request. `shaped_like_file` calls it a file only when there are no `icy-*` headers, an
  `ETag`/`Last-Modified` is present, and the length is not Icecast's 1073741823 sentinel (Icecast
  honours ranges on live streams). A playlist `Content-Type` (`mpegurl`, `dash+xml`, `x-scpls`)
  stays a stream, as does anything unclear (non-2xx, a redirect, a timeout). A file goes to
  `SetAVTransportURI` with a `musicTrack` DIDL that declares its `audio/*` type (an extensionless URL
  is otherwise refused with 714). The cost: a file replaces the room's source.
- **`now --json` fills a file's duration** with one `GetPositionInfo`, only when the Control API gave
  none and the room is playing or paused. `status` does not, to keep its sweep cheap.

### `x2rock stations`: a directory that is not Sonos's

[Radio Browser](https://www.radio-browser.info) needs no key and no account. x2rock uses
`url_resolved`, never `url` (often a `.pls`), and renames it to `url` in its JSON. Results are sorted
by votes with `hidebroken=true`, and nothing is cached on disk. It sends `User-Agent: x2rock/<version>`,
as the operators ask; `http::AGENT` sends it on every request lacking one. The `hls` flag is reported
and not acted on: some HLS stations play and some do not. `all.api.radio-browser.info` round-robins,
so SRV lookup is not implemented.

## Music services

### SMAPI, and who is the client

SMAPI is SOAP 1.1 over HTTPS (`http://www.sonos.com/Services/1.1`). **Sonos is the client and the
music service is the server.** A controller that wants search does what a player does and calls the
service directly. The Control API has no content discovery at all. Its published spec (53 paths) and
the live one agree, and `musicServiceAccounts:1` offers only `match`.

The chain, no account involved:

```
MusicServices ListAvailableServices (LAN, unauthenticated)  -> endpoint, Policy Auth, manifest URI, service type
manifest + presentation map (anonymous CDN)                  -> endpoints, search categories, ratings
SMAPI search / getMetadata                                   -> items
SMAPI getMediaURI, or the player resolving an enqueued id    -> playback
```

**The credentials header:**

| field | when to send it |
|---|---|
| `deviceProvider` | always, `Sonos` |
| `loginToken` (`token`, `key`, `householdId`) | for a service that authenticates |
| `deviceCert` | never needed: the per-service "requires device certificate" flag gates nothing |
| `deviceId` | never needed; a player sends its `R_TrialZPSerial`, which is household-wide |

- **Send no XML declaration and no BOM.** A declaration produces a misleading
  `Expecting state 'Element'` fault, and incoming BOMs are stripped.
- **Faults are read from the body, not the status.** A pending link arrives as a fault at HTTP 200.
  SOAP 1.2 faults (`Code/Value`, `Subcode/Value`, `Reason/Text`) are read too.
- **Always send a `User-Agent` and decode gzip.** Deezer answers an empty 200 without a
  `User-Agent`, and gzips replies unasked, ignoring `Accept-Encoding: identity`. `http::gunzip`
  strips the RFC 1952 header for `miniz_oxide`.
- **A service's `count`/`total` can lie.** Parse what arrived and cut pages to the count asked for
  (`smapi::at_most`).
- **`X2ROCK_DUMP_SMAPI=1`** prints every request and reply with the whole credentials header
  replaced, not the fields inside it.
- **A service id is not a constant.** Spotify is 12 here and 9 in a European snapshot of the same
  endpoint. Ids are always read from the household's own descriptor list. `SecureUri` is preferred;
  no live service gives a different `Uri`.

### The catalogue, categories and what is searchable

- **`services.json`** caches the descriptor list. It is invalidated by `AvailableServiceListVersion`
  (the same number `musicServicesChanged` carries), never by a timeout, and carries a `SCHEMA`
  constant whose mismatch discards the file. Search works offline from the cache; only `--play`
  needs a player. A corrupt cache reads as empty, since it is regenerable. A service name resolves by
  exact match, then a unique prefix.
- **Policy `Auth`:** `Anonymous`, `DeviceLink` and `AppLink`. Sonos's cloud calls the same tiers
  `ANONYMOUS`, `OAUTH_WEB_ONLY` and `OAUTH`. A household catalogue holds about 108 services, roughly
  32 anonymous, 14 device-link and 62 app-link. The live answer is always bare `x2rock search` and
  bare `x2rock link`.
- **Reachable is not searchable.** `Catalogue::usable` (anonymous, or a token held) governs `browse`,
  `play-item` and `queue-item`. `Catalogue::searchable` also needs a published search category,
  learned and cached per service. Twelve anonymous services publish none and are browse-only. Asking
  one to search gives `no_search_categories`, with `browse` as the fix.
- **Categories** come from the presentation map's `PresentationMap type="Search"`. The id a person
  types maps to the id sent on the wire (TuneIn: `stations` -> `search:station`). Never split a
  mapped id on its colon. `<CustomCategory stringId mappedId>` takes `stringId` as its id (Hype
  Machine's Blogs). A category id repeated across `SearchCategories` blocks (Apple Music's and YouTube
  Music's library halves) takes its `mappedId` as its name (`librarysong`, `UPLOADED_SONGS`).
- **`all` is rare.** Universal Search (a category with id `all`) is what Sonos's mobile apps get
  through Sonos's cloud. Only 3 of 35 services here declare one. So x2rock does over Classic Search
  what Sonos does in its cloud.
- **`pick_categories`:** named categories in the caller's order, skipping any a service lacks; else
  a service's `default` categories (set only where x2rock fixes them, as for iHeartRadio); else
  `all`; else `tracks`, `artists`, `albums` where present; else the first. Several categories split
  `--count` and `--index` between them and interleave round-robin, dropping a repeated id.
  `--all-categories` asks for whatever a service publishes.
- **Merged search** (no `-s`) asks every searchable service at once. Categories are warmed
  concurrently and remembered afterwards; a failed warm is not recorded. Linked catalogues sort
  before stations, and stations before blog aggregators (Hype Machine indexes blog posts, carries
  tracks only, and its titles are unreliable). The sort is stable within a service. Timeout 12 s,
  with a slow service named on stderr. Five rows per service by default, three per service under a
  heading. `--only-linked` skips the long tail. A failure is per service and category, and partial
  results are normal. The envelope adds `asked`, `searches`, `answered`, `slow` and `refused`.
  Refreshed tokens are saved one at a time after the join.
- **`container` decides playability, never `canPlay`.** An item that arrived as a `mediaCollection`
  is a container, except that a declared `track` or `stream` inside one is a leaf (Plex wraps tracks
  that way). `Item::opens()` is a container that is not a `program`, since a station plays rather than
  opens. That one rule decides both `--play` gates, the trailing slash in `browse` and the JSON
  `container` field. A row that says `canPlay false` *and* `canEnumerate false` (Amazon's
  "Content Unavailable" sentinel) is dropped.

### Browse, and the front page

`getMetadata` and `search` return the same payload, so one parser serves both. Many services' app
front pages do not come from SMAPI's `root`. A service's manifest can name a `browse` endpoint, and
`frontpage.rs` reads it:

- **`root` becomes** the page's shelves in its order, then SMAPI root entries the page lacks
  (deduplicated by id and by title).
- **SMAPI's root is read first**, because that call refreshes a stale token.
- **A bearer refused with 401 or 403 is retried bare.** Sonos Radio's page needs no credential and
  refuses one.
- **A view's id is an ordinary SMAPI container id.** A shelf `getMetadata` refuses is answered from
  the page's inline items.

Spotify, Deezer, TuneIn (New), SiriusXM and Sonos Radio match the Sonos app shelf for shelf, and
services with no endpoint match because SMAPI root is the page. Amazon's home endpoint refuses a
bearer (401) and Apple's answers 500. The Sonos app shows full pages for Amazon Music, Apple Music
and YouTube Music only through Sonos's cloud; see the terms note. x2rock's path to that service
(`src/sonos/cloud.rs`) is inactive under current license requirements: `cloud::ACTIVE` is false, so
it makes no request.

**A curated shelf is not the addressable set.** Saavn's Top Shows lists 92 shows, yet any show id
from the service's own website browses. "Not in the browse tree" does not mean unreachable.

### Linking: how a token is obtained

1. `getDeviceLinkCode` (device-link) or `getAppLink` (app-link, whose reply nests the same
   `regUrl`/`linkCode`) returns a URL and a code. `showLinkCode` says whether the person must type
   the code, so it predicts how much work they are about to do.
2. The browser opens on Linux with `xdg-open`, or `--no-open` prints the URL over ssh.
3. `getDeviceAuthToken` is polled about every 3 s for up to seven minutes. Pending is the fault
   `NOT_LINKED_RETRY` (SonosError 5); 6 is fatal. A transport error during polling is evidence about
   the poll, not the link: it prints `?` and keeps going. A service refusal stops at once
   (`link_refused`).
4. Success returns `authToken`, `privateKey` (sometimes junk or empty; accepted) and sometimes
   `userIdHashCode`.

`PollInterval` in a descriptor is not a link rate limit. Every link service declares one (30 to
3600), and honouring it would break every link; it is how often a player re-asks for content
changes. `linkDeviceId` is minted per attempt by the service, so x2rock echoes what the reply
carried.

Flows seen: a code in the URL (Bandcamp, TuneIn), a typed code (iHeartRadio, Saavn), OAuth
authorize with the code in `redirect_uri` or `state` (Mixcloud, TIDAL, TuneIn (New)), and Plex's own
PIN flow. **Refusal shapes:**

- by caller (YouTube Music);
- by subscription tier (Saavn without Pro, which the Sonos app hits too);
- a working login page whose token exchange always answers `NOT_LINKED_FAILURE` (Qobuz; Sonos's
  cloud completes app-link and the service never implemented the controller poll);
- stubbed link methods (Classical Archives);
- refused outright (Apple Music, SoundCloud, SiriusXM).

A refusal at `getAppLink` rules out x2rock's own flow, not the service: the household import below
reaches several of these.

**`match` associates and never creates.** `musicServiceAccounts:1 match` (`nickname`, `serviceId`,
`userIdHashCode`) returns the household's serial for an account the Sonos app already registered,
and refuses otherwise ("Link code required to add guest account", or a bare
`ERROR_COMMAND_FAILED`). The order that fills `account_id` is the Sonos app first, then
`x2rock link`. It is provenance only: playback never needs it.

**Credentials** (`$XDG_STATE_HOME/x2rock/credentials.json`, mode 600, keyed household -> service ->
account):

- Not a keyring: a locked or absent keyring would sit between a person and their music, over ssh or
  in a widget subprocess.
- The mode is set at creation, and a loose file is tightened on read.
- A corrupt file is an error, not an empty store.
- Several accounts per service are kept, one preferred (`accounts --prefer`); `token_for` resolves
  the preference.
- An account key is the household serial, else `userIdHashCode`, else `link`; a write whose token
  already exists lands on that record.
- Every read resolves the current household first (the reached player's, or the only one when
  offline).
- `unlink` forgets a service from every household; `accounts` groups by household when there are
  several.
- Schema 2 migrates; schema 1 and newer schemas are refused rather than read as empty.
- The token is saved before `match` runs, since a link code is single-use.
- Tokens are never stored on the speaker (`SystemProperties SetString`), where every LAN client
  could read them.

### Household registration is what playback needs

**A local token buys search and browse, and nothing of playback.** On-demand content is enqueued, and
the *player* resolves it with the household's own registration for that service, made in the Sonos
app. Measured both ways:

- Deezer and TIDAL linked first: search works, enqueue answers 800.
- The same services added in the app, with the token unchanged: queued and playing.

What follows from that:

- **Enqueue validates the service at insertion.** With no registration it answers 800. Entries
  already queued stay after an account dies. A stream with no registration plays only if the service
  implements `getMediaURI` and the item is a stream.
- **Registration is per account.** A household can hold several accounts for one service, and they
  coexist.
- **Serials are `highest live serial + 1`.** They are unique only among live accounts, fall when the
  top account is removed, and mean nothing across time. Re-adding the same subscription mints a new
  one.
- **No verb lists a household's accounts**, on any Control API namespace, UPnP service or
  `/status` page (`/status/accounts` and every unknown `/status/*` name answer an empty
  `ZPSupportInfo`). `registeredServicesVersion` on the `musicServicesChanged` event moves when the
  account set changes. Harvesting `sid`/`sn` from `FV:2` and `Q:0` is neither a lower nor an upper
  bound (it keeps dead serials and misses stations); the household import reads the real set.
- **Preferred Service** (the app's setting) is not stored on the household, and x2rock cannot read
  it.

### Which account an enqueue plays from

The enqueue URI's `sn=` picks the account. The cdudn's selector
(`SA_RINCON<type>_X_#Svc<type>-<selector>-Token`) does not, but on the queue path the queued row's
`sn` is rewritten from the selector, so a mismatched pair leaves the row disagreeing with what plays.
The Sonos app always sends a matched pair, and so does x2rock (`content::Naming`, carried on the
`Token` by `Account::token`, so the account asked and the account named come from one lookup). A
missing or unknown `sn=` falls back to the household's default account, which is not sticky. Set
Primary in the app governs the app's own playback, not the player's default. The account in use is
`accountId` from `playbackMetadata:1 getMetadataStatus`, at group scope.

### Household token import (`link --from-household`)

Each player keeps every account's SMAPI credentials and puts them in a variable named
`ThirdPartyMediaServersX`. It appears only in the **initial** `ZoneGroupTopology` event sent to a new
subscriber. It is not in `GetZoneGroupState`, and `QueryStateVariable` answers 401. So reading it
needs the player to connect back (TCP 3401, `--callback-port`, `0` for ephemeral). The envelope
(`2:` prefix) is keyed to the household id. The method is SoCo's, published in
[SoCo PR #1010](https://github.com/SoCo/SoCo/pull/1010), and holds across firmware generations.
x2rock's implementation is `src/sonos/stored.rs`: `decrypt_accounts` is pure and tested with
synthetic vectors, and `capture_envelope` does the network part.

The plaintext has one record per account:

- `Token0`, `Key0`, `Nickname0`, `Tier0`, `SerialNum0`, `Md0`;
- `Username0`, the account selector `X_#Svc<type>-<key>-Token`, which can be an email address
  elsewhere, so it falls back to the whole value;
- `Flags0`: `0` if the account was the service's first when registered, `4` if it was added beside
  another; the form is fixed at registration;
- `NumAccounts`: always 1 so far, with a second account arriving as a second record.

It is Sonos's legacy `/status/accounts` field set, flattened into attributes. The selector also
appears in plaintext in favorites' cdudns and is not secret; the household id is.

What it opened:

- Qobuz, SiriusXM and SoundCloud, whose link flows refuse, search, browse and play with the imported
  token.
- YouTube Music's imported token still gets 403 (the block is the caller, not the account).
- Apple Music's token has an empty key and is refused (`InvalidTokenException`).
- Bandcamp's household token is refused from x2rock while working for the speakers, so x2rock's own
  link token stays preferred for Bandcamp.

The import has no `match` step, and `--dry-run` shows the records without keeping any. x2rock never
*writes* household accounts.

### Playback routes

`play-item` (and `search --play`, `browse --play`, `bookmark`, `replay`) picks a route by kind:

1. **`stream`** goes through the session (`loadStreamUrl`), or to the transport if the URL is a file.
2. **`program`** (a station, an artist radio) becomes the room's source: `x-sonosapi-radio:` with an
   `audioBroadcast` DIDL. The queue is never tried. A program that refuses the radio route (SiriusXM
   live channels) falls back to the stream.
3. **`audiobook`** resumes: one `getMetadata` (`smapi::resume_point`) gives
   `positionInformation` (the chapter and a chapter-relative `offsetMillis`). The chapter plays as a
   track, then `playback:1 seek`. The speaker reports the position back to the service itself.
   `--from-start` starts over. `queue-item` on a book adds every chapter.
4. **Anything else** is enqueued with the service's cdudn, and a refusal falls back to the stream.
   A refusal is the whole sequence (add, point the group at the queue, seek, play): a row accepted
   and then refused at play is taken back out (`not_queue_material`). The fallback runs only in that
   direction, since a stream's failure is silent.
5. **`loadContent`** is the last resort, when the household holds an account; see below.

**Enqueue URIs follow the player's own scheme for the service.** The default is
`x-sonosapi-hls-static:<percent-encoded id>?sid=<sid>&flags=65544&sn=<serial>`. Spotify needs
`x-sonos-spotify:` (`native_scheme`); without it the row queued and then answered 711 on seek. The
object id is percent-encoded in lowercase hex, in both the URI and the DIDL `id` (a raw colon broke
Mixcloud). `flags` is not load-bearing. The player rewrites what it is given into its own form
(`x-sonos-http:<id>.flac`, `.mp3`, `.mpd`).

**Containers:** an album or playlist enqueues as
`x-rincon-cpcontainer:<prefix><id>?sid&flags&sn` with a container DIDL, and the player expands it
into tracks. The 8-hex prefix (`10` + class + flags in hex) varies by service. An artist (a container
of containers) is refused with a sentence, not a UPnP code (804). A service container cannot be
*started* by URI with `SetAVTransportURI` (714), only enqueued, or played as a favorite.

**The synthesized DIDL** names the account through the cdudn
`SA_RINCON<type>_X_#Svc<type>-<selector>-Token`, where `type` is the service type from
`AvailableServiceTypeList` (`sid*256+7`), and the trailing `Token` is a placeholder the player
resolves against its own credential. That is why x2rock enqueues content without ever holding the
service's token.

**`playback:1 loadContent`** (group-scoped) takes `{serviceId, accountId, objectId}` and a `type`, and
the player resolves the rest:

- It loads and does not start (`playOnCompletion` is ignored). `load_and_start` pauses, loads, then
  presses play until the content changes or the container is the item named.
- It cannot append.
- It refuses anonymous services and accounts the household does not hold.
- It accepts a dead account and plays nothing.
- **It restarted a speaker on Mixcloud**, so `replay` goes through `play_item`'s routes instead. Only
  a Sonos playlist (`65435`) still loads, and Mixcloud never reaches `loadContent`.
- `replay` keeps a history item's own account unless the household no longer holds it, because a
  listener's custom station plays only as that listener.

### Token refresh, and re-reading the household

- **In-band refresh.** A fault carrying `refreshAuthTokenResult` (for example
  `Client.TokenRefreshRequired`) is a replacement token. `smapi::call` retries once with it and
  saves it through `Credentials::update` to the record it refreshed (household and account ride on
  `smapi::Token`). The original `linked` timestamp is kept.
- **Household re-read** (`rereads.rs`). When a service refuses a token that came from the household
  and offers none, x2rock re-reads the household's store, at most once per household every
  10 minutes per process, serialized. If the store holds a different token for the same account, it
  saves that and retries. This applies to imported accounts only, and to any refusal, since a garbled
  token's refusal can be a service-side `TypeError`.

### Per-service notes

The table below is the measured state; these are the facts that do not fit a cell.

- **YouTube Music.** Search is closed. Its SMAPI endpoint is Google's "YouTube Music API (Partner)",
  method `google.music.sonos.v1.Sonos.SendRequest`, enabled only for Sonos's own projects. A
  self-service key or OAuth token is judged against its own project and gets `SERVICE_DISABLED`; read
  `details[].reason`, not the HTTP status. The manifest's `apiKey` is encrypted to Sonos's apps and
  players and is not presented. Its object ids are opaque partner ids, so the public YouTube Data API
  cannot bridge. Playback through the household registration works (`keep`, favorites, bookmarks).
  The same is true for x2rocktv.
- **Apple Music: playback yes, search no.** `getAppLink` is refused, and the household's token has an
  empty key that never worked. Search goes through Apple's public iTunes Search API
  (`src/itunes.rs`): `song:<trackId>` and `album:<collectionId>` are exactly the ids the player
  plays. It covers tracks and albums only, with no library or playlists. The storefront is the
  locale's region, else US. Offset is ignored, so a page is sliced from one answer. Playback needs the
  household import, for the service record (`find_usable`).
- **Plex.** Its SMAPI link methods are dead. Its SMAPI endpoint accepts any Plex account token
  (empty key). `link plex` runs Plex's own PIN flow (`plex.tv/api/v2/pins`, client id
  `x2rock-<hostname>`, revocable at plex.tv). Without Remote Access, that token searches but cannot
  browse `root`. The household's token (import, or `--from-player`, read from an art URL's
  `X-Plex-Token`) browses everything and dies on relink, so it is preferred. Plex on Sonos breaks if
  the Plex server's host runs a VPN: `publicAddressMatches: false` in `plex.tv/api/v2/resources` is
  the tell.
- **iHeartRadio.** It is device-link. Search has three sections, as the app shows them: Stations
  (sent as `artists`), Live (sent as `stations`) and Podcasts (`iheart.rs`). `artist_radio_track`
  ids are per-listener and stream-only. Podcasts queue across accounts.
- **Spotify.** Its enqueue scheme is `x-sonos-spotify:`. `getMediaURI` returns `x-spotify://`, which
  no stream path can play.
- **Amazon Music.** On a paid tier an album is a real tracklist. Its stream fallback is a presigned
  HLS URL that plays without a registration. It ignores `count`.
- **Deezer.** Ratings are a heart and a ban (see *Ratings*). It needs a `User-Agent` and gzip.
- **Saavn.** Without Pro, everything is refused, the Sonos app included. Podcasts are browse-only
  (`SHOWS:` -> `SEASONS:` -> `EPISODE:`). Radio ids embed JSON with `**` for its colons.
  Non-Latin (Devanagari) metadata survives query, DIDL, queue and playback.
- **Qobuz.** It can be linked only by import. A Sonos Favorite of a Qobuz track fails in the app,
  because queued Qobuz rows carry no cdudn.
- **Bandcamp.** A token opens the account's own collection, not a catalogue.
- **Audible.** Root is the seven marketplaces; resume is above.
- **Pandora.** Every hit is a station (`canEnumerate false`) and plays as radio. Playing a search hit
  adds it to My Stations.
- **SiriusXM.** Live channels are streams; artist and Xtra channels are programs.
- **SoundCloud** ignores `index`, so its second page repeats the first.
- **Radio Paradise** implements no `getMediaURI`, so it needs a registration to play.
- **Classical Archives**' link methods are stubs. It is not tested (decided 2026-10-11).
- **Pandora CloudCover** is out of scope.

### Music services: what has actually been tested

"Playback" depends on the household having added the service in the Sonos app; see *Household
registration is what playback needs*. Rows describe the direct path. Dates are the last check.

| service | tier | how x2rock gets a token | search / browse | playback | verified |
|---|---|---|---|---|---|
| Deezer | device-link | link | ✓ | enqueue ✓; file fallback ✓ | 2026-10-11, home and office |
| TIDAL | device-link (OAuth) | link | ✓ | enqueue ✓; file fallback ✓ | 2026-09-19, home |
| iHeartRadio | device-link | link | ✓ (three sections) | stations stream; podcasts enqueue | 2026-10-12, office |
| Spotify | app-link | link, or import | ✓; root is the app's page | enqueue ✓ (`x-sonos-spotify`) | 2026-10-11, home |
| Amazon Music | app-link | link, or import | ✓ (paid tier); root is Library and Playlists only | enqueue ✓; HLS fallback ✓ | 2026-10-11, home |
| YouTube Music | app-link | import only; still 403 | ✗ (Google's partner gate) | ✓ for known ids | 2026-10-11, home |
| Apple Music | app-link | import (unusable for search) | catalogue search via iTunes | ✓ any catalogue id | 2026-10-11, home |
| Qobuz | app-link | import only | ✓ | ✓ | 2026-10-11, home |
| SiriusXM | app-link | import only | ✓ | live channels stream; stations as radio | 2026-10-12, office |
| SoundCloud | app-link | import only | ✓ | enqueue ✓ | 2026-10-11, home |
| Pandora | app-link | link, or import | ✓ (stations) | as radio | 2026-10-12, home |
| TuneIn (New) | app-link | link | ✓; root is the app's page | streams | 2026-10-11, home |
| Radio Paradise | app-link | link (anonymous login) | browse only | as radio, with a registration | 2026-09-30, office |
| Sonos Radio | device-link | import only (link handlers crash) | ✓; root from its `browse/v1` | as radio | 2026-10-11, home |
| Saavn | device-link | link (Pro required) | ✓ | enqueue ✓ | 2026-10-11, home |
| Plex | own PIN flow | PIN, or import | ✓ (root needs the household token) | enqueue ✓; Shuffle All as radio | 2026-10-12, home |
| Bandcamp | device-link | link (household token refused) | ✓ (own collection) | enqueue ✓ | 2026-10-12, home |
| Mixcloud | device-link | link | ✓ | enqueue ✓ (never `loadContent`) | 2026-10-11, home |
| Audible | app-link | import | ✓ | resumes | 2026-10-06, home |
| Classical Archives | device-link | ✗ (link stubbed) | - | - | won't test |
| Pandora CloudCover | app-link | - | - | - | out of scope |

All 24 services searchable on the office household on 2026-09-18 were played, each verified by the
playing position advancing.

### Ratings

A service declares ratings in its presentation map (`NowPlayingRatings`), keyed on a state such as
`ISFAVORITE`, and `rate` sends SMAPI `rateItem`. `smapi::RatingKind` reads a thumb or a heart as up,
a ban as down, and Deezer's `DELETE_TRACK` as `unfavorite`. `rate` distinguishes "no state reported"
from "this state does not offer that". A dislike may auto-skip (`AutoSkip="ALWAYS"`), and nothing
undoes it, so the TUI asks before sending one. `x2rock:hasTrackId` gates rating in front ends.
Research record: [openphonos-ratings-findings.md](openphonos-ratings-findings.md).

## Authentication on, and signing in

With Authentication on:

- Every Control API command answers `ERROR_NO_PERMISSION`, while UPnP keeps answering.
- Sockets opened before the switch keep working.
- Discovery stays open: REST `GET /api/v1/players/local/info` reports `credentialTypeAllowed:
  GUEST_TOKEN` instead of `API_KEY`.
- `attach` maps the refusal to `authentication_required` at once, with no rescan.

The credential is a Sonos OAuth 2 bearer token from the person's **own** developer integration;
x2rock ships no key or secret. That is a confidential-client flow: the secret is needed on the code
exchange and on every refresh, through `api.sonos.com/login/v3/oauth` and `/oauth/access`, with scope
`playback-control-all`, a 24-hour access token and a refresh token. On the WebSocket the token goes
in **each command's header** as `"authorization": "Bearer <token>"`, not in the handshake. On REST
it is the usual `Authorization` header.

`x2rock login`: Sonos redirects only to a registered public HTTPS page, so the page
(`rahga.github.io/x2rock/callback.html`, CSP `default-src 'none'`) forwards the browser to a
`127.0.0.1` port that x2rock carries at the front of the OAuth `state`. It also shows the address to
paste when the browser is on another machine (`x2rock login '<address>'`).

The token file is mode 600 (`src/sonos/login.rs`):

- Every command reads it from disk, so a running daemon sees login, logout and replacement.
- It is refreshed ten minutes before expiry; one refresh runs per process at a time, with a
  60-second cooldown after a failure.
- A refresh is saved only if the token it refreshed is still current, so a late reply cannot
  restore a logged-out sign-in.

## The Connection Security switches

The app's Account > Privacy and Security > Connection Security switches are readable on the Control
API through `effectiveSettings:1 getSettingsGroup {"groupName": "security"}`, player-scoped with no
other parameter:

- `allowUnauthenticatedControl`: Authentication off;
- `allowInsecureUPnP`: UPnP on;
- `allowGuestAccess`: Guest Access.

`x2rock security` shows all three. A player-scoped `subscribe` sends `settingsChanged` with a
timestamp per settings group whenever one changes.

**UPnP off:** every SOAP call answers 403 (queue, alarms, EQ writes, sleep timer, TV handoff), while
the Control API and cover art on :1400 carry on. The daemon reads the switch, re-reads it when the
`security` timestamp moves, and publishes `x2rock:upnpOff`. False means on or unknown, so an older
daemon leaves clients offering everything. The widget and TUI then withdraw what would be refused,
and the daemon stops its queue-version browse. `play-item` keeps the routes that do not need UPnP,
and `tv` falls back to `loadHomeTheaterPlayback` for a lone soundbar.

## The daemon and MPRIS

`x2rock daemon` publishes one MPRIS2 player per **group**, `org.mpris.MediaPlayer2.x2rock-<room>`,
named for its coordinator. Party mode collapses a household to one player. It holds one WebSocket per
coordinator plus best-effort member sockets for per-room volume. Losing a member socket freezes one
slider (`Loss::Tolerated`); losing a coordinator reconnects.

MPRIS has no word for a group, a member's volume, a TV input or a live stream, so the daemon adds
`x2rock:` keys to `Metadata`:

- `members`, `memberVolumes`, `muted`, `volumeLevel`, `fixedVolume`;
- `canRepeat`, `canShuffle`, `canCrossfade`;
- `noSource`;
- `hasTvInput`, `onTvInput`, `inputFormat`;
- `isLiveStream`, `stationName`, `streamInfo`;
- `hasTrackId`, `queueVersion`, `upnpOff`, `nightMode`, `enhanceDialog`.

The README lists them all.

- **Live stream** means `container.type == "station"`, the player's own vocabulary. It is not a
  missing duration, `objectId "-1"` or a missing `currentItem`; those are TuneIn's shape.
- **Live and pausable are different properties.** TuneIn has `canPause: false` and resumes at the
  live edge. Sonos Radio pauses, then resumes the same track from its start. Transport controls gate
  on `canPause`.
- **`stationName`** is set only when the stream is live and the station name is not already the
  title.
- **Title and art** come from `track` before `container`, so Sonos Radio's per-track art updates on
  its own.
- **A podcast episode** has no artist or album; `album` falls back to the show (`track.podcast.name`).
- **The service id** is read from the art URL's `sid` first. The metadata object's `serviceId` is
  wrong for HLS content.

**The daemon on `Pool`.** `serve` owns a `Session`. Sockets beyond the primary come from
`Pool::reach`, which evicts a socket its keepalive gave up on. `session.groups` follows every
`groups` event. A republish closes the pool.

**Daemon writes never touch a room.** Recording history swallows its own errors. The queue-version
fetch runs off the event loop, one per group, so one wedged coordinator cannot stall every room.
`panic = "abort"`: a panic restarts the unit clean rather than continuing on half-mutated state, so
`lock().unwrap()` is correct.

## The agent contract

The CLI is an API, and an agent is as likely a caller as a person.

- **`status --json`** is the one-call snapshot: every group through its own coordinator, all at
  once. An unreachable coordinator tags its own room with `error`; the rest still answer. `--full`
  wraps it in `{household, network, total, reachable, warnings, rooms}`.
- **Errors are data.** A `Hint` (`src/hint.rs`) carries a stable `code`, an optional runnable `fix`
  and optional `data`. With `--json`, a failure prints `{error, code, fix, …data}` on stderr and
  exits non-zero. A plain error is `code: "unknown"`. `unknown_room` carries `rooms` and
  `did_you_mean`. `no_player` keeps a sharper connection-layer code from underneath.
- **A `fix` must be safe and local, and must run.** It never scans a network, so neither network
  error carries `discover`. `hint::shell_arg` quotes names with spaces or quotes, which is tested by
  running the emitted fix, not by matching strings.
- **Outcomes, not printing.** A command returns a typed outcome implementing `commands::Report`
  (`Serialize` plus `text()` and `notes()`). `emit()` prints text or one JSON line on stdout, and
  notes on stderr. JSON goes through `serde_json::to_value`, so keys print sorted.
- **Fan-out.** `-r` repeats and `--all` applies to per-room commands (volume, transport, repeat,
  shuffle, crossfade). One connection; rooms report as they finish; it stops at the first failure.
  Several `-r` on a command that does not fan out is `too_many_rooms`.
- **Fields that save a parse:** `on_tv`, `audible`, `service_id`, `queue_position`,
  `favorites --json`'s `playable` and `category`. Two favorites sharing a name are listed by id
  rather than picked.
- **The skill ships in the binary** (`include_str!`). `x2rock skill` installs it, and its error table
  is tested against the code in both directions.

## Implementation

### Layout

One crate:

- `main.rs` and `cli.rs`: the clap tree.
- `commands/`: one file per command family; nothing there parses arguments.
- `session.rs`: connect, the `Pool`, progress lines.
- `discover.rs`, `netid.rs`, `state.rs`, `store.rs`.
- `daemon.rs`, `mpris.rs`, `restart.rs`: the daemon, its MPRIS server, logind and NetworkManager.
- `hint.rs`.
- Service-side modules: `catalogue.rs`, `credentials.rs`, `bookmarks.rs`, `streams.rs`,
  `stations.rs`, `itunes.rs`, `iheart.rs`, `frontpage.rs`, `rereads.rs`, `art.rs`, `speech.rs`,
  `clipserve.rs`.
- `tui/`.
- `sonos/`: `local.rs` (the WebSocket transport), `api.rs`, `proto.rs`, `upnp.rs`, `http.rs`,
  `smapi.rs`, `plex.rs`, `stored.rs`, `login.rs`, `cloud.rs`.

The one boundary designed up front is the transport (`local.rs` against `api.rs`). Everything else
was split when a file became unwieldy. `main.rs` became `cli.rs` and `commands/` as a pure move,
verified byte-identical against captured output. `scripts/output-baseline.sh` captures 33 commands
for that kind of diff; it changes rooms and restores most of their state.

### Dependencies

- **The Sonos client is written from scratch.** Every Sonos crate surveyed speaks UPnP only.
- **`mpris-server`** (on `zbus`), because the daemon exposes an interface rather than consuming one.
- **One HTTP client**, `sonos/http.rs`: plain HTTP to a player, TLS to a service. Timeouts belong to
  the caller (8 s to a player, 6 s to a service). It reads to EOF, and `dechunk` uses checked
  arithmetic.
- **`aes`, `cbc` and `md-5`** for the household envelope only; `ring` offers neither cipher. base64
  is a dozen hand-rolled lines.

### `ring` over `aws-lc-rs`, deliberately

rustls defaults to `aws-lc-rs`. Swapping to `ring` took the stripped binary from 9.3 MB to 7.2 MB
and removed the cmake step, with the same cipher suites. The swap spans three places that have to
move together: `default-features = false` on `rustls` and `tokio-rustls`, and
`sonos::crypto_provider()`, the one site that names a backend. `builder_with_provider` is used
everywhere, so a second provider linked in later cannot panic at the first handshake.

Two things were given up. One is post-quantum key exchange: nothing x2rock sends is worth
harvesting, and this should be revisited if it ever carries a durable secret. The other is P-521:
a P-521 certificate would fail the handshake with an opaque alert, before `AcceptAnyCert` is
consulted.

### State files and concurrency

All of these live under `$XDG_STATE_HOME/x2rock/`; the secret ones are mode 600:

- `networks.json`: players per network;
- `services.json`: the catalogue cache;
- `credentials.json`;
- `bookmarks.json`;
- `scenes.json`;
- `speech.json`;
- `streams.json`;
- the Sonos sign-in.

The art and speech caches live under `$XDG_CACHE_HOME/x2rock/`.

- **Every write is atomic, and owns its scratch file.** It writes a PID-and-counter name with
  `create_new` at the final mode, renames it into place, and removes the scratch on any failure.
  Atomic replacement does not merge competing updates.
- **Read-modify-write takes `store::Lock`**, an `flock` on a sibling `<file>.lock`, since the rename
  replaces the data file's inode. Bookmarks, credentials, scenes and the sign-in use it. The player
  list does not: its writers write what a player just said.
- **User data migrates; caches are discarded.** The bookmark `pinned` flag defaults to true; a
  catalogue with the wrong `SCHEMA` is refetched.

### The CLI, the TUI and the pool

Most of a CLI call's ~420 ms is opening sockets, not process start (3 ms).

- **`Session` holds a `Pool`** of connections beyond its primary, keyed by address. The lock is
  never held across an open; the loser of a race closes its socket. Dead sockets are evicted.
  `Session::close` closes everything, since `Connection` has no `Drop`. A split socket closes only
  when both halves drop, so the read loop drops the sink on its way out.
- **The TUI runs commands in-process** (`tui::action::Speakers`). It keeps a session from its first
  write and re-reads the topology before every write. The connect is a spawned single-flight task.
  The held session carries a generation, so a failure releases only its own. Only an unreachable or
  dead socket drops the session. A failed write re-asks the daemon for the household, so a bar that
  moved on the keypress snaps back. The cursor follows the room a key chose, through republishes.
  Writes take about 180 ms once the session is open.
- **Doing and printing, separated.** Because commands return outcomes, the TUI and a future D-Bus
  interface ([dbus-interface.md](dbus-interface.md), a proposal) can call them without parsing
  output.

### Cover art

`x2rock art <url>…` (`src/art.rs`) fetches covers for the widget, so QML never fetches an arbitrary
URL:

- only `https://`, or `http://` to a private IPv4 address on port 1400;
- at most three redirects, each held to the same rule;
- at most 2 MB in 8 s, and any `Content-Encoding` refused;
- the body must start like an image;
- cached in `$XDG_CACHE_HOME/x2rock/art` (mode 700), named by MD5, pruned hourly at most: past
  30 days, then least recently used down to 50 MB.

`--each` streams a row per image as it lands. `CoverArt.qml` shows only a `file://` path in that
cache.

### Speech: `x2rock say`

The provider is `src/speech.rs`:

- ElevenLabs text-to-speech, with the key in `speech.json` (mode 600) or `ELEVENLABS_API_KEY`;
  `--set-key` reads it from stdin.
- The voice list is remembered, so a key restricted to text-to-speech still resolves names.
- The provider is an enum, ready for an OpenAI-shaped endpoint, which is not built until it can be
  verified.

The speaker cannot fetch from ElevenLabs (its bearer path is not the API key), so `clipserve.rs`
serves the clip once, on port 3401, to the target player's address only. If the player never
connects, the error is `clip_not_fetched`, with the `ufw` line in `data.firewall_rule` and no `fix`.
Clips are cached by provider, voice, model and text. Playback is `loadAudioClip`, player-scoped, like
`notify`.

### Decisions upheld on review

These were challenged and stand; do not "fix" them:

- `lock().unwrap()` under `panic = "abort"`.
- The restart channel's capacity of one, since a reconnect signal needs only the latest message.
- `Service::entry` looks up by name, not position.
- Clamp-then-cast for volume, rather than a silent fallback to 0.
- `http.rs` reads to EOF.
- The picker's merged search is opt-in (`searchService` defaults to TuneIn).

**Process:** run `cargo fmt --check`, Clippy with `-D warnings` and the tests before pushing; CI
checks all three.

## Tested hardware and its limits

Home has five rooms:

- a 5.1 Beam with a Sub and two Play:1 surrounds;
- a 5.0 Beam with two One SL surrounds;
- a standalone Beam;
- a One SL;
- a SYMFONISK Bookshelf stereo pair.

The office has one One SL. All of them are on SonosNet, through one wired Beam.

`system` reads the topology's `ConnectionType`: 1 is wired, 2 is SonosNet (a pair half included),
6 is a home-theatre satellite. `WirelessMode` reads 0 everywhere on S2 and means nothing. A speaker
joined to WiFi rather than SonosNet has never been observed, so it reports `unknown`.

Known blind spots:

- **No Atmos.** The Beam (S14) is first generation, so a `5.1.2` reading needs an Arc or a Beam
  gen 2.
- **No line-in.** There is no Port or Amp here, and no line-in command.
- **A speaker on the home WiFi.**
- **`DialogLevel` above 1.**
- **TruePlay's `Available` meaning.**
- **Multi-room scene apply and party mode** have not been tested against the current code.

One port scan of a One SL (firmware 96.1) found 1400, 1410 (an HTTP server answering 404 to
everything), 1443 and 7000 (AirPlay) open. Sonos's published firewall list covers the whole product
line and history.

## Open questions

- **The widget and `omarchy.media`.** Omarchy's media widget shows MPRIS title and artist, never the
  room. It prefers players with a local PipeWire stream, so a Sonos room never wins that round, and
  then the oldest playing player. The widget could pin a room with
  `bar.shell.firstPartyServiceFor("omarchy.media").selectPlayer(dbusName)`, or better, read
  `activePlayer` to mark which room the bar is showing. Neither is built, and behaviour with several
  rooms on the bus is untested.
- **The household's default account** for an enqueue with no `sn=`: the lowest live serial or the
  first registered. One household cannot separate the two.
- **`homeTheater:1 setOptions`**, a possible Control API write path for night and dialog. It has not
  been tried with real parameters.
- **The TUI** has no queue, sources or search panes yet.
