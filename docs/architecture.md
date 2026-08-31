# x2rock — Architecture & Design Notes

> This began as the project kickoff and has been kept current as the design was verified against
> real hardware. It records not just what x2rock does but *why* — the transport choice, the
> firewall and discovery constraints, and the protocol facts each decision rests on. Every claim
> marked "verified" was tested against an actual Sonos player, not taken from documentation.
>
> The original kickoff assumed the Sonos **cloud** Control API was the only integration path. That
> was wrong; the local LAN WebSocket is better on every axis. See "Integration path" below.

## What this is

A Rust rewrite of the Sonos control CLI, split off from the original `x2rock` project. The
original repo has been renamed **`x2rocktv`** and continues as the Kotlin/JVM Android TV app
(`:app` + `:core`) — locally at `~/x2rocktv`, on GitHub at `rahga/x2rocktv`. Note that the rename
moved the source tree only; see the config-path note under "Carried-over knowledge".

This new repo — the new **`x2rock`** on GitHub — is Linux-only, Rust, and targets
**Omarchy 4.0 "Quattro"** specifically. No Android code, no JVM, no `:core` shared-module
architecture to preserve.

Not a port of the Kotlin CLI line-for-line — a fresh implementation informed by everything the
Kotlin version got right (and wrong) over its lifetime. See "Carried-over knowledge" below.

### Reading this document from another language or platform

Most of what is here is not about Rust or Linux at all. Two thirds of it is protocol: what the
speakers answer, what they refuse, and what they do while they are refusing it. That part is worth
as much to a Kotlin app on Android TV as it is here, so it is worth being explicit about which is
which.

**Protocol truth — portable anywhere, and the reason this file exists:** "Integration path" and its
queue subsection, "Volume handling", "Grouping over the local API", "Soundbars: the TV input",
"Per-player volume, and who may be asked", "Queue mutation over UPnP", "Favorites over the local
API", and "Protocol facts learned while building the daemon". Every "verified" note in those is a
statement about Sonos hardware, not about this implementation.

**Host-specific — true here, and not to be inherited without retesting:** "The firewall problem"
and "Discovery" are both conclusions about *Omarchy's default-deny ufw*, not about Sonos or about
networks in general. "Target platform", "Language & structure", "Rust ecosystem notes" and the
Quickshell sections are implementation.

**Mixed:** "Connection lifecycle and network mobility" - the *reasons* to reconnect are universal,
the logind and NetworkManager mechanisms are not.

A concrete porting guide is under "Porting to Android TV" below.

## Scope

- **In scope**: **local-first** Sonos control over the LAN Control API (see below), a `clap`-based
  CLI (`x2rock play`, `rooms`, `vol`, etc.), an MPRIS2 server so it works with anything that
  already speaks MPRIS, and first-class support for Omarchy Quattro's Quickshell top bar.
- **Also in scope, and required**: UPnP queue navigation (see below). Playing a chosen track from
  the queue is a core requirement, and only UPnP can do it.
- **Cut from v1**: cloud OAuth (Sonos Control API over `api.ws.sonos.com`). Off-LAN control is not
  wanted for now — decided 2026-08-28. Keep the transport seam so it *can* return, since off-LAN
  control is what would arrive through it, but build no OAuth, no token storage and no `login`
  command for v1. An earlier draft of this bullet also listed *music services* as an account-linked
  feature waiting on the cloud. That was wrong — a service is linked to the **household**, not to a
  Sonos account, and the LAN hands out everything needed to address one. See "Music service search,
  reopened (verified 2026-08-31)".
- **S2 only.** Every supported device runs Sonos S2. S1 is not supported and no S1 accommodation
  belongs in this code. The check is `<swGen>` in
  `http://<player>:1400/xml/device_description.xml`, which reads `2` on every device here. This is
  not a reluctant cut: S1 and S2 systems cannot be mixed in one household anyway, so supporting
  both would mean carrying two content and grouping models to reach households this tool's target
  desktop is unlikely to be sitting on.
- **Out of scope**: Android, any pre-Quattro Omarchy/Hyprland/Waybar-specific work (Quattro
  replaced that whole stack — don't design around it), and Sonos S1.

## Integration path (revised — this is the core decision)

Sonos speakers expose the **same JSON Control API protocol on the local network** that the cloud
API exposes remotely, over a WebSocket, with **no OAuth and no internet**. This is the transport
the official Sonos mobile app itself has used since the v80/2024 rewrite.

**Verified 2026-08-28** against a Sonos One SL (`RINCON_48A6B81853E001400`, "Media Room",
firmware `95.0-77060`, `apiVersion` 1.52.1):

```
URL        wss://<player-ip>:1443/websocket/api
Headers    X-Sonos-Api-Key: 123e4567-e89b-12d3-a456-426655440000
           Sec-WebSocket-Protocol: v1.api.smartspeaker.audio
           (send NO Origin header)
TLS        self-signed — certificate verification must be disabled
Wire       2-element JSON array: [command, options]  — responses and events likewise
```

Confirmed working **unauthenticated**, reads *and* writes: `groups:1`, `playback:1`,
`playbackMetadata:1`, `groupVolume:1`, `playerVolume:1`, `favorites:1`, `playlists:1`,
`settings:1`, `audioClip:1`. A `setVolume` write succeeded. `subscribe` returns an immediate state
snapshot and then delivers **unsolicited push events** on change.

Why this wins over the alternatives:

- **No login.** `x2rock login` stops being a precondition for the tool working at all.
- **Push, not polling.** Solves the highest-leverage item in "Carried-over knowledge" below,
  without the cloud.
- **One outbound TCP connection**, carrying commands and events. This matters enormously — see
  "The firewall problem" below.
- **Same command/event model the Kotlin project already documented**, so that knowledge transfers
  directly.
- **Durable.** The API is undocumented and Sonos has said the Control API on the LAN is "not
  available for wide release" — but the official app runs on it, so it can't be removed without
  Sonos breaking their own product.

Residual risk: 2025+ firmware added an **Authentication** setting covering third-party LAN API
use, which can require client-certificate auth. It is **off by default** (demonstrably — all of
the above worked). Mitigate by keeping the transport behind an abstraction so cloud OAuth can slot
in as a second implementation rather than a rewrite.

Note the API key above is a well-known sample key, not one issued to this project.

### Queue navigation and content: UPnP is mandatory, not optional

**Requirement**: play different tracks in the queue; the queue may be populated from the official
Sonos app or elsewhere.

The Control API **cannot do this at all**. Verified 2026-08-28 by namespace probing — `queue:1`,
`playbackQueue:1` and `cloudQueue:1` all return `ERROR_UNSUPPORTED_NAMESPACE`; `playback:1
skipToItem` and `playback:1 loadQueue` return `ERROR_UNSUPPORTED_COMMAND`. The namespace model only
understands *cloud queues* that your own app hosts and loads via `playbackSession:1`. It has no
view of the local Sonos queue that the official apps populate. (This is the same gap that left
queue management missing from the 2024 Sonos app rewrite.)

**UPnP does it completely.** Verified on the test speaker:

- `ContentDirectory Browse` on `ObjectID=Q:0` enumerates the live queue with full metadata.
- `AVTransport Seek` with `Unit=TRACK_NR` jumps to a queue position.
- Full mutation is available: `AddURIToQueue`, `AddMultipleURIsToQueue`, `RemoveTrackFromQueue`,
  `RemoveTrackRangeFromQueue`, `RemoveAllTracksFromQueue`, `ReorderTracksInQueue`, `SaveQueue`,
  plus a dedicated `Queue` service (`AddURI`, `ReplaceAllTracks`, `ReorderTracks`,
  `SaveAsSonosPlaylist`).
- `ContentDirectory Browse` also enumerates the local library: Artists, Albums, Genres, Composers,
  Tracks, Playlists.

All of these are **outbound SOAP calls**, so they work on a stock Omarchy box. This corroborates
that the traditional Sonos desktop app — which handles queues well — is UPnP-based.

**The hybrid, and how the two transports cooperate:**

| Purpose | Transport | Notes |
|---|---|---|
| Control, state, **events** | WebSocket `:1443` | the spine |
| Queue read/navigate/mutate | UPnP SOAP `:1400`, **calls only** | required for the core use case |
| Content enumeration | UPnP SOAP `:1400`, **calls only** | same path, free once queue works |
| Remote (off-LAN) control | cloud OAuth | cut from v1 |

The neat part: `playback:1` events carry a **`queueVersion`** field. So the WebSocket tells you
*that* the queue changed — push, no polling — and you re-`Browse Q:0` over UPnP only when the
version actually bumps. You get push semantics for a resource UPnP would otherwise make you poll,
without ever needing GENA eventing.

### Music search: harder than the rest, but not closed

**Revised 2026-08-31 — the original text here was right about the transports and wrong about the
reason.** Two of its three claims hold:

- `musicService:1` exists as a namespace but its `search` command returned
  `ERROR_UNSUPPORTED_COMMAND` (2026-08-28).
- Sonos's `ContentDirectory` reports **empty** `SearchCapabilities` — it implements no UPnP
  `Search` action at all. Re-confirmed 2026-08-31.

The third claim — that service search goes through SMAPI, "which needs the service's own endpoint
and per-service authentication", and is therefore beyond a tool with no Sonos account — conflated
a *Sonos* account with a *service* account. The endpoint is handed out free over the LAN, and the
service link belongs to the household rather than to any controller. Local library and
queue/favorites search — filtering what `Browse` already returns — is still the cheapest thing and
still worth doing first, but it is no longer the only thing on the table. See "Music service
search, reopened (verified 2026-08-31)".

**Never use UPnP eventing (GENA `SUBSCRIBE`).** See below.

## The firewall problem (design-critical)

Omarchy's own installer (`/usr/share/omarchy/install/config/firewall.sh`) runs:

```
ufw default deny incoming
ufw default allow outgoing
```

The only inbound exceptions are LocalSend (53317) and Docker DNS. **Every stock Omarchy install —
the sole target platform — blocks inbound by default.** Verified on the dev machine.

Consequences, all of them *silent*:

- **SSDP discovery finds nothing.** M-SEARCH replies arrive from the speaker's unicast IP and
  don't match the conntrack entry for the packet sent to `239.255.255.250`, so they are dropped as
  unsolicited. mDNS fails the same way.
- **UPnP GENA eventing never fires.** `SUBSCRIBE` returns **HTTP 200 with a valid SID**, and then
  no event ever arrives. There is no error to observe. This is the same symptom Home Assistant
  users report as "Sonos subscriptions failed", and it will be this project's number-one support
  question if it is ever relied upon.
- **The LAN WebSocket is unaffected**, because it needs no inbound connection.

Both failures were reproduced and then fixed with a single `ufw allow from <speaker-ip>`, which
confirms the host firewall — *not* the network — as the cause. (The dev network is corporate
Fortinet WiFi and passes both multicast and client-to-client unicast fine.)

If UPnP eventing or SSDP is ever used as a fast path, detect the silent case — *subscription
accepted, zero events within N seconds* — and tell the user to check their host firewall before
they start suspecting their network.

## Discovery

Multicast is not dependable **here**, and the cause is this host's firewall rather than the network
or the speakers - so do not build on it, and do not carry the conclusion to a platform without that
firewall. See "Re-derive, do not inherit: discovery" under the Android TV porting notes.

1. **Steady state**: cached player IP + household ID in config; connect outbound to `:1443`.
   No firewall interaction at all.
2. **First run / bootstrap**: TCP connect-scan on **port 1443 only**, stopping at the first hit.
   Outbound, so it works through default-deny.
3. **Then fan out**: `groups:1 getGroups` returns **every** player's own `websocketUrl`,
   `householdId` and capabilities. So you scan exactly once, ever — and this also makes the
   multi-household problem below fall out naturally instead of needing special handling.
4. **SSDP as an opportunistic fast path only**, short timeout, never the sole mechanism.

Keep any scan to an explicitly-invoked `x2rock discover`, single-port and rate-limited — an
aggressive full-subnet sweep on every launch is poor manners on an office network and can register
as reconnaissance on corporate gear.

## Language & structure

- **Rust**, single crate (not a workspace) — this is a from-scratch project, not one preserving a
  reusable-core split for a second frontend that no longer exists here.
- Layout as built:
  ```
  x2rock/
    Cargo.toml
    src/
      main.rs        # clap CLI entry point
      session.rs     # command -> live connection + known household; shared by CLI and daemon
      discover.rs    # finding players: outbound TCP sweep of the local subnet, no multicast
      netid.rs       # identifying the attached network, so cached players are scoped to it
      restart.rs     # logind and NetworkManager: when to drop everything and reconnect
      state.rs       # $XDG_STATE_HOME/x2rock/: which players live on which network
      daemon.rs      # `x2rock daemon`: one MPRIS2 player per group, kept current by events
      mpris.rs       # the MPRIS2 server itself
      sonos/
        mod.rs
        local.rs     # LAN transport: WebSocket straight to a player, no cloud, no OAuth
        api.rs       # Control API commands as methods on a connection
        proto.rs     # wire types, shared by the cloud and LAN flavours of the protocol
        upnp.rs      # queue over UPnP/SOAP on port 1400 (SOAP calls only, never GENA)
    quickshell/x2rock.sonos/   # Omarchy Quattro bar widget: BarWidget.qml, CoverArt.qml,
                               # manifest.json, and a README of its shell.json keys
    systemd/x2rock.service     # user unit for the daemon
  ```
  The one split that was worth having up front is the transport boundary (`sonos/local.rs` vs
  `sonos/api.rs`), because it is what contains the Authentication-setting risk noted above. The
  rest was split only as files actually got unwieldy, per the project's "no unnecessary
  abstractions" preference.

## What this was actually tested on (2026-08-29)

One household: three Sonos Beams and a One SL on firmware 95.1-78010, and an
IKEA SYMFONISK Bookshelf on 86.7-77050. Worth recording for two reasons. The
SYMFONISK is a third-party player and behaves identically, on firmware nearly a
decade of version numbers behind the rest — so nothing here depends on being
first-party or current. And every "verified" note in this file means *these*
devices, which is a real limit on some of them.

Two gaps are known and specific rather than general:

- **No height channels have ever been seen.** `HomeTheaterFormat` handles them
  and would render `5.1.2`, but no speaker here does Atmos, so that branch has
  never run against hardware. An Arc would settle it.
- **No player with analog line-in.** `playback:1 loadLineIn` was found to refuse
  a Beam - "player does not have line-in" - which is why TV input goes over UPnP.
  A Port or an Amp would very likely accept that same command, so the conclusion
  "the Control API cannot switch inputs" is really "cannot switch a *soundbar's*
  HDMI input", and should not be generalised further than that.

## Portability, and where the Omarchy dependency actually is

Recorded 2026-08-29, because "targets Omarchy" reads as a harder constraint than it is.

The Omarchy dependency is the bar widget and nothing else. `src/` contains no reference to Omarchy,
Quickshell or Hyprland; the CLI and daemon are ordinary Linux programs. What they do rely on is
generic: systemd for the user service, a session D-Bus for MPRIS, `/proc/net/arp` and the interface
netmask for discovery, and plain outbound TCP for both transports.

logind and NetworkManager are used but not required — each is attempted separately and a missing
one costs only its own speed, which was built that way for laptops and turns out to be what makes
a machine without either work at all.

Two things that would trip a non-Omarchy install, both worth stating rather than discovering:
`edition = "2024"` needs Rust 1.88, which is newer than several distributions package; and the
systemd unit is `WantedBy=graphical-session.target`, which never fires on a headless machine.

This is not a promise to support other desktops. It is a note that the CLI and the MPRIS daemon
cost nothing to use elsewhere, and that MPRIS is where most of the widget's value already lives -
`playerctl`, Waybar, GNOME and KDE all drive it without knowing what Sonos is.

## Porting to Android TV (Kotlin), with no Sonos account

Written 2026-08-29 for the `x2rocktv` line, whose driving requirement is an Android TV app that
does **not** depend on logging in to a Sonos account.

That requirement is not a preference. **Sonos's OAuth consent page cannot be completed with a
remote** - reaching and activating its Sign In control wants a mouse and keyboard, which is not
what a television has. A login flow that assumes a pointer is a login flow an Android TV app cannot
ship, whatever else is right about it. The usual escape on this platform is a second-screen or
device-code grant, where the TV shows a short code and the sign-in happens on a phone; that is not
offered here. So the account path is not merely undesirable on TV, it is closed, and a transport
that never asks for one is the only way the app exists at all.

**That requirement is already met by the central finding here, and cheaply.** Everything x2rock
does - rooms, transport, volume, grouping, favorites, queue read *and* write, soundbar TV input,
what the TV is sending - runs over the LAN with no login, no token, no OAuth, no internet. See
"Integration path". The only capability that needs an account is control from outside the house.
So the port does not need a reduced feature set to avoid OAuth; it needs the same feature set over
a different transport than the cloud API, and that transport is fully documented above.

### Re-derive, do not inherit: discovery

This is the one place where copying a conclusion from this document would be a mistake.

"Discovery" says multicast is not dependable and to use an outbound TCP connect-scan of port 1443.
That is a true statement about **an Omarchy laptop**, and the cause is named in "The firewall
problem": `ufw default deny incoming` drops the speakers' unicast SSDP replies because they do not
match the conntrack entry for the multicast query. It was proven by fixing it with a single
`ufw allow from <speaker-ip>`, and re-confirmed 2026-08-29 - an M-SEARCH for
`urn:schemas-upnp-org:device:ZonePlayer:1` from this host still gets **zero replies** while five
players sit on the same subnet answering everything else instantly.

The network passes multicast fine. The speakers answer. A **stock Android TV device has no host
firewall doing this**, so SSDP is likely to work there and would replace the subnet scan entirely -
faster, politer, and without the "looks like reconnaissance on corporate gear" problem. Test it
first rather than porting the scan.

Two Android caveats if you do: multicast receive needs a `WifiManager.MulticastLock` held across
the query, and an Android TV box is usually on Ethernet, where that lock is not the relevant
control - so verify on the transport the device actually uses, not on an emulator. Keep the port
1443 connect-scan as the documented fallback, because it works through anything.

### Platform translations

| This implementation | Android TV equivalent | Note |
|---|---|---|
| `rustls` verifier accepting a self-signed cert | custom `X509TrustManager` + hostname verifier | the cert never matches the IP; both have to be relaxed, and only for the players |
| `tokio-tungstenite` on `wss://ip:1443` | OkHttp `WebSocket` | `Sec-WebSocket-Protocol: v1.api.smartspeaker.audio` and **no `Origin` header**; confirm the client library lets you control both before building on it |
| MPRIS2 over D-Bus, one bus name per group | `MediaSession` per room, or one session plus a room switcher | this is the biggest design decision in the port and has no obvious right answer |
| `x2rock:*` MPRIS metadata keys | `MediaMetadata` / `MediaSession` extras | same idea: the standard has no notion of "which rooms are grouped", "is this on TV", or "what channels is the TV sending", so they ride as custom keys |
| systemd user unit | foreground service | Android TV rarely sleeps, but a background service will still be killed |
| logind `PrepareForSleep`, NetworkManager `StateChanged` | `ConnectivityManager.NetworkCallback` | the *reasons* in "Connection lifecycle" all still apply; only the signal changes |
| `$XDG_STATE_HOME/x2rock/` keyed by network | app-private storage keyed the same way | "Identify the network before deciding what to try" is not Linux-specific |
| `/proc/net/arp` + interface netmask | `ConnectivityManager` `LinkProperties` | only needed if you keep the connect-scan |

### Android traps this codebase never had to face

- **UPnP is plain HTTP, and Android blocks cleartext by default.** Everything in "Queue mutation
  over UPnP" runs over `http://<player>:1400` with no TLS. Since API 28 that is refused unless a
  network security config permits it. Get this wrong and the whole queue layer fails - possibly
  quietly, which is the worst kind. It is the first thing to prove on device, before writing any
  SOAP.
- **Two different trust relaxations, for two different ports.** 1443 needs a self-signed
  certificate accepted; 1400 needs cleartext allowed. They are configured in different places and
  neither implies the other.
- **Scope both narrowly.** These are local speakers on a home LAN; a blanket "trust everything"
  config is a real weakness in a shipped app, not a shortcut.
- **Chunked responses.** The players answer UPnP with `Transfer-Encoding: chunked` and
  `Connection: close`. A normal HTTP client handles this; this codebase hand-rolls it only because
  its client is deliberately minimal. Do not port `dechunk`.

### What does not transfer at all

D-Bus and MPRIS, systemd, XDG paths, `/proc`, logind, NetworkManager, Quickshell and every QML
section, and the Rust crate choices. The Quickshell notes are still worth skimming for *what a
control surface needs to show* - per-room volume, group membership, the TV format at a glance -
which was learned from use rather than from the protocol.

### What the Rust version learned that the Kotlin one predates

Beyond the protocol sections, four hard-won bugs are worth carrying over as design rules, all
written up above: one connection **per coordinator** rather than one per household (routing group
commands down an arbitrary socket published one room out of five and then retried forever);
`playerVolume:1` addressed to the player itself and never the coordinator; topology compared
properly before republishing, or every snapshot flaps every bus name; and a member's socket treated
as best-effort so one flaky portable cannot tear down the household.

## Target platform: Omarchy 4.0 "Quattro"

Confirmed from Omarchy's own repo (`basecamp/omarchy`, `quattro` branch) as of this writing:

- Quattro (released 2026-08-14) rewrote Omarchy's entire shell — bar, launcher, notifications,
  OSDs, lock screen — into one long-running **Quickshell** process with a plugin architecture.
  This fully replaced the prior Waybar + Hyprland-config-script stack. **Do not design for
  pre-Quattro Omarchy or generic Waybar-first integration** — Quattro is the only target.
- Quickshell bar plugins support three integration shapes (source:
  `shell/plugins/bar/README.md` in that repo):
  1. **Command polling** — a plugin config declares `{"type":"command","exec":"...","interval":N}`;
     output is plain text or Waybar-style JSON (`text`/`tooltip`/`class`). This is the lowest-effort
     integration path and probably where `x2rock`'s CLI binary plugs in first.
  2. **Native QML widgets** — get `bar`/`moduleName`/`settings` injected, can fire-and-forget shell
     commands via `bar.run(...)`. More work, richer UI (needed for anything MPRIS can't express).
  3. **Direct D-Bus/MPRIS subscription** — for widgets that want live now-playing data without
     polling a command.
- **MPRIS is still the built-in, first-class mechanism.** Omarchy ships a built-in (off-by-default)
  `omarchy.media` plugin that reads MPRIS now-playing data directly (scrolling track/artist, cover
  art, click/scroll transport controls) — see `manual/05-the-top-bar.md`. Enabled via
  `omarchy plugin enable omarchy.media --section center`; config lives in
  `~/.config/omarchy/shell.json` under `bar`.
- **Practical implication**: publish a standard MPRIS2 interface and Omarchy's own `omarchy.media`
  widget picks it up with zero custom code, same as Waybar's `mpris` module did before. A bespoke
  Quickshell widget is only needed for things MPRIS genuinely can't express: multi-room grouping,
  per-room/per-player volume, favorites, household switching. That bespoke widget would most
  likely use the **command-polling** pattern, shelling out to the `x2rock` binary with `--json`
  output, not a native Rust↔QML binding — Quickshell widgets are QML/JS; there is no Rust-native
  integration point.
- No official upstream Quickshell documentation was directly verified — everything above comes
  through Omarchy's own docs of how it uses Quickshell. If Quickshell has more integration surface
  than Omarchy exposes, that is still undiscovered.
- Later, also support Waybar per the original ask — since MPRIS is the shared mechanism, this
  should come close to free once the MPRIS server exists; Waybar's `mpris` module needs no code on
  x2rock's side at all.

## Volume handling (Sonos's own rules — follow them)

Volume is the single most bug-prone area of any Sonos client: a group volume change makes every
member emit an event, ordering is not guaranteed, and naive handling produces event storms. This
is a documented, well-known failure mode — the 2024 Sonos app shipped it. Per
`docs.sonos.com/docs/volume`:

- **Never send a volume command in response to a volume event.** This is the feedback loop.
- **Batch commands** — no more than roughly one per 100ms.
- **Always use group volume commands for groups.** Never fan out per-player commands; doing so
  destroys the user's carefully-set relative levels between speakers.
- **`setVolume` for controls with known state** (a slider); **`setRelativeVolume` for stateless
  controls** (buttons).

That last rule maps directly onto this project's integration points: a Quickshell scroll-to-adjust
gesture and MPRIS volume steps are *stateless increments*, so they must use `setRelativeVolume`,
not read-modify-write.

**Measured 2026-08-28 on the One SL — the player enforces its own coalescing:**

- A single volume command is applied and readable within ~21ms of its ack.
- A second volume command arriving within roughly **260ms** of the first is *deferred* until that
  window closes. During the window, `getVolume` returns the pre-change value even though the
  command was acked as successful. Identical behaviour whether the commands share a connection
  or each opens a fresh one.
- **Both `setVolume` and `setRelativeVolume` unmute** the group as a side effect.

Consequences: a read straight after a write is unreliable and must never be used to report a
result — the CLI computes the outcome from the requested change instead. A scroll-wheel burst from
a bar widget will be coalesced by the player into one settled `groupVolume` event ~260ms later;
the daemon should surface *that* event and nothing else, and per the rule above must never answer
it with a further command.

## Connection lifecycle and network mobility (design-critical)

**Decided 2026-08-28: build for the laptop case.** Many users are expected to be road warriors,
moving between home, office, client sites and hotels. This is not a hardening detail bolted onto an
always-on design — it changes the shape of the program.

### The speaker being absent is the normal state, not an error

A road warrior's laptop is on a network with no reachable Sonos most of the time. Treat "no player"
as a first-class steady state:

- MPRIS should **withdraw its bus name** when no player is reachable, rather than advertise a
  player that cannot be controlled. A dead media widget in the Omarchy bar is worse than no widget.
- Reconnect with **capped exponential backoff** — retrying every few seconds forever on a network
  that has no Sonos wastes battery and fills logs. Reset the backoff on a network change, not on a
  timer.
- Never treat absence as a failure worth reporting loudly. Log it once, quietly.

### Identify the network before deciding what to try

Config should not hold "the speaker IP" but **known locations**, each with a household ID, its
players, and their last-known addresses. On any network change, work out where you are first:

- **Primary fingerprint: the default gateway's MAC address** (`ip neigh show <gateway>`). It is
  stable per site and does not collide the way SSIDs and RFC1918 subnets do — `192.168.1.0/24` and
  an SSID of `guest` are shared by half the planet.
- **Secondary: SSID / NetworkManager connection UUID.** NetworkManager is present on Omarchy
  (`nmcli` available, service active) and its D-Bus signals are the right change trigger.
- On an unrecognised network: try nothing, advertise nothing, wait.

### Wake and network-change handling

- **Subscribe to `PrepareForSleep` on `org.freedesktop.login1`.** It is present on Omarchy. (Built
  2026-08-28. The original claim that the MPRIS `zbus` connection makes this nearly free was
  wrong — logind is on the *system* bus and MPRIS on the session bus, so it needs its own.)
- **A frozen socket is not a closed socket.** On resume the WebSocket is typically a zombie: writes
  succeed into a dead TCP session and nothing surfaces until a long timeout. Do not wait for the
  socket to report failure — treat wake as *assume dead, reconnect from scratch*.
- **Network changes happen while awake too** — docking, VPN up/down, AP roaming. NetworkManager
  signals matter at least as much as sleep signals.
- **Re-resolve the address on reconnect.** A cached IP is a hint, not a fact; DHCP leases lapse.
- **Missed events are not recoverable and do not need to be.** Subscriptions do not survive a
  reconnect, and there is no replay. Re-`subscribe`, take the fresh state snapshot as truth, and
  discard pre-sleep state rather than merging it.

### Do not scan networks you do not know

The bootstrap TCP scan is acceptable on a network the user has identified as theirs. Running it
automatically on hotel, airport, conference or client-site WiFi is genuinely bad behaviour and is
exactly the traffic corporate security gear is tuned to flag. So:

- **Never scan automatically on an unrecognised network.** Cached addresses only.
- Scanning happens on **explicit `x2rock discover`**, single-port, stop-at-first-hit.
- Once a location is known, `getGroups` keeps its player list fresh without ever scanning again.

### Expect guest networks to block this outright

Hotel and guest WiFi very commonly enable client isolation, which blocks client-to-client traffic
entirely — the WebSocket will not connect at all, and nothing can fix that from this side. Fail
fast and quietly. (Note that the *dev* network, corporate Fortinet WiFi, does **not** do this; the
failures seen during investigation were the local host firewall. See above.)

## Grouping over the local API (verified 2026-08-29)

Probed for the contract first, then exercised on real rooms.

- **`modifyGroupMembers` is the right primitive in both directions**, not `createGroup`. It is
  group-scoped, takes `playerIdsToAdd` and `playerIdsToRemove`, keeps the group and whatever it is
  playing, and moves only the players named. Removing a player leaves it as a group of its own, so
  "ungroup" needs no separate command. `createGroup` exists and wants `playerIds`, but builds a new
  group and so decides the coordinator itself.
- **It answers with the resulting group** (`groupInfo`), which is what lets the CLI print what
  actually happened rather than assuming its request took effect.
- **Empty on both sides is a safe no-op** that still returns the group - useful for probing, and it
  means a redundant request costs nothing.
- **`evictPlayers` does not exist** on this firmware: `ERROR_UNSUPPORTED_COMMAND`.
- **A group is named after its coordinator and renamed as it grows** - "Dining Room" becomes
  "Dining Room + 2". So a room must be resolved through *players*, never group names: the group
  name is not stable, and the player name is.
- **A room keeps its own queue across grouping.** Joining shows it the group's queue; leaving gives
  it its own back. Confirmed deliberately (2026-08-29) after a session of regrouping left a room
  empty and cast doubt on it: Kitchen with six tracks joined a group of three, read those three
  while grouped, and had its own six again on leaving.
- **What leaving does not restore is the position.** The room comes back `IDLE` at the first track
  rather than paused where it left off.
- **Removing the *coordinator* from a group hands its queue to whoever is left, and costs them
  theirs.** Reproduced deliberately: Kitchen holding six tracks, Dining Room holding three; add
  Dining Room to Kitchen so Kitchen coordinates, then tell Kitchen to leave. Kitchen departs with
  its own six intact, and Dining Room — the room that stayed — comes out holding a copy of those
  six, its own three gone.

  Worth stating plainly because the damage lands where nobody looks for it: the room that *left* is
  fine, and the room that *stayed put* is the one whose queue was replaced. It also explains a
  household that ended up with two rooms holding identical copies of a third's queue while the
  third was empty, which had looked like a queue being lost at random.

  This is Sonos's own behaviour, not something x2rock introduces, and the Sonos app permits the
  same move — so it is documented rather than prevented. But any UI that lists a group's members
  with a "leave" beside each, as the bar widget's grouping panel does, makes it a single click.

## Music service search, reopened (verified 2026-08-31)

Search was ruled out of scope on 2026-08-29, and the decision rested on one claim: that reaching a
music service needs "per-service authentication", meaning the Sonos account this tool deliberately
does not have. **That claim conflated two different accounts and is wrong.**

The first counter-evidence was ordinary use, not a probe. The Sonos app on iPhone and on Android,
*not* signed in to a Sonos account, lists services — Sonos Radio, YouTube Music — searches them,
and a Sonos One SL plays what the search returns. Whatever search needs, a Sonos login is not it.

The reason is that a music service is linked to the **household**, not to a controller. Any
controller on the LAN inherits the link. This document had already recorded half of that without
following it through: `favorites:1 getFavorites` works on the LAN with no login, and each favorite
carries the service's own account token inside `r:resMD`.

### Probed 2026-08-31 (office household, Sonos One SL, firmware 95.0-77060 / displayVersion 18.4)

- **`MusicServices ListAvailableServices` answers unauthenticated over the LAN** — 53KB, 108
  services. Each descriptor carries the SMAPI endpoint (`Uri` and `SecureUri`), a `Capabilities`
  bitmask, a `<Policy Auth=...>` and a `<Manifest Uri=...>`. "Needs the service's own endpoint" was
  never a barrier; the speaker gives it away.
  - `Id="284" Name="YouTube Music"`, `Auth="AppLink"`, `https://music.googleapis.com/v1:sendRequest`
  - `Id="303" Name="Sonos Radio"`, `Auth="DeviceLink"`, `https://sali.sonos.superhi.fi/smapi`
- **The `Manifest` URI is fetchable anonymously** from Sonos's CDN (`cf.ws.sonos.com/p/m/<uuid>`)
  and declares typed endpoints. The two services differ in shape, which matters for the client:
  - Sonos Radio declares a dedicated search endpoint —
    `{"type":"search","uri":"https://sali.sonos.superhi.fi/content/search","version":"2.3"}` —
    alongside `browse`, `radio` and `reporting`.
  - YouTube Music declares only `reporting`, so its search is the classic SOAP `search` action at
    its `SecureUri`.
  - So a client needs **both** paths, and the manifest is what tells it which to use.
- **The presentation map (`cf.ws.sonos.com/p/p/<uuid>`, also anonymous) declares the search
  categories**, which is exactly what a search UI needs to offer:
  - Sonos Radio: `stations`, `artists`, `genres`, `all`
  - YouTube Music: `artists`, `playlists`, `tracks` (mapped `SONGS`), `albums`, `all`
- **Which services a household has linked is derivable from `FV:2`.** Every favorite here carries
  `<desc id="cdudn">SA_RINCON77575_X_#Svc77575-0-Token</desc>`, and 77575 = 303·256 + 7 — service
  303, Sonos Radio. No account call needed to learn the linked set.
- Note the household difference: the office household has **3** favorites and one linked service.
  The 41-favorite count and the `Svc51463` token recorded elsewhere in this document are the home
  household. Numbers in this document are per-household; the mechanisms are not.

### What still stands

- **UPnP `Search` does not exist.** `GetSearchCapabilities` returned an empty `SearchCaps` again on
  2026-08-31. Search will not come from `ContentDirectory`.
- `musicService:1 search` returned `ERROR_UNSUPPORTED_COMMAND` on 2026-08-28. **Not re-probed** —
  the CLI has no raw Control-API command and this machine has no websocket client — so treat it as
  probably-still-true rather than confirmed.

### What is genuinely unsolved: the credential

The endpoint is free; the credential is not in our hands. `SA_RINCON77575_X_#Svc77575-0-Token` is a
*reference*, not a secret — the literal trailing `Token` is a placeholder the **player** resolves
against its own stored credential. That is precisely why passing `r:resMD` through verbatim works
for enqueuing: the player substitutes. A search issued by x2rock straight at the service endpoint
gets no such substitution.

Three questions settle the design, cheapest first:

1. **Re-probe `musicService:1` on current firmware** — `search`, and whatever else the namespace
   answers. If the *player* will run the search, the credential problem disappears and this becomes
   a small feature. This needs a raw Control-API command in the CLI, which is worth having on its
   own merits and is the obvious first commit.
2. **Where a controller's SMAPI credential comes from** with no Sonos account. `Policy Auth` is
   `AppLink` or `DeviceLink` — device-link flows that mint a per-controller token. Whether an
   already-linked household will hand one over, or whether x2rock must run the link flow itself
   once and store the result, is the pivot: the first is small, the second is a real feature with
   token storage, and it would be the first secret this tool has ever had to keep.
3. **Whether a service track can be enqueued** — the experiment named on 2026-08-29 and never run,
   because every favorite on the home household is a container or a station. Still the right test,
   and cheaper now: Sonos Radio search returns stations, and stations from that service are already
   known to play here.

Also unexplored, and possibly a way around `AddURIToQueue`'s refusals entirely: `playbackSession:1`
`loadStreamUrl` and `loadCloudQueue`. A cloud queue would mean x2rock serving HTTP the speaker can
reach, which the firewall section makes awkward but not impossible.


## Bar-widget interop with `omarchy.media`, and the room-name decision (2026-08-31)

Verified against Omarchy 4.0.2 running from `/usr/share/omarchy/shell`. **Only the office Media
Room was reachable, so everything below about *multiple* players is read out of Omarchy's source
rather than exercised — that half needs the home household to confirm.**

### Settling which widget drew what

`qs -p /usr/share/omarchy/shell ipc call shell debugBarGeometry` returns every bar slot's id,
section, x and width. Coordinates are logical; multiply by the scale (1.25 on a 1920px screen
here) to compare against a `grim` capture. Use it. Reading a screenshot by eye misattributed the
now-playing text twice before this call settled it:

- `omarchy.media` — center, x=830 w=203 (physical 1037–1291): the "now playing" text.
- `x2rock.sonos` — right, x=1396 w=18 (physical 1745–1767): icon only, as `BarWidget.qml:758`
  sizes it (`glyph.implicitWidth + Style.space(14)`).

So with the daemon running, the track name in the bar is **Omarchy's** widget rendering x2rock's
MPRIS data. x2rock's own pill contributes nothing but the speaker glyph.

### `omarchy.media` never shows the room

Its bar label is `trackTitle + "  ·  " + trackArtist`. Its popup source rows use
`trackTitle || identity` and `trackArtist || identity`. Since x2rock always supplies a title and
an artist, the MPRIS `Identity` — which is the room name — never renders anywhere in that widget.
Two rooms playing the same album produce two visually identical rows.

### Which player it picks

`plugins/services/media/Service.qml`, `selectActivePlayer()`:

```
preferred && preferred.isPlaying
  → oldestPlayingPlayer(true)    // requires a local PipeWire stream
  → oldestPlayingPlayer(false)
  → streamPreferred → streamCandidate → preferred → trackPlayer → …
```

- `oldestPlayingPlayer(true)` demands `playerHasPlaybackStream()`, a fuzzy name match of the
  player against local PipeWire *output* streams. A Sonos room has no local stream — the audio is
  on the speaker — so it can never pass that round, and **local playback outranks any room** unless
  the room is the pinned `preferredPlayerKey` *and* playing.
- "oldest" means start-order, stamped by `syncPlayingOrder()` the first time a player is seen
  playing and held until it stops. Among several playing rooms the bar keeps whichever started
  first; starting a second room does not move it, and the widget's middle-click / scroll then acts
  on the first room rather than the one just started.
- With everything paused it falls through to `preferred`, else `trackPlayer` — the first
  `Mpris.players` entry carrying metadata, i.e. D-Bus arrival order, which is not stable across
  daemon restarts.

### One player per group, not per room

`src/mpris.rs:20` and `daemon.rs:348` publish one bus per Sonos *group*, named for the
coordinator. Party mode collapses the whole household to a single MPRIS player. This is why a
multi-room house can look better behaved here than it is: grouped rooms never exercise the
multi-player paths above.

### Why the room name came out of the x2rock pill (decided 2026-08-31)

Given the line above, a room name on the pill would have named the *coordinator* rather than the
room in front of you, and re-labelled itself on every regroup. It also could not switch rooms, so
it was a label with nothing behind it. The pill stays an icon; the room list lives in the popup.

### The interop that would justify putting it back — designed, not built

`shell.qml:279` is `function firstPartyServiceFor(pluginId) { return serviceFor(pluginId) }` — no
first-party gate — and `Bar.qml`'s `injectProps()` hands every widget, third-party included, the
`bar` reference. So x2rock's widget can reach Omarchy's media service the same way Omarchy's own
widget does:

```qml
readonly property var mediaService: bar?.shell?.firstPartyServiceFor("omarchy.media")
mediaService?.selectPlayer(roomPlayer.dbusName)   // pins the bar pill to that room
```

`playerKey()` is just `player.dbusName`, which the widget already holds from
`Quickshell.Services.Mpris` — no need to recompute `bus_suffix()` in QML.

Caveats, all from the selection order above: the pin only wins outright while that player is
playing; `runAction` overwrites `preferredPlayerKey` on every successful transport action, so a
click on the media pill re-pins it; it is session state, lost on shell restart; and it is an
undeclared dependency on an Omarchy internal. That last one is harmless if `omarchy.media` is not
placed in the bar — its manifest is `keepLoaded` with kind `service`, so the service exists and
the call is a visual no-op.

The **reciprocal read is probably the better feature**: `mediaService.activePlayer` tells x2rock's
popup which room the bar pill is currently showing, so the popup can mark it. That hands
`omarchy.media` the room identity it structurally cannot express, without putting a name back on
the pill.

### Unexplained, low priority

With a single paused room, the `omarchy.media` marquee sat frozen at the same pixel offset across
four captures ten minutes apart, showing `Espresso` with the artist clipped out of view.
`running: labelText.needsScroll && !root.popupOpen` points at a stuck `popupOpen`, but the
bar-level `openPanelIndicator` (`Bar.qml:1641`, `panelOpen: root.activePopout === slot.activeItem`)
had already cleared by then, and `qs ipc call shell call omarchy.media close ""` answers
`unknown`. Worth a second look at home, where several players may make the trigger obvious.


## Quickshell facts learned wiring the per-room volume sliders (2026-08-29)

- **An `ai` metadata value does not reach QML as an array.** MPRIS metadata
  carrying a D-Bus array of *strings* (`as`) arrives as an ordinary JS array;
  the same shape as *ints* (`ai`) arrives with no length and no indexing. Member
  volumes were published as `ai` and every slider silently read zero, while the
  volumes themselves were being set correctly the whole time. Publish numbers as
  decimal strings and parse them on the far side.
- **Assigning the same object reference back is not a change.** A `property var`
  holding a JS object, mutated in place and reassigned, notifies nothing - the
  bindings that read it never re-run. Build a fresh object instead. This is what
  made an optimistic "hold the value the user just asked for" fix appear to do
  nothing at all.
- **`PanelSlider` returns its handle to the bound value on release**
  (`liveValue = value`). That is invisible for a slider bound to MPRIS, which
  updates in the same frame, and very visible for one whose value has to go out
  through the CLI and come back as an event. Anything in the second category
  needs to hold the requested value until the device confirms it.
- **A row that contains a control must not also be a click target.** The member
  rows were MouseAreas whose click removed the room from the group, with a
  volume slider inside them. Give the action its own small target and let the
  control have the rest.
- Debugging any of this from the outside is not possible: the fix was
  `console.log` inside the QML, read back from
  `/run/user/1000/quickshell/by-id/*/log.qslog`, which showed `members` arriving
  as an array and `memberVolumes` as `""` in the same line.

## Quickshell facts learned building the favorites picker (2026-08-29)

- **A bar popup cannot take keyboard focus.** Omarchy's `PopupCard` takes a `HyprlandFocusGrab`,
  which is for click-away dismissal only; it never sets `WlrLayershell.keyboardFocus`. Nothing in a
  bar popup can be typed into, which is why no bar widget has a search box. `Ui/KeyboardPanel`
  is the surface that does ask for keyboard focus, and it is what the menu, clipboard and emoji
  pickers use. So the picker is a second surface, and opening it closes the room list.
- **Two surfaces must not share an `owner`.** Both `PopupCard` and `KeyboardPanel` dismiss by
  calling `owner.close()`, so one owner means each closes the other. The picker gets its own.
- **`Ui/PanelKeyCatcher` cannot carry a text filter.** It claims `h`/`j`/`k`/`l` as arrows, `x` as
  delete and space as activate *before* emitting `textKey`, so a typed name loses letters. Its own
  documentation points at the alternative - a real `TextField` with focus - which is what this uses,
  with arrows and Enter handled on the field itself.
- **Quickshell's Mpris has no `Playlists` interface**, only a `Playlist` loop-state value. Favorites
  could not have reached the widget over MPRIS even if the daemon published them, which is why it
  shells out to the CLI instead.

## Soundbars: the TV input, and what is arriving on it (verified 2026-08-29)

- **`capabilities` already says which rooms are soundbars.** `getGroups` returns
  it per player and x2rock had been deserializing it and reading it nowhere.
  `HT_PLAYBACK` and `HDMI` mark the home-theatre players; on this household the
  Beams carry `PLAYBACK,CLOUD,HT_PLAYBACK,HT_POWER_STATE,AIRPLAY,AUDIO_CLIP,
  VOICE,HDMI`, and one adds `IR_CONTROL`. No probing needed to know which rooms
  have a TV input.
- **The Control API cannot switch to it.** `playback:1 loadLineIn` is for analog
  line-in and answers `ERROR_NOT_CAPABLE`, "player does not have line-in", on a
  Beam - with or without a `deviceId`. This is the second thing after the queue
  that the Control API simply has no reach into.
- **UPnP does it**: `SetAVTransportURI` to `x-sonos-htastream:<playerId>:spdif`,
  which is exactly what the device reports as its own `CurrentURI` while on TV.
  `spdif` covers HDMI-ARC as well as optical. Verified by switching a soundbar
  off a radio stream onto TV and back.
- **The audio format arrives as a push event, in a namespace already
  subscribed.** `playbackMetadata:1` carries `container.htInputFormat` on a
  soundbar:

  ```json
  { "numGroundChannels": 5, "numLFEChannels": 1,
    "numHeightChannels": 0, "streamDescription": "Dolby Digital Surround" }
  ```

  So "what is the TV actually sending" needs no polling and no new subscription.
  It is also the only place this is visible: `GetPositionInfo` returns
  `NOT_IMPLEMENTED` for everything on an HT stream, `HTControl:1` is only IR and
  LED, and `RenderingControl:1` has no format read.
- **The channel counts are the point, not the codec name.** A source that has
  fallen back reports its codec unchanged and drops the channels - "Dolby
  Digital 2.0" where "Dolby Digital Surround 5.1" was expected. Observed live:
  the same Beam read 2.0, then 5.0, then 2.0 again as the content changed.
- **With the television off it reports `streamDescription: "No Signal"` and no
  channels**, which is worth rendering as "No Signal" rather than "No Signal
  0.0".
- **Ask `x2rock:onTvInput` whether a room is on TV; never `x2rock:inputFormat`.**
  The format is display text and nothing else: it is empty off the TV input, but
  it is also empty *on* it whenever the player names no codec and no channels, so
  reading emptiness as "not on TV" is wrong in a state that happens routinely.
  `onTvInput` is the presence of `container.htInputFormat`, which is the question.
- **A soundbar's HDMI belongs to the player, not to the group.** A Beam that
  joined a bookshelf speaker's group still has its TV input, so "does this room
  have a TV" is `any(members)`, not the coordinator alone - and `x2rock tv` finds
  the soundbar among the members the same way.
- **Switching a *member* soundbar to TV hands it the group, and costs the
  request its answer** (measured 2026-08-29, Beam + SYMFONISK, firmware
  95.1-78010). `SetAVTransportURI` goes to the coordinator, as AVTransport calls
  do. The soundbar then takes coordination, and:

  | t | soundbar | old coordinator |
  |---|---|---|
  | 0 - 1.1s | answers ~10ms, `x-rincon:<coord>` | answers ~10ms, own queue |
  | ~1.05s | `x-rincon-queue:<self>#0` for ~250ms | - |
  | 1.1 - 13.3s | **answers nothing** | **answers nothing** |
  | ~13.5s | `x-sonos-htastream:<self>:spdif` | `x-rincon:<soundbar>` |

  The original request never returns: the coordinator stops coordinating before
  it replies, so the read times out and reports a failure for a switch that
  plainly happened. **There is no faster witness.** An uninvolved player answers
  `GetZoneGroupState` in 30ms throughout, and reports the *old* grouping for the
  same thirteen seconds. The 250ms flicker at ~1.05s is too narrow to poll for
  reliably - tried, and missed. So a switch that takes the group over is waited
  out, roughly fourteen to twenty seconds, and only then reported.
- **Addressing the soundbar directly is a different command, not a shortcut.**
  `SetAVTransportURI` sent to the soundbar itself answers in 10ms with no stall -
  but it makes the soundbar *leave* its group and take the TV alone, rather than
  bringing the room with it. Both verified; the group-preserving one is what
  `x2rock tv` means.

## Per-player volume, and who may be asked (verified 2026-08-29)

- **`playerVolume:1` is player-scoped, and only that player will answer it.**
  Sent to anyone else - the group's coordinator, say - it fails with
  `ERROR_INVALID_OBJECT_ID`, "Incorrect playerId". Group commands go to the
  coordinator; player commands go to the player itself. Getting this wrong is
  not a quiet failure: the daemon subscribed member volumes over the
  coordinator's socket and spent several minutes in a retry loop publishing
  nothing at all.
- `getVolume` answers `{volume, muted, fixed}` - the same shape `groupVolume:1`
  returns, so one wire type serves both.
- **A group's volume is derived from its members', not stored beside them.**
  Two rooms at 0 and 14 report a group volume of 7; setting one member to 22
  moved the group to 18. So the group slider and the per-room sliders are
  genuinely different controls, and the group one cannot be used to reach a
  single room.
- Muting is deliberately left group-only in x2rock. A single muted member of a
  group is a puzzle to find later, and the group mute is what people mean.
- **A member's socket is opened best-effort and is dropped the same way.** The
  per-member connections exist only to balance one room against another, so
  losing one costs a frozen slider and nothing else. The daemon classifies them
  `Loss::Tolerated` and swallows their `LOST` rather than forwarding it: a
  primary or coordinator going away means every player it feeds is stale and the
  daemon must reconnect, but one flaky portable must never tear down the whole
  household. A member `LOST` that appears to vanish is that rule working, not a
  dropped event.

## Queue mutation over UPnP (verified 2026-08-29)

Read from the players' own service descriptions before sending anything, then
confirmed with deliberately invalid arguments. Nothing was mutated to learn any
of this.

- **`http://<player>:1400/xml/AVTransport1.xml` states the contract.** Fetching
  the service description is a plain GET and answers most of what probing would,
  at no risk. It should be the first move for any new UPnP action.
- **Mutation lives on `AVTransport:1`**, which x2rock already talks to. The
  Sonos-proprietary `Queue:1` service is not needed: `AddURIToQueue`,
  `AddMultipleURIsToQueue`, `ReorderTracksInQueue`, `RemoveTrackFromQueue`,
  `RemoveTrackRangeFromQueue`, `RemoveAllTracksFromQueue`, `SaveQueue`,
  `CreateSavedQueue` and `AddURIToSavedQueue` are all advertised there.
- **The argument is `UpdateID`, not `UpdatedID`** - a plausible-looking guess
  that the description file settles.
- **`RemoveTrackRangeFromQueue` is native**, taking `StartingIndex` +
  `NumberOfTracks` and returning `NewUpdateID`. No client-side descending loop,
  and so no half-removed range to explain.
- **`Browse` returns the queue's `UpdateID`** in the same envelope as
  `TotalMatches`, which `browse_queue` had been discarding.
- **`UpdateID` is strictly enforced, and that is worth knowing.** A *valid*
  index sent with a stale `UpdateID` answers **1028** and removes nothing
  (confirmed: a 756-track queue stayed at 756). So a mutation must read the
  version immediately before sending, not carry one across a user's
  deliberations - otherwise every change after any other edit fails.
- **Mutation faults are 800 and 1028, not 711.** 800 is a position that does not
  exist; 1028 is the version mismatch above. 711 belongs to the *navigation*
  actions. The two are easy to conflate: probing with both a bad index and a bad
  `UpdateID` returns 1028, which makes it look like the index error until the
  cases are separated. `NumberOfTracks: 0` is rejected as 402, so a no-op
  removal is not a way to probe anything.
- **`SaveQueue` wants `Title` and an `ObjectID`** (empty for a new playlist) and
  returns `AssignedObjectID`.

### Content to enqueue, and the token that must travel with it

- **`Browse SQ:`** lists saved Sonos playlists as containers whose `res` is a
  plain `file:///jffs/settings/savedqueues.rsq#N`. Nothing else is needed.
- **`Browse FV:2`** lists favorites, and these are the awkward ones. Each carries
  an `<r:resMD>` blob holding a second, escaped DIDL-Lite document, and inside it
  a `<desc id="cdudn">SA_RINCON51463_X_#Svc51463-0-Token</desc>` - the music
  service's account token. **`EnqueuedURIMetaData` must be that `r:resMD` passed
  through verbatim**, not a DIDL document synthesized from title and URI, or the
  service's own credential is dropped. This is why enqueuing has to carry raw XML
  fragments from a browse rather than flattened fields.
- Note the escaping depth: the DIDL is escaped inside `<Result>`, and `r:resMD`
  is escaped again inside that.
- **`FV:2` and `favorites:1 getFavorites` agree**, at 41 each on re-check the
  next morning. An earlier count of 70 from the Control API could not be
  reproduced and is not recorded as a fact - most likely the household's
  favorites had simply changed between the two readings. Worth re-checking
  before relying on the two being interchangeable, since nothing guarantees it.
- **What can be enqueued is decided by the kind of thing, not by its URI
  scheme** (corrected 2026-08-29 after an earlier reading of this got it wrong).
  `AddURIToQueue` takes anything a player can hold a position in - an individual
  track - and refuses stations and collections. It answers 800 for
  `x-rincon-cpcontainer` playlists and albums and for `x-sonosapi-*` stations,
  and accepts a `musicTrack` on those *same* schemes: a track lifted out of a
  queue and a `TRACK` favorite both add, with their own metadata or with none.

  The first pass concluded "service-backed content cannot be enqueued" because
  every favorite tried happened to be a container or a station. Five of this
  household's favorites are tracks, and refusing them was a bug of that
  reasoning, not of Sonos. The discriminator to use is the `upnp:class` inside
  the item's `r:resMD` - `object.item.audioItem.musicTrack.#TRACK` against
  `object.container.playlistContainer` - because a favorite's own class is
  always `sonos-favorite` whatever it points at.

  Metadata was never the obstacle either way: a hand-extracted 624-character
  `resMD` failed on a container exactly as an empty one did, and a track adds
  with either. For stations and collections, `favorites:1 loadFavorite` -
  which *replaces* the queue - remains the only way to play them.
- **`EnqueueAsNext` on its own still appends.** "Next" has to be named as a
  position in `DesiredFirstTrackNumberEnqueued` - the current track plus one -
  or the tracks land at the end regardless (verified both ways).
- **`ContentDirectory` advertises `CreateObject` / `UpdateObject` /
  `DestroyObject`**, so creating a favorite may be possible after all - the
  Control API's `favorites:1` has no such command. Untested, because proving it
  means actually creating one.

## Favorites over the local API (verified 2026-08-28)

Probed directly against a player on this household, then built on.

- **`favorites:1 getFavorites` works on the LAN with no login**, which was not a given — favorites
  are account-linked content, and the concern was that they would need the cloud API. They do not.
  It is household-scoped, so any player answers it; no coordinator needed.
- **`favorites:1 loadFavorite` is group-scoped** and takes `favoriteId` in the *options*, not the
  command envelope; omitting it fails with `Parsing terminated:[1].favoriteId`, and an id that does
  not exist fails with `ERROR_INVALID_OBJECT_ID`. It also accepts `playOnCompletion`.
- **Only `id` and `name` can be relied on.** Of the 70 favorites on this household, 26 carry no
  service at all, and the resource — and so the content type — is missing from some. Everything
  else must be optional.
- **Favorites are not playlists.** `playlists:1 getPlaylists` is a separate namespace with separate
  content (Sonos playlists, with a `trackCount`); a name in one is not a name in the other.
- **A reply can exceed one WebSocket frame.** `getGroups` and `getFavorites` both arrive
  fragmented, as an initial frame plus continuations with only the last carrying FIN. Anything
  hand-rolling this transport has to reassemble; `tokio-tungstenite` already does, which is why
  x2rock never had to care.

## Desktop-bus facts learned wiring up restarts (2026-08-28)

All observed against this laptop while testing real suspends and WiFi cycles.

- **logind is on the system bus, NetworkManager too.** Neither shares the session bus connection
  the MPRIS servers use, so the daemon holds two more.
- **A zbus `PropertyStream` opens by yielding the value it already has** — documented, and the
  cause of a spurious reconnect ~2s after every daemon start until the value was *compared* rather
  than counted. Skipping the first item instead would be wrong: the opening yield only happens if
  the property cache is already populated, so in the un-cached race the first item is a real change.
- **NetworkManager climbs `CONNECTED_LOCAL` → `SITE` → `GLOBAL`** as its connectivity check
  completes. Firing on "state is connected" therefore fires up to three times per arrival; fire on
  the *transition into* connected instead.
- **Losing the network is not worth acting on.** Both a `StateChanged` down and a
  `PrimaryConnection` cleared to `/` arrive when WiFi drops, and reconnecting then only produces a
  retry storm against a network that is not there. Worse, it leaves the daemon in a backoff loop
  that races the real arrival trigger, and both land — republishing, and flapping every bus name,
  twice. Only arrival is a reason to reconnect.
- **The global state stays `CONNECTED_GLOBAL` while the route out changes underneath.** Docking
  and a VPN coming up move `PrimaryConnection` without moving the state, so watching the state
  alone misses exactly the cases that change the local address.

## Protocol facts learned while building the daemon (2026-08-28)

All verified against the One SL; each one shaped the transport's design.

- **`cmdId` is echoed** in reply headers, so replies can be matched to callers regardless of
  order. **Except** when the player fails to parse the command at all - then the reply has no
  `cmdId`. The deliberately-invalid `{}` household probe hits exactly this case, so the transport
  falls back to arrival order for id-less replies, and discards a reply whose id nobody awaits.
- **Replies carry `success`; events never do.** That is the discriminator between the two.
- **`groups` events omit `playbackState`** from each group and carry a `partial` flag; only
  `getGroups` replies include playback state. Wire types must default it. The initial snapshot
  after `subscribe` describes the topology already known - republishing MPRIS for it flaps every
  bus name once per connection.
- **The player answers WebSocket Ping with Pong**, so ping-based liveness works. The daemon pings
  every 30s and treats 90s of total silence as a dead socket.
- **Subscription snapshots arrive as events immediately after the `subscribe` reply**, in the
  order: reply, then one event per namespace. Listen for events *before* subscribing or the
  snapshot is missed.
- **`track.imageUrl` is served by the player itself** (`http://<ip>:1400/getaa?...`) and works
  directly as MPRIS `mpris:artUrl`.

## Carried-over knowledge from the Kotlin project (x2rocktv)

Real lessons from building and running the Kotlin CLI in production on this exact use case — don't
rediscover these the hard way:

- **Sonos accounts can have more than one household**, and the API gives households no name of
  their own — only opaque IDs. A household with a single standalone speaker, set up separately
  from the rest of a system, is common enough to design for from day one. The Kotlin CLI originally
  only queried the first household `.households.firstOrNull()`-style and silently couldn't see a
  real, existing room. Local discovery makes this easier than it was: each player reports its own
  `householdId`, so enumerate players and group them, rather than picking "whichever household
  comes first."
- **CLI flag ordering matters more than it seems.** A root-level flag (equivalent to the Kotlin
  version's `-r`/`--room`, `-H`/`--household`) needs to work whether typed before or after the
  subcommand name (`x2rock -r Office next` and `x2rock next -r Office` should both work). `clap`
  supports global args that propagate into subcommands more naturally than Clikt did — verify this
  early rather than discovering the gap after users hit it, as happened here.
- **One daemon process per room was the Kotlin version's design, and it's a real weakness** — N
  rooms means N long-running processes, each polling independently. Multiplex several rooms as
  several D-Bus bus names from one process instead. This is now easier: one WebSocket per player,
  all in one async runtime, all push-driven.
- **~~Everything polls the REST API on a fixed interval~~ — SOLVED.** The 5s polling loop was the
  Kotlin version's biggest architectural weakness. The local WebSocket's `subscribe` gives real
  push events, so the Rust version should never poll. This was flagged as the single
  highest-leverage difference the rewrite could make; it is now settled and cheap.
- **`x2rock login` is no longer required for first-time setup.** It becomes optional, only for
  off-LAN control. If it is ever implemented, keep the `--manual` fallback (paste the redirect URL)
  for environments where a URL scheme handler can't be registered.
- **A rebuild wipes any "build output" directory** — install the built binary to a stable path
  (`~/.local/bin` or `~/.local/share/x2rock` + symlink) as part of the normal dev loop, don't
  reference `target/release/...` directly from systemd units or Quickshell plugin configs.
- Config storage: `$XDG_CONFIG_HOME/x2rock/` (or Rust equivalent via the `directories` crate),
  file mode 0600. **This directory is now free.**

  It previously collided: the Kotlin CLI's `Xdg.configDir` resolved a hardcoded `"x2rock"`, so
  renaming that repo to `x2rocktv` moved the source tree but not the config path. Resolved
  2026-08-28 — the Kotlin CLI was changed to `$XDG_CONFIG_HOME/x2rocktv/` (commit `cd60aa4` in
  `x2rocktv`) and the old directory deleted, so the Rust tool can use `x2rock/` without
  reservation and needs no migration path.

  Still shared, and worth not breaking:
  - `X2ROCK_ROOM` and `X2ROCK_HOUSEHOLD` remain the Kotlin CLI's env vars. If the Rust tool reuses
    them they should mean the same thing. `X2ROCK_PLAYER` (player address) is new and unclaimed.
  - `SONOS_CLIENT_ID`/`SONOS_CLIENT_SECRET` remain the Kotlin CLI's OAuth overrides. Irrelevant
    here while OAuth is cut.
  - **The MPRIS bus name is an unresolved collision.** The Kotlin daemon publishes
    `org.mpris.MediaPlayer2.x2rock` by default (overridable with `--bus-name`). The Rust daemon
    will want the same name, and two processes cannot hold one bus name. Decide before the MPRIS
    slice: either the Rust tool takes the plain name and the Kotlin one is considered retired, or
    it picks a distinct suffix. Note the Kotlin default is baked into any existing bar config.
  - `x2rock://` (the OAuth callback URL scheme), the `x2rock` binary name and
    `$XDG_RUNTIME_DIR/x2rock` still belong to the Kotlin CLI. Only the runtime dir could collide,
    and only if the Rust tool ever gains OAuth.

## Rust ecosystem notes

- **WebSocket client**: `tokio-tungstenite`, with a `rustls` certificate verifier that accepts the
  speaker's self-signed cert. This is the main new dependency the revised design needs.
- **MPRIS server**: use **`mpris-server`** (built on `zbus`) — the modern, maintained crate for
  *exposing* an MPRIS interface (server-side, which is what x2rock needs). Don't use
  `mpris`/`mpris-player` — those are client-side and/or unmaintained.
- **Sonos client**: still write from scratch. Crates on crates.io (`sonos-api`, `sonor`,
  `sonos-sdk`, `wez-sonos`, `sonos.rs`) all target local UPnP/SOAP. That is now *partially*
  relevant — they may be worth reading for the `ContentDirectory` browse path — but none of them
  speak the LAN WebSocket Control API, which is the spine of this design.
  - `ronor` (github.com/mlang/ronor) targets the cloud Control API with OAuth. Since the local API
    uses the same namespaces and command shapes, it remains useful prior art for endpoint shapes.
    Maintenance status unconfirmed — reference, not a dependency.
- **OAuth / HTTP client**: not needed for v1 at all. Defer entirely.
- A ~60-line dependency-free Python reference implementation of the LAN WebSocket client (handshake,
  framing, command/subscribe) was written during this investigation and is a direct model for the
  Rust port.

## Open questions

1. **Music service search — now a goal, not a non-goal** (reopened 2026-08-31). The transports and
   the discovery half are settled and written up under "Music service search, reopened"; what is
   open is the credential. Work it in the order given there: a raw Control-API command in the CLI,
   then re-probe `musicService:1 search`, and only if the player will not search for us go after a
   controller-side SMAPI credential. Do not start with the SMAPI client — that is the expensive
   branch and it may turn out to be unnecessary.

2. Whether to wire x2rock's widget to `omarchy.media`'s service — either pinning the bar pill to a
   room via `selectPlayer()`, or the reciprocal read that marks which room the pill is showing. The
   mechanism is confirmed to exist (see "Bar-widget interop with `omarchy.media`" above); what is
   not confirmed is whether it behaves with several rooms on the bus, since only one room was
   reachable when it was written up. **Pick this up from the home household.**

3. Upstream Quickshell docs (not just Omarchy's usage of it) — worth a direct look before
   committing to only the three integration patterns Omarchy's plugin README documents. Much less
   pressing than it was: a working widget now exists, and the Quickshell behaviours that actually
   cost time are written up above rather than left to be rediscovered.

## Resolved since the original draft

- ~~Music search is out of scope~~ — **decided 2026-08-29, reversed 2026-08-31.** The 08-29 entry
  gave two reasons. The first, that search "needs SMAPI, with per-service endpoints and
  authentication", was wrong: it read *service* authentication as a *Sonos account*, when a service
  is linked to the household and the LAN gives up the endpoint for free. The second — that
  `AddURIToQueue` refuses service-backed containers and stations, so a search might have had
  nothing it could enqueue — is still unrefuted, and the cheap experiment it named (find one
  service *track* and try to enqueue it) is still the right test; it has simply never been run.
  The entry is kept here rather than deleted because the way it went wrong is worth remembering:
  a single unexamined word in a rationale closed a feature for two days. See "Music service search,
  reopened (verified 2026-08-31)".

- ~~Is a bespoke Quickshell widget worth building in v1?~~ — built, 2026-08-28/29, and it went
  further than the question imagined: per-room volume, transport, favorites with a type-to-filter
  picker, grouping and party mode, and cover art. What the question got wrong was calling it a
  stretch goal; the widget is where a household that is not the author actually uses this.

- ~~Cloud Control API is the only integration path~~ — false; the LAN WebSocket API is better.
- ~~Build WebSocket from day one, or ship polling v1 first?~~ — settled: push from day one, and it
  costs *less* than polling, not more.
- ~~Carry over `docs/sonos-control-api.md` from the old repo~~ — those files are saved HTML dumps of
  Sonos's readme.io pages, not clean references. The namespace/command reference is better taken
  live from `docs.sonos.com`, which applies to the local transport too.
- ~~Is content browsing needed, or is favorites + transport enough?~~ — settled 2026-08-28: queue
  navigation is a core requirement, so UPnP is mandatory. This reverses the earlier "optional
  second transport" framing.
- ~~Is off-LAN control wanted?~~ — no, not for now. Cloud OAuth cut from v1, seam retained.
- ~~Is multi-household a day-one requirement?~~ — no. Local devices come first. The distinction may
  return when dealing with accounts logged in to Sonos via their API; keep household IDs plumbed
  through (they come free from `getGroups`) but build no household-switching UX for v1.
- ~~Always-on machine or laptop that sleeps?~~ — laptop, and users are expected to be road
  warriors. Network mobility is now a design driver rather than an edge case; see "Connection
  lifecycle and network mobility".
- ~~Queue navigation needs building over UPnP~~ — done 2026-08-28. `x2rock queue` / `play N` over
  AVTransport + ContentDirectory on 1400. Verified: `Browse Q:0` lists it, `Seek TRACK_NR` jumps
  (error 711 out of range, 701 when the queue is not the current source), `GetMediaInfo` reports
  `x-rincon-queue:` when the queue is the active source. The CLI switches the source with
  `SetAVTransportURI` before seeking, since after a radio stream the queue is not current.
