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

## Search, as built (2026-08-31)

`x2rock search` exists. `x2rock search` alone lists what can be searched, `-s <service>` alone
lists that service's categories, and a term searches. `--play N` plays the Nth hit.

```
$ x2rock search
32 of 108 services can be searched without an account:
  ... Hype Machine, SomaFM Radio, TuneIn, ...

$ x2rock search -s tunein --count 5 jazz
s249973        stream     Smooth Jazz  Smooth Jazz
s328971        stream     One Jazz  Jazz
...
5 of 93 on TuneIn.

$ x2rock search -s somafm --count 5 --play 3 ambient
Media Room — Deep Space One on SomaFM Radio
```

### How it is put together

- **`sonos/http.rs`** — the HTTP/1.1 client, lifted out of `upnp.rs` and given TLS. One client for
  both directions: plain to a player on 1400, TLS to a service on 443. `tokio-rustls` and
  `webpki-roots` were **already in the lock file** via `tokio-tungstenite`, so this cost no new
  third-party crate. Timeouts belong to the caller, because 8s against a player on the same switch
  and 6s against a service in another country are different budgets.
- **`sonos/smapi.rs`** — the SMAPI client. Parses the descriptor list, reads categories out of the
  manifest and presentation map, and does `search` and `getMediaURI`.
- **`upnp.rs::list_services`** — `ListAvailableServices`, the one LAN call search needs.
- **`main.rs`** — the `search` command, and nothing else touches any of it.

### Things worth knowing, learned building it

- **One crypto provider, named explicitly.** `rustls::ClientConfig::builder()` panics at runtime
  when more than one provider is compiled in and none is installed as the process default — which
  is exactly this binary now that `tokio-tungstenite` and `tokio-rustls` each bring one. Both
  `local.rs` and `http.rs` name `aws_lc_rs` through `builder_with_provider`. This would not have
  shown up until the first TLS handshake.
- **Certificates are validated here, unlike the player socket.** `local.rs` accepts any
  certificate because a player presents a self-signed one for its own IP and the transport never
  leaves the LAN. A music service is a public host with a real chain, and this call does leave the
  LAN, so it gets real roots.
- **No XML declaration, no BOM.** Already recorded, and the client now strips a leading BOM from
  every response too, because services send one and every XML parser then rejects it as content
  before the declaration.
- **Two services, two shapes.** TuneIn's categories are `stations`/`podcasts` mapping to
  `search:station`/`search:show`; SomaFM's differ. The mapped id is what goes on the wire and the
  plain id is what a person types. Nothing about the code is TuneIn-shaped, which was worth
  proving with a second service before believing it.
- **Playing a hit uses a session, not the queue.** `createSession` then `loadStreamUrl`, exactly
  as the sample app documents. Verified that the queue is **untouched**: `x2rock queue` shows the
  same single track before and after, while the room plays the stream.
- **Transport and volume keep working against a session source.** `pause`, `play` and `vol` all
  act on it normally. Note that pausing a live stream reports `IDLE` rather than `PAUSED` — Sonos
  stops a live stream rather than holding a position in it, which is correct behaviour and not a
  bug to chase.

### The isolation, checked rather than asserted

The rule above says the daemon must never acquire an internet timeout. That is now structural and
can be grepped:

```
$ grep -rln "http::get\|Endpoint::Web" src/     # who can leave the LAN
   src/sonos/http.rs
   src/sonos/smapi.rs
$ grep -n "smapi\|http::get\|Endpoint::Web" src/daemon.rs src/mpris.rs
   (nothing)
```

`smapi` is reachable only from `main.rs`'s `search` arm. The daemon and the MPRIS server cannot
reach the internet, so nothing that reaches the widget over MPRIS can be delayed by a service.

### The catalogue cache (added 2026-08-31)

`src/catalogue.rs`, cached in `$XDG_STATE_HOME/x2rock/services.json` alongside the player list.
A cold search cost ~950ms and three round trips before any query ran; warm it is ~420ms.

- **Invalidation is a real signal, not an expiry.** `ListAvailableServices` returns
  `AvailableServiceListVersion` in the *same reply* as the descriptors — here
  `RINCON_…:58`, the same 58 that `musicServicesChanged` reports as `availableServicesVersion`.
  So the cheap LAN call decides whether the expensive internet ones can be skipped, and no TTL had
  to be guessed. Category lists are keyed to the catalogue version rather than to a service id,
  because a presentation map can change while an id does not.
- **A player is wanted, not required.** This is the part worth keeping: the first attempt put the
  cache behind `session::connect`, so with the household unreachable `x2rock search` failed at the
  connection and the fallback inside the cache was dead code. A cache that fails whenever the thing
  it caches is unavailable is not doing its job. `search` now runs before the connection, tolerates
  its failure, and only `--play` — or a first run with nothing cached — insists on a player:

  ```
  $ X2ROCK_PLAYER=<unreachable> x2rock search
  x2rock: no player reached, using the cached catalogue (…Connection refused…)
  32 of 108 services can be searched without an account: …

  $ X2ROCK_PLAYER=<unreachable> x2rock search -s tunein --play 1 jazz
  Error: no player to play it on: …
  ```
- **A corrupt cache reads as empty rather than failing.** Unlike the player list, it is wholly
  regenerable, and refusing to search over a malformed cache file would be the cache causing the
  outage it exists to prevent.
- **A name resolves by exact match, then by *unique* prefix.** "radio" matches a dozen services on
  this catalogue; picking whichever sorted first would be worse than naming the alternatives.
  An exact match still wins over an ambiguous prefix, so a service actually called "Radio" resolves.

### Search in the widget (2026-08-31)

The favorites picker now searches too. Typing filters the favorites as before; with a term typed
and nothing sent yet, a **`Search TuneIn`** row appears at the end of the list, and choosing it
runs the query. Hits land in the same list under the same delegate.

Four decisions in it:

- **One list, not two.** Favorites and hits answer the same question — what should this room play —
  and a person filtering for something they own should not have to decide in advance whether they
  own it. This is why `search --json` was changed to emit the *same field names* as
  `favorites --json` (`name`, `type`, `description`, `art_url`): the widget concatenates rather
  than translates, and one delegate renders both.
- **The search row is an action, not a result.** Nothing is sent until it is chosen. Searching on
  every keystroke would put a network round trip behind typing, which is the behaviour this widget
  exists to avoid.
- **A reply belongs to the term that asked for it.** The field can be edited while the subprocess
  runs, so the term in flight is kept in `pendingTerm` and results are only shown while the typed
  text still matches `searchedTerm`. Without that, a slow answer surfaces under a different word.
- **Failure is one line, never an empty list.** A non-zero exit sets a status string shown in the
  count position at the top right, leaving the rows already on screen up. Empty stdout is a
  failure; `[]` is a real answer meaning the service has nothing. Copied from the favorites picker,
  which already had all of this right.

`searchService` in `shell.json` picks the service (default `TuneIn`) and `""` turns the feature
off, leaving the picker exactly as it was with no network call behind it. `searchCount` sets how
many hits to ask for.

Playing a hit uses **`x2rock play-item -s <service> <id> --title <name>`**, added for this:
`search --play N` would re-run the query to find the Nth result, costing a second round trip and
risking a different item if the service reordered. The widget already holds the id.

**Verified in the widget**, by hand: typed a term, chose the search row, played "Texican Radio T"
into the Media Room from the picker, and watched it come back through the daemon to MPRIS. The
whole chain — descriptor list, manifest, presentation map, SMAPI `search`, `getMediaURI`,
`createSession`, `loadStreamUrl`, MPRIS — now runs from one keystroke in a bar popup.

It took a fix to get there, and the bug is the interesting part. **A status line must not pre-empt
the list it describes.** `pickerStatus` keyed its visibility off `favoritesStatus !== ""`, and the
list was bound to `!pickerStatus.visible`. On a household with no favorites that status is
permanently set, so the entire list was hidden — search row included — and typing did nothing
visible. The status now shows only when there are genuinely no rows, and what it had to say moves
to the count line while the list is up, so a real favorites *failure* is still reported rather than
swallowed by the rows that survived it.

Two things worth carrying forward from that:

- It is the same shape as the cache bug: a fallback that fires whenever the thing it describes is
  unavailable, taking the working part down with it. Copying the favorites picker's status handling
  faithfully was right up until a row appeared that did not come from favorites, which broke the
  assumption the handling was built on.
- **It could only be caught on this household.** With the home household's 41 favorites,
  `favoritesStatus` is empty and the bug never appears. The office household — three favorites the
  Control API does not report — is the worse fixture and therefore the better test.

Driving the picker open from a script still does not work: `wtype` does keys only, Hyprland has no
click dispatcher, and auto-opening from `Component.onCompleted`, a `Timer` and an
`onPopupOpenChanged` hook all failed to produce a visible panel. The pickers appear to need a real
interaction, not just `pickingFor` being set. Worth knowing before anyone tries to test this widget
without hands.

### Still to do

- Paging. `search` takes `index` and the CLI always sends 0.

## YouTube Music without a Sonos account: what the player will hand over (2026-08-31)

Probed while the household played a YouTube Music album started from a phone that is **not signed
in to a Sonos account**. That detail is the whole point: the service is linked to the *household*,
and every controller on the LAN inherits it.

### The full MusicObjectId is readable, with no credential

`playbackMetadata:1 getMetadataStatus` returns, for the container and for the current track:

```json
"id": { "_objectType": "universalMusicObjectId",
        "accountId": "sn_3", "serviceId": "284",
        "objectId": "ALkSOiGTPQu20Hqb6iEmeMhGFI_jhhXgHyx7WTjmO6bs1i3H" }
```

`accountId` is `sn_3` — the same account serial that appears as `sn=3` inside the player's own
`x-sonosapi-hls-static:` URIs. So the triple that `createSession`'s optional `accountId` and
`loadCloudQueue`'s `trackMetadata.id` both want is simply *there*, for the asking, on a household
nobody has logged into.

Also confirmed on the phone: the app's search offers exactly **Sonos Radio** and **YouTube Music**
— the household's registered set, and the enumeration no API would give up. And its favorites list
is empty, which is what settled the `FV:2` shortcut question above.

### A service *track* can be enqueued. The 2026-08-29 question is answered.

That entry said `AddURIToQueue` refuses service-backed containers and stations, and that whether an
individual **track** fares better "was never testable here, since every favorite on this household
is a container or a station". A phone-started album put one in the queue, so it became testable:

```
AddURIToQueue
  EnqueuedURI          x-sonosapi-hls-static:ALkSOiGTPQu2…?sid=284&flags=65544&sn=3
  EnqueuedURIMetaData  (empty)
→ FirstTrackNumberEnqueued 2, NumTracksAdded 1, NewQueueLength 2
```

**Accepted.** So the refusal is about the *kind* of content, not about the service: containers and
stations no, tracks yes. And the URI carries its own account (`sn=3`), which is why no metadata was
needed for the player to take it.

### But empty metadata cost the queue its titles

Immediately afterwards, `Browse Q:0` returned **no `dc:title` for either item** — including the
original, which had shown "Bodies" minutes earlier. Removing the added track did not bring it back,
and it had not returned several seconds later. `x2rock now` still shows the title correctly, since
that comes from `playbackMetadata` rather than the queue, so the damage is confined to the queue
listing.

This is caused by the add, on the evidence — it was the only mutation between the two reads —
though it cannot now be proved, since the first read is gone. Two things follow:

- The 2026-08-28 insistence that `EnqueuedURIMetaData` must carry the item's `r:resMD` **verbatim**
  is right, and stronger than it looked: the player accepts an add without it and then has nothing
  to display, for the whole queue rather than just the new row.
- A YouTube Music queue item carries **no `r:resMD` at all** — the field the favorites path relies
  on simply is not there to copy. So "enqueue a service track properly" is not solved by copying
  what the queue already holds. Whether a synthesized DIDL with the `universalMusicObjectId` triple
  in it would satisfy the player is the next question, and it is untested.

### Both open questions closed: it plays, and metadata fixes the titles (2026-08-31)

Re-run with the room at volume 0. A **synthesized** `EnqueuedURIMetaData` — no `r:resMD` to copy,
so one was built from scratch — with the service's cdudn in it:

```xml
<item id="00032020ALkSOiGTPQu2…" parentID="-1" restricted="true">
  <dc:title>Bodies</dc:title><dc:creator>Offset, JID</dc:creator>
  <upnp:class>object.item.audioItem.musicTrack</upnp:class>
  <desc id="cdudn" nameSpace="urn:schemas-rinconnetworks-com:metadata-1-0/"
    >SA_RINCON72711_X_#Svc72711-0-Token</desc>
</item>
```

- **The title came back**: the queue row reads `Bodies — Offset, JID  2:59`. The duration was never
  sent, so the player resolved it through the cdudn — the metadata is not merely being echoed, it
  is being *used* to reach the service.
- **It plays.** `x2rock play 2` moved the cursor to our row and `positionMillis` advanced
  19004 → 23239 across four seconds. Streaming, not just a hopeful `PLAYING`.

**The cdudn is derivable, not scavenged.** `SA_RINCON<N>_X_#Svc<N>-0-Token` where `N` is the
service type from `ListAvailableServices`'s `AvailableServiceTypeList`: YouTube Music is 72711
(= 284 × 256 + 7), and the same arithmetic reproduces the `SA_RINCON77575` that Sonos Radio's
favorites actually carry (303 × 256 + 7). So a service's cdudn can be computed for any service in
the list, from data the player hands out unauthenticated.

### So this works, today, with no account

```
objectId (from playbackMetadata, or anywhere it can be had)
  → x-sonosapi-hls-static:<objectId>?sid=<serviceId>&flags=65544&sn=<account serial>
  → AddURIToQueue with a synthesized DIDL carrying SA_RINCON<type>_X_#Svc<type>-0-Token
  → the room plays it
```

x2rock never sees a token. The player resolves the credential it already holds, exactly as it does
for a favorite. What is missing is only **discovery** — a way to learn an `objectId` for something
not already playing — and that is what search would have provided.

Which makes the feature concrete: **remember what played, and start it again.** Every id needed is
readable while something plays; storing them is a local matter. Discovery stays with the Sonos app,
repetition moves to the bar.

### Built: `keep` / `bookmarks` / `bookmark` (2026-08-31)

`src/bookmarks.rs`, stored in `$XDG_STATE_HOME/x2rock/bookmarks.json`. `keep` reads the playing
track's `universalMusicObjectId` from `playbackMetadata`, `bookmark` rebuilds the URI and a
synthesized DIDL and enqueues it. Verified against YouTube Music: kept while the album played,
then replayed from the bookmark alone with `positionMillis` advancing 14740 → 19016.

Two things it taught, both worth more than the feature:

- **The cache had no schema version, and that is a real design fault.** Adding `service_type` to
  the cached `Service` left every existing file deserializing *cleanly* — `Option` defaults to
  `None` — while still matching the player's `AvailableServiceListVersion`, so nothing refetched
  and every cdudn was underivable. The invalidation key answered "has the catalogue moved?" and
  never "do we still read it the same way?". `Catalogue` now carries a `SCHEMA` constant and
  `load()` discards a file that does not match, which is a rule any on-disk cache in this project
  should follow. Pinned by a test built from the exact file that broke.
- **Two failures were wearing one message.** "Service 284 is not in this player's service list" was
  reported for a service that *was* in the list but had no type to derive a cdudn from, which sent
  the debugging in the wrong direction. Split.

`from_id` refuses what cannot be replayed rather than storing it: `objectId: "-1"` (what a player
reports for a live stream's notional container), a missing `serviceId`, a missing `accountId` —
each with its own reason, because each means something different went wrong.

### In the widget (2026-08-31)

Kept items join the picker beneath the household's favorites — a flat list, since both answer "what
should this room play" and the CLI emits them with the same field names. A **keep** glyph sits in
each room's switch row, next to the TV input rather than with the grouping pair, because
remembering what is playing is about this room's own source. It dims when there is no title to hang
a name on, since the CLI refuses a live stream and a button that looks available and silently does
nothing is worse than one that looks unavailable.

`bookmarksProc` is quieter on failure than `favoritesProc`: an empty bookmark list is the normal
state until someone keeps something, so a non-zero exit leaves the section absent rather than
claiming an error.

### The daemon remembers what plays (2026-08-31)

`keep` requires remembering to press something, which is exactly what a person listening to music
does not do. So the daemon notes every track that has a real id, and `x2rock bookmarks --all`
includes them. Kept entries are *pinned*: they never expire, they sort first, and they carry a `*`
in the listing.

One store, not two. "Recently played" and "saved" are the same list with a flag, which means one
file, one command set, and `keep` on something the daemon already saw promotes it rather than
adding a duplicate.

**The write must never be able to break a room.** This is the first state the daemon has ever
written, and it is a convenience sitting inside the process whose job is transport. So `remember()`
has no `?` in it: every failure is logged and swallowed. Verified by making the store unreadable
(`chmod 000`) and confirming `pause`, `play` and `vol` all still worked and the unit stayed active.

Cheap by construction, too: `note()` returns whether anything changed, so a track playing for four
minutes writes once, not once per event. A pinned entry keeps its name and its pin — only the
timestamp moves — because someone named it deliberately and the daemon must not rename it back.
That one is pinned by a test.

**User data migrates; caches get discarded.** The opposite of the rule for the service catalogue,
and deliberately: `pinned` defaults to *true* when absent, because anything written before the flag
existed got there by someone running `keep`. Discarding a file this program no longer understands
is right for something refetchable in a second and wrong for the only copy of what a person saved.

Several daemons write this file if a household runs one unit per room, as the home one does. The
window is smaller than it first looks — `remember()` loads, notes and saves each time rather than
holding a copy — so two writers must interleave inside a few milliseconds, and the atomic rename
means the worst case is a lost timestamp rather than a corrupt file. Not worth a lock on that
evidence; worth revisiting if entries actually go missing.

### Two things learned keeping an album (2026-08-31)

**`keep --container` is only meaningful while a real container is playing.** After x2rock replays a
single track, the player reports the *container as the track* — `container.type: "track"`, with the
track's own object id. That is not a bug in either place, but it means the useful moment to keep an
album is right after starting it from the Sonos app, before anything else has been queued.

**`keep` was blanking what it did not know.** Keeping the container of a track already kept arrived
with no artist — containers carry none — and overwrote the artist already stored. An update that
silently downgrades saved data is worse than one that fails, so `keep` now fills each display field
from the existing entry when the new one has none, while a genuinely new value still wins. Two
tests, one for each direction.

**Also observed, unexplained:** the account serial moved. The phone-started album reported
`accountId: "sn_3"`; after x2rock enqueued a track built with `sn=3`, the player began reporting
`sn_2` for the same content. Playback is unaffected either way, but a stored serial may not be as
stable as it looks, and a bookmark that stops working is the symptom to expect. Worth watching on a
household with more than one account on a service.

### An intermittent enqueue failure, observed not explained

`AddURIToQueue` returned **UPnP 800, "no such position in the queue"**, once, for a call that
succeeded on each of the next three attempts with no change. It is the same flavour as the Sonos
app failing to start this album the first time and working seconds later — the household reports
losing its connection to YouTube Music now and then.

No retry has been added. 800 nominally means the add did not happen, so retrying would be safe, but
"nominally" is not enough to build on and one observation is not a pattern. Worth watching: if it
recurs, the question is whether the player or the service is the one dropping out, which
`playbackStatus` events during the failure would settle.

One caution for whoever builds it: the earlier add with *empty* metadata blanked the titles of the
whole queue and they did not come back. Always send a metadata document.

### `match` wants a link code

With all three required fields present but an unknown `userIdHashCode`, `musicServiceAccounts:1
match` answers:

```
ERROR_COMMAND_FAILED — "Link code required to add guest account"
```

So `match` is not a lookup we lack a hash for; with an unrecognised hash it tries to *add* an
account and wants the `linkCode` from the browser link flow. That confirms the chain from the
player's side: `getDeviceLinkCode` → user authenticates → `getDeviceAuthToken` returns
`userIdHashCode` and the link code → `match` registers the account and returns its id.

### Where this leaves YouTube Music

Search stays closed — `music.googleapis.com` answers 403 without Sonos's own encrypted API key, and
that key is not something to go after. But the ids are readable and a track will enqueue, so the
shape of a feature x2rock *could* have is: **remember what played and start it again**, referencing
content by `objectId` and letting the player resolve the credential it already holds. Discovery
stays with the Sonos app; repetition moves to the bar.

What has to be settled first, in order:

1. Does an enqueued service track actually **play**? Cheap, needs one queue add and a `play 2`.
2. Can a **synthesized** `EnqueuedURIMetaData` restore the titles, given there is no `r:resMD` to
   copy? This is the one that decides whether the feature is usable or merely possible.
3. Does `createSession` with `accountId: "sn_3"` plus `loadCloudQueue` work as an alternative that
   sidesteps the queue entirely — at the cost of x2rock serving HTTP the players can reach?


## `FV:2` carries shortcuts; `getFavorites` does not (settled 2026-08-31)

Recorded on 2026-08-28 as agreeing at 41 each on the home household, then apparently contradicted on
the office household:

- `favorites:1 getFavorites` → `{"items": []}` — **empty**.
- UPnP `Browse FV:2` → **three** items: "Discover Sonos Radio", "Sonos Presents", "Trending Now".

They do not actually disagree about favorites. All three of those carry
`<r:type>shortcut</r:type>` and an **empty `<res>`**, and their `r:description` is "Sonos Radio":
they are the service's own navigation entries, not saved favorites. The Control API is right to
omit them.

**Confirmed against the Sonos app**, which is the tie-breaker: on iPhone and Android the favorites
list for this household shows *empty*. Nobody saved anything here, and the app does not present the
shortcuts as favorites either. So `x2rock favorites` printing "No favorites." was correct all
along, and the earlier note guessing at "defaults a service contributes" was directionally right
but had no mechanism.

The real defect was elsewhere. `x2rock queue sources` browses `FV:2` over UPnP and was listing all
three as `favorite play` — offering things with no resource, which then failed a step later with
`"Trending Now" has nothing to play`. `BrowseItem` now carries a `shortcut` flag taken from the
`r:type` marker, and the sources list drops them, so both paths agree on nothing-to-play.

Two details worth keeping:

- **The marker decides, not the missing `res`.** A real favorite whose content the service resolves
  can also arrive without one, and filtering on `uri.is_none()` would lose things that do play.
- An empty `<res></res>` parses as *no text*, so `uri` is `None` rather than `Some("")` — which is
  why `uri` alone could never have told the two apart. Pinned in a test.

### What the app's search list says about this household

Also from the phone: tapping search offers **Sonos Radio** and **YouTube Music**, and nothing else.
That is the household's *registered* set — the answer to the enumeration question this document has
been unable to get out of any API. Worth noting the asymmetry it exposes:

- The Sonos app searches the services the household has **linked** (2 here).
- `x2rock search` searches the services that need **no** linking (32 here).

The two sets do not overlap at all. x2rock offers more search than the app does on this household,
just not the two the household actually uses. Which is the whole case for the account-linking work,
and the whole reason YouTube Music being closed matters.

## Linking an account: what the browser flow actually is (verified 2026-08-31)

The question was whether a Linux desktop can link a music service account gracefully — no embedded
browser, no Sonos partner registration. **For services that offer device linking, yes, and it is
better than an OAuth popup.** Proven against Bandcamp in one call.

### A correction first

An earlier note here said `musicServiceAccounts:1 match` needs a `userIdHashCode` that "only the
service's own SMAPI server can compute", and used that to argue a controller could never register
an account. That is wrong. [`getDeviceAuthToken`](https://docs.sonos.com/docs/add-browser-authentication)
returns **`authToken`, `privateKey` *and* `userIdHashCode`** to whoever completes the link. The
field is handed to the controller by design — it is the controller that later calls `match`.

### The flow, and who does what

Per [Add browser authentication](https://docs.sonos.com/docs/add-browser-authentication), the
**controller** drives it; the player only confirms the account afterwards:

1. Controller calls `getDeviceLinkCode` (device-link services) or `getAppLink` (app-link services).
2. Service returns `regUrl`, `linkCode`, and `showLinkCode` — the last saying whether the user must
   type the code or whether it is already embedded in the URL.
3. Controller opens `regUrl` in a browser. On Linux that is `xdg-open`, in whatever browser the
   person already uses — **no embedded Chromium, no webview, nothing to bundle.**
4. Controller polls `getDeviceAuthToken` for up to seven minutes. Pending is a SOAP fault with
   `faultcode Client.NOT_LINKED_RETRY` and `SonosError` 5 — a fault that means "keep waiting", not
   a failure.
5. On success: `authToken`, `privateKey`, `userIdHashCode`.

No Sonos partner registration appears anywhere in it, and no device certificate.

### Verified: Bandcamp handed over a link URL immediately

One `getDeviceLinkCode`, with a credentials header containing nothing but
`<deviceProvider>Sonos</deviceProvider>`:

```
regUrl        https://bandcamp.com/login?sonos_link_code=7083934b166ea10c27f9495e71fa6a8b
linkCode      7083934b166ea10c27f9495e71fa6a8b
showLinkCode  false
```

`showLinkCode: false` because the code is already in the URL — so the whole interaction is: open a
link, log in, done. That is the graceful flow, and it exists today.

### But it is per-service, and YouTube Music is not one of them

The 108 services split three ways by `<Policy Auth>`: **32 Anonymous** (working now), **14
DeviceLink**, **62 AppLink**.

The 14 device-link services are the tractable next tier: AccuRadio, Bandcamp, Classical Archives,
Deezer, FIT Radio, iHeartRadio, Mixcloud, Murfie, NhacCuaTui, Saavn, Sonos Backgrounds, Sonos
Radio, TIDAL, Tribe of Noise. Not all answered the probe — iHeartRadio and Deezer returned nothing
to a minimal `getDeviceLinkCode`, so the request needs more than the household id and that is a
per-service detail to work out.

**YouTube Music answers `getAppLink` with HTTP 403:**

```json
{"error":{"code":403,"status":"PERMISSION_DENIED",
  "message":"Method doesn't allow unregistered callers (callers without established identity).
             Please use API Key or other form of API consumer identity to call this API."}}
```

Two things follow. App-link services expect the Sonos app to *launch the service's mobile app* —
there is no YouTube Music desktop app to hand off to, so Linux would be on the browser fallback,
which a service need not offer. And Google gates the endpoint on an API key before the question of
user auth even arises.

Notably the key is not secret from us: YouTube Music's **manifest**, fetched anonymously from
Sonos's CDN, carries an `apiKey` object with `cr` and `zp` values. So the 403 is probably
answerable. Whether x2rock *should* present a key Sonos distributes for its own clients is a
judgement call and not a technical one — it is a different act from completing a link flow a
service publishes for whoever asks, and it should be made deliberately rather than because the
bytes happened to be reachable.

### Auth is not the last wall

Even with a token, playing protected content is a separate problem.
[`getMediaURI`](https://docs.sonos.com/docs/getmediauri) can return `httpHeaders` for the player to
send with the media GET, plus `contentKey` and `deviceSessionKey` for encrypted streams.
`loadStreamUrl` has **no field for headers**; only `loadCloudQueue` does (`httpAuthorization`),
which means x2rock serving a cloud queue over HTTP the players can reach — behind Omarchy's
default-deny firewall. Free radio worked with `loadStreamUrl` precisely because it needs none of
this.

### Recommended order

1. **Device link against Bandcamp**, which already answers. It exercises the whole flow —
   `getDeviceLinkCode`, `xdg-open`, polling `getDeviceAuthToken` through `NOT_LINKED_RETRY`,
   storing `authToken`/`privateKey`, and `match` to register the account on the household — against
   a service that has proven it will talk to us. Also the first time this project stores a secret,
   which deserves its own thought rather than being smuggled in behind a bigger service.
2. Then the other device-link services, which differ only in request details.
3. Then decide about app-link and the API key, with the playback wall priced in. That is a
   different project, and the honest version of "and then it gets happy" is that YouTube Music
   specifically may not get happy at all.


## `x2rock link`, built (2026-08-31)

Step 1 of that order, shipped: `x2rock link [service]`, `x2rock accounts`, `x2rock unlink
<service>`. `getDeviceLinkCode`, `xdg-open`, polling `getDeviceAuthToken` through the pending
fault, storing the token, and `musicServiceAccounts:1 match` to register the account. Verified on
the office household against Bandcamp as far as the browser step; the login itself is a person's
job and is noted below as the one thing still unconfirmed.

`link` with no argument lists the 14 device-link services, which is the doc's list exactly -
AccuRadio, Sonos Backgrounds, Bandcamp, Classical Archives, Deezer, FIT Radio, iHeartRadio,
Mixcloud, Murfie, NhacCuaTui, Saavn, TIDAL, Tribe of Noise, Sonos Radio. `search` with no argument
still lists 32 of 108, and now says how many more could be linked.

### The finding that cost the most: a fault at HTTP 200

`getDeviceAuthToken`'s pending reply is a SOAP fault, as documented, but it arrives with **HTTP
200** - and the fault code is `s:NOT_LINKED_RETRY`, not the documented `Client.NOT_LINKED_RETRY`:

```
HTTP/1.1 200
<s:Fault><faultcode>s:NOT_LINKED_RETRY</faultcode>
  <faultstring>Link Code not found retry...</faultstring>
  <detail><ExceptionInfo>NOT_LINKED_RETRY</ExceptionInfo><SonosError>5</SonosError></detail>
</s:Fault>
```

`smapi.rs` had only ever looked for a fault when the status was not 200, which was fine for every
call it made before this one, because every one of those either worked or failed with a 500. Here
it read the pending fault as a *successful reply*, found no `authToken` in it, and reported
"Bandcamp linked but returned no authToken" three seconds into a flow that had not begun. **The
body decides whether a reply is a fault, not the status.** Both signals are now checked for
pending - the `NOT_LINKED_RETRY` substring and `SonosError` 5 - because services are inconsistent
about which they populate and Bandcamp's faultcode already disagrees with the spec.

A related hardening, from the same lesson: a body that will not parse at all falls back to a
substring check for `NOT_LINKED_RETRY` before being called an error. Seven minutes of polling
should not abort on one truncated reply, because the cost of aborting is a link code that cannot be
reused and a person sent back to the browser.

### The first secret, and where it goes

`$XDG_STATE_HOME/x2rock/credentials.json`, mode **0600**, its own file. Decided over the DBus
Secret Service deliberately. A keyring would encrypt it at rest and would add a dependency plus a
new failure mode - a locked or absent keyring between a person and their music - to a tool that is
expected to work over ssh and inside a bar widget's subprocess. The token is scoped to one music
service and revocable from that service's own account page, and the file leans on the disk
encryption a laptop already has.

Four decisions in the store worth keeping if it is rewritten:

- **The mode is set when the temporary file is created**, in the `OpenOptions`, not `chmod`ed
  afterwards. A `chmod` after the write leaves a window where the token sits on disk at the umask's
  mercy.
- **A loose file is tightened on read**, with a line to stderr. Warning and carrying on would leave
  a world-readable secret world-readable.
- **Keyed by service id, not name.** A name in Sonos's catalogue can change under a stable id.
- **A corrupt file is an error, not an empty account list** - the bookmarks rule, not the catalogue
  rule. Silently starting over would present itself as "no account linked" and send someone back
  through a browser flow to fix a typo they could have edited back.
- **The token is written before `match` is attempted.** A link code is single-use, so a failure in
  the household step must not cost the credential that already worked; `match` failing prints a
  warning and exits 0 with the token saved.

### `Auth` grew a third case, and the cache had to go

`Auth` was Anonymous-or-Linked, which flattened exactly the distinction linking depends on. It is
now `Anonymous`, `DeviceLink`, `AppLink`, so `SCHEMA` went to 2 and every cached catalogue is
discarded - a service wrongly filed as unusable is precisely what this feature exists to fix.
Anything unrecognised maps to `AppLink`, the conservative direction: mislabelling costs a clear
error instead of a confusing failure part-way through a flow.

Refusals now name the fix. A device-link service says `Run \`x2rock link <name>\``; an app-link one
says a Linux desktop cannot hand off to a mobile app and does *not* suggest a command that will
not help.

### Linked for real, and the flow works (verified 2026-08-31)

A person logged in. The whole path runs:

```
$ x2rock link Bandcamp
Opened Bandcamp in your browser.
Waiting for you to finish.............................
Linked Bandcamp. Search it with: x2rock search -s Bandcamp
```

29 polls, about 90 seconds of someone registering an account. What came back:

- `authToken` — 32 hex characters.
- `privateKey` — **`p455w0rd`**, literally. Bandcamp does not use the field and says so in the
  most legible way available to it. The decision to accept a reply with a token and no real key
  rather than refusing it was the right one, and this is why.
- `userIdHashCode` — **absent**. So `match` was skipped, with the message it was written for, and
  the household does not know about the account.

The `loginToken` header is accepted: `getMetadata` on `root` returns Bandcamp's browse tree, which
is proof the credential is honoured by an endpoint that has one to check.

### The deflating part: a token buys a *collection*, not a catalogue

`search` returns `total 0` for every term in every category, with no fault. Not a bug, and the
probe says why. Bandcamp's SMAPI root is:

```
artists / albums / tracks / rp (Recent Purchases) / rr (Recent Releases)
```

and `getMetadata` on all five reports `total=0`. Those are the *account's* containers, not
Bandcamp's catalogue: a new account with no purchases, no wishlist and nobody followed has nothing
in them, so a search over them finds nothing. The categories and their `mappedId`s were never
wrong — Bandcamp's own presentation map declares exactly `artists`/`albums`/`tracks` with identity
mappings, and that is what was sent.

**This corrects an assumption this document carried without stating it**: that the 76 services
search cannot reach are 76 catalogues behind a credential. At least for Bandcamp, the credential
opens a personal library. Linking is still worth having — it is the only way to reach a paid
collection from Linux at all — but "link a service and search it" is not the shape of the feature
for services like this one, and the next service linked should be checked against this question
first rather than assumed.

That points at browsing, not searching, as the thing worth building next for linked services:
`getMetadata` over those containers is what would make a Bandcamp collection playable from the bar.
Its absence is why `search -s Bandcamp` looks broken when nothing is.

### `X2ROCK_DUMP_SMAPI`

Set it and every SMAPI request and reply goes to stderr. It paid for itself twice in one session -
once on the HTTP 200 fault, once on the empty search - so it stays. The **whole credentials header**
is replaced with `(credentials omitted)`, not the token and key within it: a redaction that has to
enumerate which fields are secret is one field away from printing a token into a log, and the
header has never been the interesting half of a request that is misbehaving.

### Still unconfirmed

- **`match`**, entirely. Bandcamp sends no `userIdHashCode`, so nothing exercised it. The account
  id is read from `id` or `accountId`, and a reply with neither is treated as success; which field
  a household actually uses is unverified. A device-link service that *does* send a hash is needed,
  and TIDAL or Deezer are the obvious candidates.
- **Whether any device-link service offers a searchable catalogue** rather than a personal library.
  Sonos Radio is the most likely to, being stations rather than purchases.
- **`linkDeviceId`**, still left out of both link calls. No longer a leading hypothesis for
  anything: 10 of the 14 answer without it, iHeartRadio among them, and the 4 that do not fail for
  four unrelated reasons. See "All 14 probed".

### All 14 probed: 10 answer, and 4 fail in four different ways (verified 2026-08-31)

One `getDeviceLinkCode` each. **Ten hand over a link URL immediately:**

```
AccuRadio        https://www.accuradio.com/sonos/login/?code=XJWOLY
Bandcamp         https://bandcamp.com/login?sonos_link_code=...
FIT Radio        https://www.fitradio.com/sonoslogin?sonos_code=Z6MKJ3H
iHeartRadio      https://www.iheart.com/activate/sonos/?code=88224
Mixcloud         https://app.mixcloud.com/oauth/authorize?client_id=...
Murfie           https://www.murfie.com/sonos/link/15KBLX6K
NhacCuaTui       https://sonos.nhaccuatui.com/device/link?linkCode=...
Saavn            https://www.saavn.com/login.php?ctx=sonos&linkcode=...
TIDAL            https://login.tidal.com/authorize?redirect_uri=...
Tribe of Noise   https://sonos.tribeofnoise.com/sessions/start/4AYWS
```

**This corrects the earlier note that iHeartRadio and Deezer "returned nothing".** iHeartRadio
answers fine — a five-digit activation code, the shortest of any of them. Only Deezer does not, and
the four failures are four different problems, none of them `linkDeviceId`:

- **Deezer** — HTTP 200 with an **empty body**. Not a parse problem; the service says nothing.
- **Classical Archives** — a fault whose entire message is `str3`.
- **Sonos Backgrounds** — a reply with no `linkCode` in it. Plausibly not a real music service.
- **Sonos Radio** — its SMAPI server **crashes**: `TypeError: method is not a function`, SOAP 1.2,
  HTTP 500. Sonos's own service is the only one that returns a stack-trace-shaped error, which
  closes the hopeful guess that Sonos Radio would be the device-link service with a real catalogue.

So `linkDeviceId` is no longer the leading hypothesis for anything, and the deferred question is
not "make more services answer" — 10 of 14 already do.

### Which of the ten are actually worth a login (probed 2026-08-31)

The link URL is a login page, so it can be fetched and read without linking anything. Sign-in
providers are taken from the served HTML, which is suggestive rather than final — several of these
are JS-rendered and may offer more than they ship in the first response.

| Service | Page | Sign-in seen | Cost | Shape |
|---|---|---|---|---|
| Mixcloud | 200 | Google, Facebook, Twitter | free tier | DJ mixes; **OAuth `authorize` flow**, unlike any linked so far |
| Saavn (JioSaavn) | 200 | Google, Facebook | free tier | very large catalogue |
| FIT Radio | 200 | Google, Apple, Facebook | subscription | workout radio |
| AccuRadio | 200 | Facebook, Apple, password — **no Google** | free | US radio |
| Tribe of Noise | 200 | none in HTML | free | Creative Commons music |
| TIDAL | 403 to curl (bot check) | — | paid | **the only protected-stream candidate** |
| Murfie | 200, 2.7 KB, Facebook only | — | — | company shut down years ago; a zombie in Sonos's catalogue |
| NhacCuaTui | 200, **175 bytes** | none | — | serves an all but empty page |
| Bandcamp | linked | — | free | shop-shaped, collection empty |
| iHeartRadio | linked | — | free | broadcaster, works |

Read for what each would *teach* rather than what would merely work:

- **Mixcloud** is the best next login. Free, Google sign-in, catalogue-shaped, and its `regUrl` is a
  real OAuth `authorize` endpoint carrying the `linkCode` in `redirect_uri` — a third flow shape
  after Bandcamp's code-in-URL and iHeartRadio's typed code, and the only one that puts a consent
  screen in the path. It also has a following/favourites side, so it exercises browse and search at
  once, and it is a second chance for `match` to succeed.
- **TIDAL** is the only route to the one architectural unknown left: whether `getMediaURI` ever
  returns `httpHeaders` or a `contentKey` that `loadStreamUrl` cannot carry. Every service reached
  so far hands back a plain URL. Costs a subscription, which is why it is not first.
- **AccuRadio** looks like the obvious free-radio pick and is the wrong one for a Google account.
- **Murfie and NhacCuaTui answer `getDeviceLinkCode` and are not worth a login.** A service
  answering the protocol says nothing about the service being alive, which is worth knowing before
  reading a refusal as a bug in this code.

### Two parser gaps the survey found, both fixed

Neither was reachable from Bandcamp, which is why building against one service was not enough:

1. **SOAP 1.2.** SMAPI is specified as 1.1 and Sonos Radio replies in 1.2, where a fault has no
   `faultcode` and no `faultstring`: the code is `Code/Value` plus `Subcode/Value`, the message is
   `Reason/Text`. Reading only the 1.1 names turned "TypeError: method is not a function" into "a
   fault with no faultstring" — the useful answer was in the reply and got discarded. All code
   values are joined, so a pending check works whichever half carries the word, and a 1.2-shaped
   `NOT_LINKED_RETRY` is now pending too, though nothing has sent one.
2. **An empty 200 is not a reply.** Deezer's empty body fell through to the XML reader and reported
   "parsing getDeviceLinkCode response", blaming the parser for a service that said nothing.

The pattern in both: an error message that named the wrong culprit. `X2ROCK_DUMP_SMAPI` found each
in one run.

### The free radio services are the cheap way forward

Four of the ten that answer are free, catalogue-shaped services needing no purchase: **AccuRadio,
FIT Radio, iHeartRadio, Tribe of Noise**. Any one of them settles, for the price of one browser
login, the two things Bandcamp cannot:

- **Whether a linked service offers a searchable catalogue** rather than a personal library. A
  radio service has no collection to be empty, so a search that returns nothing there would mean
  something quite different from Bandcamp's zero.
- **Whether `match` ever runs.** It has never executed, because Bandcamp sends no
  `userIdHashCode`. Any service that sends one exercises it.

iHeartRadio is the pick: the largest catalogue of the four, and its five-digit code makes the
browser step the shortest.

## iHeartRadio linked: a token does buy a catalogue (verified 2026-08-31)

The experiment the Bandcamp result called for, and it answers the open question in the affirmative.

```
$ x2rock link iHeartRadio
Opened iHeartRadio in your browser.
Enter this code when asked:

  45943

Waiting for you to finish...
Linked iHeartRadio.
```

Three polls. `showLinkCode: true` this time, so the code-on-screen path is verified too — the
branch Bandcamp never exercised, since its code rides in the URL.

**Search works over a real catalogue.** Six categories — stations, artists, tracks, albums,
playlists, podcasts — and `jazz` returns 55 stations. So the deflating Bandcamp finding was about
*Bandcamp*, not about linking: a device-link credential can absolutely open a searchable catalogue,
and the difference is whether the service is a shop or a broadcaster. Bandcamp sells you a library;
iHeartRadio broadcasts a catalogue. Both are reached the same way.

Its token is also the second in a row to make the same point about strictness: `authToken` is 11
characters and `privateKey` is **empty**. Bandcamp sent `p455w0rd`. Refusing a reply that carries a
token without a real key would have blocked both services this project has successfully linked.

### `match`, and why nothing needs it yet

`musicServiceAccounts:1 match` ran for the first time here — iHeartRadio does send a
`userIdHashCode` (`13012528881`) — and it was refused: `ERROR_COMMAND_FAILED`, no reason. Probed
with `x2rock raw` afterwards:

- Without `linkCode`: an explicit complaint, **`Link code required to add guest account`**.
- With `linkCode`, real or freshly minted, and with or without `linkDeviceId`:
  `ERROR_COMMAND_FAILED` and **no reason at all**.
- `serviceId` as a JSON number or a string makes no difference.

So the household reaches a silent failure path whenever a code is present. The word *guest* is the
only real clue, and it hints that `match` adds a limited kind of account rather than a full one. A
fresh unredeemed code failed identically, but that probe used a bogus `userIdHashCode`, so it does
not cleanly rule out the tidier theory — that `getDeviceAuthToken` and `match` are two *alternative*
consumers of one single-use code, and the controller taking the token leaves nothing for the
household to redeem. Testing that properly needs a service linked by calling `match` *before*
`getDeviceAuthToken`, which is a different flow from the one built here.

**None of which has cost anything, because the account identity travels in the stream URL.**
`getMediaURI` on an iHeartRadio station returns a plain HLS URL:

```
http://stream.revma.ihrhls.com/zc4242/hls.m3u8?...&deviceId=Sonos_Gcd...&profileId=13012528881&...
```

`profileId` is the `userIdHashCode`. No `httpHeaders`, no `contentKey`, no `deviceSessionKey` — so
`loadStreamUrl` can carry it, and "Auth is not the last wall" turns out not to be a wall for this
service at all. The household does not need to know about the account because every request already
names it.

Which reframes `match` from a required last step into an optional one that may not be available to a
controller. It is still attempted, because it is the documented step and one household's refusal is
not proof, but the message when it fails is now mild rather than alarming: every `match` this
project has attempted has been refused, and nothing has yet needed one.

### Played, end to end (verified 2026-08-31)

```
$ x2rock play-item -s iHeartRadio live_stations.2157 --title "The BIG 98"
Media Room — The BIG 98 on iHeartRadio
$ x2rock now
PLAYING  Springsteen — Eric Church
```

The whole chain: device link, browse, `getMediaURI`, `loadStreamUrl`, audio in the room, and live
per-track metadata arriving back through the daemon's MPRIS without the daemon knowing a service was
ever linked. **There is no wall.** "Auth is not the last wall" was written about content that needs
`httpHeaders` or `contentKey`; a linked broadcaster needs neither.

Three facts learned in the attempt:

- **`getMediaURI` has an id grammar, and it is narrow.** Asking for `artist_radio.41615` was refused
  with the accepted set spelled out: `artist_radio_track`, `live_stations.`, `podcast_show`. So an
  `artist_radio` entry is a *container* to browse into, not a playable id, even though `getMetadata`
  marks it `canPlay` — that flag means "this collection can be played", not "this id can be
  resolved". Worth remembering before trusting `canPlay` as a filter.
- **A failed `play-item` leaves the room alone.** `getMediaURI` is called before the session is
  created, so the refusal above cost nothing: the room kept playing what it was playing. That
  ordering was accidental rather than designed, and is worth keeping.
- **iHeartRadio's own "My Stations" is broken**, server-side and nothing to do with x2rock:

  ```
  faultcode senv:Server.ServiceUnknownError
  Request to http://ampinternal.ihrprod.net/api/v2/playlists/13012528881?... failed with error
  {"obj.hits[0].seedProfileId":[{"msg":["error.expected.int"],"args":[]}]}
  ```

  Their internal playlists API cannot serialize its own first result. `for_you` (57 items) and
  `nearby_stations` (10) answer fine, so the account is healthy and one container is not.

### The clearest argument yet for `x2rock browse`

The request that produced all of the above was "play something from my iHeart playlists" — and it
was **not expressible with the CLI as it stands.** Search takes a term; a personal container is not
a search. Answering it needed hand-rolled SOAP against `getMetadata`, and the account root turned
out to hold seven containers worth reaching:

```
for_you (57)      nearby_stations (10)   live_stations   create_station
my_playlists_<userIdHashCode>  ("My Stations", broken upstream)
top_podcasts      top_genres
```

That id embedding the `userIdHashCode` is the same identity `profileId` carries in the stream URL,
which is a third sighting of the same fact: everything personal about a linked service is keyed by
that hash, and the household is never consulted.

So `browse` is not a nice-to-have for shop-shaped services like Bandcamp. It is the missing half of
the feature for *every* linked service, and the first real user request against linked accounts
could not be served without it.

## `browse`, and the picker that stopped being about favorites (2026-08-31)

`x2rock browse [-s service] [container]`, and the widget's picker rebuilt around it. Verified three
levels deep on TuneIn — root, `Music`, `Top Music Stations`, ending in playable `s30119` streams —
and on iHeartRadio's own containers.

`getMetadata` and `search` return the same payload, so one parser serves both. What browsing needs
that searching did not is a single new field on an item: **`container`**, true when it arrived as a
`mediaCollection` rather than a `mediaMetadata`. That is the *only* reliable answer to "can this be
played", and the reason is written up above — iHeartRadio marks an `artist_radio` collection
`canPlay` and then refuses its id with a grammar error. `canPlay` is never consulted.

Browsing is offered for exactly the services search is offered for: an endpoint plus, if linked, a
token. So `searchable()` backs both, and nothing new had to decide reachability.

### The picker is now a music picker

The room-row button changed from a star to a note, and with it the panel's meaning: it was the
household's favorites, and it is now favorites, this machine's kept items, a service's own
containers, and a search. Four sources, one question — what should this room play.

- **The glyph is `nf-md-music` (U+F075A), two beamed quavers.** Plain `♪` (U+266A) was tried first,
  to avoid depending on a patched font — and reverted the same day, because **the dependency was
  already there**: sixteen of the widget's seventeen glyphs are Nerd Font icons and the whole thing
  draws boxes without one. Avoiding it for a single button bought nothing and cost a per-character
  font fallback, since JetBrainsMono Nerd Font has no U+266A and the note arrived from Adwaita or
  Liberation at a different weight from its neighbours. The lesson is small and generalises: check
  what a dependency already costs before paying to avoid it in one place. `♪` stays documented as
  the right default for a bar whose font is *not* patched, which is what `glyphs.music` is for, and
  `party`'s `◉` remains the one plain-Unicode default.
- **The glyph key is `music` now, and `favorites` still works.** Renaming a documented setting to
  reflect a better name is not worth breaking a household's `shell.json` over, so the old key is
  honoured after the merge.
- **Inside a container the list is that container and nothing else.** Favorites are not mixed in:
  with them there, "back" has two meanings and the count line describes nothing.
- **The path is pushed before the reply arrives**, so the title and the back row are right while the
  call is still in flight. The reply is then only shown if it answers the container on screen —
  `browseAnsweredFor` against the frame's key — because a slow reply to somewhere already left would
  silently move someone.
- **A container that fails to open leaves the path where it is.** iHeartRadio's own My Stations
  returns a 500, and dropping someone out of a tree they were halfway down would be a worse answer
  than a one-line status. This is the "one failing container must not fail the listing" rule from the
  entry above, in the only form the widget needs it.
- **`Backspace` and `←` go up, but only on an empty filter.** Both keys mean something to a text
  cursor, and a filter someone is still editing outranks navigation.
- **`play-item` takes the row's own service**, not `searchService`. A browse row can come from a
  service the picker was not configured around, and the old code would have handed one service's id
  to another.
- **The subtitle drops the service name while browsing.** The title already says which service one
  is inside; repeating it on fifty rows is noise.

Its own `Process`, like `searchProc` and for the same reason: browsing leaves the LAN, and the rule
that search never enters the daemon covers this too.

## Mixcloud: a third flow shape, and a search that answers in places (verified 2026-08-31)

Linked through an OAuth `authorize` endpoint with a consent screen — the third flow shape after
Bandcamp's code-in-URL and iHeartRadio's typed code, and the one that most resembles what people
expect from "sign in with". `showLinkCode` false, 28 polls, no `userIdHashCode`, so `match` was
skipped again. Its `privateKey` is 10 characters and its `authToken` 32.

Its root is the hybrid Bandcamp and iHeartRadio each only half were:

```
feed / trending / listen-later / categories:music / categories:talk
new-uploads / queue / user:<username>
```

and `user:<username>` opens Stream, Uploads, Favorites, Listens, Playlists, Followers, Following.
So one service carries both a catalogue and a personal library, reached identically. That id
embedding the account's own username is the third sighting of the pattern — iHeartRadio's
`my_playlists_<userIdHashCode>`, `profileId` in its stream URL, and now this: everything personal
about a linked service is keyed into the id, and the household is never consulted.

### The bug it found: a search can answer entirely in containers

**Mixcloud's only search category is `tags`, and every hit is a `tag:` collection rather than a
track.** No service before it had done this — TuneIn and iHeartRadio both answer searches with
playable streams — so `search` had quietly assumed its results were things to play:

- `search --json` did not emit `container`, though `browse` did and the field existed on the item.
- `search --play N` would have handed a collection id to `getMediaURI`.
- The widget offered every hit as a track, so choosing one would have failed in `play-item`.

All three fixed, and the widget now descends into a container hit instead of playing it. That path
had to stop assuming a browse frame was open, because a container can now arrive as a *search* hit
with no frame to inherit a service from.

The general lesson is the one `canPlay` already taught in a different costume: **`container` is the
only thing that says whether an id can be played, and it has to be carried everywhere an item
goes.** Adding a field to a struct is not the same as plumbing it.

### Still no wall

`getMediaURI` on a Mixcloud track returns a plain HLS URL, the third service in a row to do so:

```
https://aod.mixcloud.stream/secure/hls_aes128/...index.m3u8
```

Worth reading closely: `hls_aes128` means the stream **is** encrypted, and Sonos still needs no
`contentKey` — the key URI travels inside the m3u8, the way HLS specifies. Which suggests
`contentKey` and `deviceSessionKey` are for some other, probably older DRM path, and that the
"auth is not the last wall" warning may be pointing at a wall that modern services simply do not
use. Not proven, but three services in and nothing has needed `httpHeaders` yet. TIDAL is still the
only candidate left that might.

### How to test this when the collection is not empty (deferred 2026-08-31)

Everything above was verified against an account with nothing in it, which is exactly the state
that cannot tell "the credential is not working" apart from "there is nothing to find". Closing
that needs one purchase, one wishlist item, or one followed artist on
[bandcamp.com](https://bandcamp.com) — the account is already linked, so no re-link is needed.
Then, in order, and each one answers something the empty account could not:

```sh
X2ROCK_DUMP_SMAPI=1 x2rock search -s bandcamp -c albums <something in the collection>
```

1. **Do the containers fill?** The probe worth running first is `getMetadata`, which x2rock has no
   command for yet — so either build `x2rock browse` (the open question's first item) or hand-roll
   the SOAP call. `artists`/`albums`/`tracks`/`rp`/`rr` reporting a non-zero `total` is the whole
   answer: the credential works and the library is simply reachable.
2. **Does `search` see the collection, or only browse?** A wishlist item that `getMetadata` lists
   but `search` still misses would mean Bandcamp's `search` is scoped to purchases alone, or is not
   really implemented — worth knowing before any UI leans on it.
3. **Does a hit play?** `x2rock play-item -s bandcamp <id>` through `getMediaURI`. This is the one
   that could still fail on its own terms: `getMediaURI` may answer with `httpHeaders` or a
   `contentKey`, and `loadStreamUrl` has no field for either. That wall is documented above under
   "Auth is not the last wall" and it has never been hit in practice, because free radio needs
   none of it. A paid download is the first content that might.

If (1) comes back empty with something genuinely in the collection, suspect the account rather than
the code: Bandcamp's Sonos integration may want the purchase to be a *download* rather than a
stream-only item, and re-linking would be the cheap thing to rule out next.


## Rule: search never enters the daemon (decided 2026-08-31)

Talking to music services is allowed. Breaking the parts that do not need the internet is not.
Losing a name lookup must never cost the household its transport or its volume.

The architecture already separates these, and the rule is to keep it that way rather than to build
anything new for it:

- **State reaches the widget over MPRIS**, published by the daemon from the player's own events.
  The daemon speaks only to the LAN — the Control API WebSocket on 1443 and UPnP on 1400 — and
  cover art comes from the player itself (`http://<player>:1400/getaa`). **Nothing the daemon does
  touches the internet, and search must not change that.** A daemon that fetched a service
  catalogue would put an internet timeout in front of play/pause for every room.
- **Actions reach the speakers over MPRIS too**, except the scroll gesture, which shells out to
  `x2rock vol +N -r <room>` because Sonos wants relative volume and MPRIS cannot express it. That
  call is LAN-only and stays that way.
- **Search is therefore a CLI command and nothing else.** The widget invokes it the way it already
  invokes favorites: a separate `Process`, whose failure is a string inside one picker rather than
  a fault in the widget.

That last point is not a plan, it is a working precedent. The favorites picker in `BarWidget.qml`
already does exactly what search needs to do, and it is worth copying rather than reinventing:

- a `Process` of its own, so a hung or failed call cannot reach anything else;
- a non-zero exit sets a status string shown *in the picker*, leaving rooms, transport and volume
  untouched;
- empty stdout is treated as failure rather than as an empty list, because the two mean different
  things and only the exit code can tell them apart;
- the results already on screen stay up while a refresh runs, so a network blip degrades to stale
  data rather than to an empty list.

**The CLI may be blunt.** `x2rock search` failing with a plain error and a non-zero exit is correct
behaviour; it is a terminal command and the person running it can read. The graceful half belongs
in the widget, and the split is deliberate: one honest failure mode at the boundary, one forgiving
presentation above it.

Two things follow for the implementation:

- **Internet calls need their own timeout**, short and bounded, not the LAN's. `upnp.rs` uses 8s
  against a server on the same switch; a widget-invoked subprocess that might be waiting on a
  service in another country needs a budget it will actually hit, and it must always terminate.
- **The service catalogue is cached on disk and usable stale.** With no internet, listing services
  and categories should still work from the last good read; only the query itself needs to fail.
  `musicServiceAccounts:1`'s `musicServicesChanged` event carries `availableServicesVersion`, which
  is the invalidation signal — the one use that namespace turns out to have.


## SMAPI, read properly at last — and search works today (verified 2026-08-31)

Everything this document said about SMAPI before this section was inferred from service descriptors
and from the Control API spec's passing mentions of it. [The actual
documentation](https://docs.sonos.com/docs/smapi) had never been read. It should have been read
first, and reading it turned a blocked feature into a working one inside an hour.

### What SMAPI is

SOAP 1.1 over HTTPS, namespace `http://www.sonos.com/Services/1.1`, described by WSDL. **Sonos is
the client and the music service is the server** — a service implements SMAPI so that Sonos can
call it. 32 operations in six groups; the ones that matter here are `search`, `getMetadata`,
`getMediaMetadata` and `getMediaURI`.

That direction is the thing this document kept getting backwards. There is no sense in which a
controller "gets search from Sonos". A controller that wants search does what a Sonos player does:
it calls the service directly, as a SMAPI client.

### The credentials header, and who actually needs one

From [SOAP requests and responses](https://docs.sonos.com/docs/soap-requests-and-responses), the
`credentials` header carries:

| Element | Status |
|---|---|
| `deviceProvider` | **required**, always the string `Sonos` |
| `loginToken` (`token`, `key`, `householdId`) | required **for services that authenticate** |
| `deviceCert` | optional; only when the service declares "Requires Device Certificate" |
| `zonePlayerId` | optional; only when the service declares that capability |
| `deviceId` | deprecated, ignorable |

`token` is the authentication token and `key` the refresh token — the per-household service
credential. But the header is only as heavy as the service demands, and this is the fact that
changes everything:

**32 of the 108 services on this household's descriptor list are `Auth="Anonymous"`.** The split:

| `Policy Auth` | count |
|---|---|
| `AppLink` | 62 |
| `Anonymous` | **32** |
| `DeviceLink` | 14 |

Anonymous means no `loginToken` at all. And the anonymous third is not a leftover — it is TuneIn
(254), SomaFM (516), Hype Machine (44), Global Player, CBC Radio, and a long tail of radio
networks. Exactly the discovery-shaped services.

### Verified end to end, no account, no token, no cert

Against TuneIn (`https://legato.radiotime.com/Radio.asmx`), with a credentials header containing
nothing but `<deviceProvider>Sonos</deviceProvider>`:

- **Search categories** come from the presentation map, fetched anonymously:
  `stations → search:station`, `podcasts → search:show`.
- **`search`** with `id=search:station`, `term=jazz`, `index=0`, `count=5` returned **HTTP 200 and
  93 total matches**, as `mediaMetadata` with `id`, `title`, `itemType=stream`, genre, country,
  bitrate and logo URL. The response is the same shape as `getMetadata`, exactly as documented.
- **`getMediaURI`** for the first result (`s249973`, "Smooth Jazz") returned a playable URL:
  `http://opml.radiotime.com/Tune.ashx?id=s249973&listenId=…&partnerId=Sonos`.

So the whole chain, with no credential anywhere in it:

```
ListAvailableServices (LAN, unauthenticated)   → endpoint + Policy Auth + manifest URI
manifest + presentation map (anonymous CDN)    → search endpoint shape + categories
SMAPI search  (deviceProvider: Sonos)          → results
SMAPI getMediaURI                              → a stream URL
playbackSession:1 createSession + loadStreamUrl → plays it
```

**The whole chain is verified.** Run end to end on the Media Room, 2026-08-31:

```sh
# search:station "jazz" -> s250015 "Jazz Club" -> getMediaURI
x2rock raw playbackSession:1 createSession \
  '{"appId":"com.rahga.x2rock","appContext":"cli"}' --scope group -r "Media Room"
#   -> sessionCreated: true, sessionState: SESSION_STATE_CONNECTED
#   -> sessionId: RINCON_48A6B81853E001400:836412709@3406530134
x2rock raw playbackSession:1 loadStreamUrl \
  '{"streamUrl":"http://opml.radiotime.com/Tune.ashx?id=s250015&listenId=…&partnerId=Sonos",
    "playOnCompletion":true,
    "stationMetadata":{"name":"Jazz Club","type":"station",
                       "service":{"name":"TuneIn","id":"254"}}}' \
  --session 'RINCON_48A6B81853E001400:836412709@3406530134'
#   -> success, empty body; the room starts playing
```

`x2rock now` then reported `PLAYING  Jazz Club`, and it propagated through x2rock's own daemon to
MPRIS — `xesam:title "Jazz Club"`, `PlaybackStatus "Playing"` — so the bar widget picked it up with
no extra work. Notes from doing it:

- `createSession` is **group**-scoped and returns the `sessionId`; everything after it is addressed
  by that id, which is why `x2rock raw` grew `--session`. A session id is an explicit address, so
  it overrides `--scope` rather than combining with it.
- `stationMetadata` is optional but worth sending: it is where the title the room displays comes
  from. Without it the stream plays with nothing to show.
- The session survives the CLI process exiting — it belongs to the group, not to the connection.
- Playback did **not** disturb the queue: `queueVersion` bumped, but this is a session source
  rather than a queue entry, which is the whole point of the mechanism.

**A gotcha that cost a request:** sending the envelope with an XML declaration produced
`s:Client / Expecting state 'Element'.. Encountered 'Text'`, which reads like a malformed-request
fault and is not one. Send the envelope with no `<?xml?>` prologue and no BOM (`curl --data-binary`
from a file written with `printf`, not `echo`).

### What this does to the credential question

It shrinks it from "the feature is blocked" to "a third of services work now, and the other
two-thirds need the household's `loginToken`".

It also explains **UPnP 806** better than "no account did". SMAPI `getSessionId` is the *legacy*
username-and-password auth path — Sonos's own guidance is that existing integrations keep working
but new ones should use `getAppLink`. Neither service tried at the office uses it: Sonos Radio is
`DeviceLink`, YouTube Music is `AppLink`. So `MusicServices:1 GetSessionId` most likely answers 806
because those services do not do sessions at all, not because the household lacks accounts.

That makes the home runbook much less interesting than it looked, and it should be re-aimed: the
question worth answering at home is not "does `GetSessionId` work for Sonos Radio" but **"is there
any read path to a `loginToken`"** — and the honest expectation is no, because
`SystemProperties:1` can write accounts (`AddOAuthAccountX`) and has nothing that reads them back,
and S1's `/status/accounts` is gone. If that holds, authenticated services are reachable only by
x2rock running a `DeviceLink` flow and registering as its own account, which is a real project and
a scope decision, not a probe.

None of which blocks anything: **build search against the anonymous services first.** It is 32
services, it needs no secret, and it is verified working.


## What Sonos's own sample app settles (read 2026-08-31)

[`sonos/api-web-sample-app`](https://github.com/sonos/api-web-sample-app) — Sonos's official
web sample, last pushed 2025-05-29 — carries two artefacts worth more than the app itself:
`sample-app/Client/src/App/museClient/OAS_production.json`, the full **Control API OpenAPI spec**
(`v2.0.0-production`), and a Postman collection of the same. Together they are the authoritative
answer to what this API does and does not contain, and they close a question this document had
been circling.

### The Control API has no content discovery. At all.

**53 paths**, and not one of them searches, browses, or lists anything a service holds: playback,
volume, groups, favorites, playlists, audio clips, sessions. That is the whole surface.

This is not the LAN transport being a subset of the cloud — it is the *cloud* spec. So the earlier
finding that `musicService:1` rejects `search` was never about firmware or about the local API. The
command does not exist anywhere, and no amount of probing a player will produce it. **Question
closed: the Control API will never search.**

### `musicServiceAccounts match` is for service providers, not controllers

The one music-service endpoint in the API is not the account-enumeration tool it looked like when
probed. Its required fields are `userIdHashCode`, `nickname` and `serviceId`, and the spec says of
the first:

> Opaque hash of the user account. You must use the same algorithm used by **your SMAPI server**.
> See `getDeviceAuthToken` and `getUserInfo` SMAPI requests for details.

with `linkCode` and `linkDeviceId` described in terms of what "your SMAPI service" sends. So `match`
is how a **music service** registers or looks up its own account on a household. It returns an
account `id`, but only to a caller that already implements the service side. A controller has no
way to supply `userIdHashCode`, and this is not a route to borrowing the household's credential.

That reframes the credential question honestly: **SMAPI is a service-provider interface, and Sonos
never intended a third-party controller to consume it.** `MusicServices:1 GetSessionId` over UPnP
is still the one candidate for borrowing what the household holds, and nothing here contradicts it
— but nothing here supports it either, and the odds should be read down accordingly. Run the
`GetSessionId` probe at home before spending anything more on this.

### What Sonos expects a third-party app to do instead: bring its own content

The sanctioned path is `playbackSession`, and it is fully specified:

- **`createSession`** — required fields are only `appId` and `appContext`, both strings the app
  picks (`appId` a reverse-DNS name). No registration, no Sonos-side identity. Optional `accountId`
  names a music service account for the session, and `customData` is an opaque blob other instances
  of the app can read back. Returns `sessionId`, `sessionState`, `sessionCreated`.
- **`loadStreamUrl`** — **required fields: `streamUrl`, and nothing else.** Optional
  `playOnCompletion`, `stationMetadata`, `itemId`. This plays a live stream with no account, no
  cloud queue and no search.
- **`loadCloudQueue`** — required: `queueBaseUrl`. The player then pulls the track list from a
  server the app hosts, with optional `httpAuthorization`, `itemId`, `queueVersion`,
  `positionMillis` and full `trackMetadata` (whose `id` is `{objectId, serviceId, accountId}` — the
  place a service account, if we ever had one, would be referenced). `refreshCloudQueue` and
  `skipToItem` complete it.

This also explains something this document recorded as a puzzle on 2026-08-29: `AddURIToQueue`
refusing service-backed containers and stations is not an obstacle to work around. Sonos's model
is that a third-party app does not inject into the Sonos queue at all — it opens a session and
serves its own content.

### Verified on the LAN, 2026-08-31

The spec is the cloud API, so whether the local WebSocket carries the same namespaces had to be
checked. Probed with `x2rock raw`, deliberately with *no* parameters so that nothing was created —
the `ERROR_INVALID_PARAMETER` / `ERROR_UNSUPPORTED_COMMAND` distinction is enough:

| probe | answer | reading |
|---|---|---|
| `playbackSession:1 createSession` | `ERROR_INVALID_PARAMETER` | command exists |
| `playbackSession:1 loadStreamUrl` | `ERROR_INVALID_PARAMETER` | command exists |
| `audioClip:1 createSession` | `ERROR_UNSUPPORTED_COMMAND` | control: namespace exists, command does not |
| `playlists:1 getPlaylists` | success — `{playlists, version}` | a whole namespace x2rock does not use yet |

So the session API is present on the LAN.

### What this makes worth building, independently of search

**`createSession` + `loadStreamUrl` is a feature available today**, and it needs nothing that is
still unsolved: one required field, no account, no credential, no HTTP server of our own. It would
give x2rock the ability to play an arbitrary stream URL in any room — the thing `play` cannot do
and `favorite` can only approximate. Worth doing before search, not after, because it is the half
of "find something and play it" that has no open questions in it.

`loadCloudQueue` is the larger version and needs x2rock to serve HTTP the player can reach, which
the firewall section makes awkward. Not now.

One gap this exposed in `x2rock raw`: session commands are addressed by `sessionId`, which
`--scope` has no case for. Add `--session <id>` when the first session command is written for real.


## `x2rock raw`, and what it found in the account namespaces (verified 2026-08-31)

The first open question said to build a raw Control-API command before guessing at anything else.
Built: `x2rock raw <namespace> <command> [PARAMS]`, with `--scope household|group|player|none` for
the target key and `--watch <seconds>` to stay attached and print events afterwards. Three
decisions in it are worth keeping if it is ever rewritten:

- **A refusal is a result.** It prints the error body and still exits 0, because
  `ERROR_UNSUPPORTED_COMMAND` is the answer a probe usually went to get. Only a transport failure
  is an error.
- **It prints the header, not just the body.** The header is where the player says which namespace
  it thinks it answered, which turned out to be the single most useful field here.
- **`--watch` attaches the event receiver before sending**, because a `subscribe` can be answered
  by an event that overtakes the reply.

### `musicService:1` is real, and it is `musicServiceAccounts:1`

The 2026-08-28 note recorded that `musicService:1 search` returns `ERROR_UNSUPPORTED_COMMAND`. That
is still true, and it was being read wrong. The reply's header says:

```
"namespace": "musicServiceAccounts:1", "response": "search", "success": false
```

The player **canonicalises** `musicService:1` to `musicServiceAccounts:1` and then rejects the
*command*. The control experiment is what makes this solid: a nonsense namespace answers
`ERROR_UNSUPPORTED_NAMESPACE` with `"namespace": "global"` and the reason
`v1:totallyBogus:9 namespace is not supported.` So the two failures are different failures.
`musicService:1` is a supported namespace with no `search` in it — and its real name says what it
is actually for, which is accounts, not content.

Always read the echoed `namespace` before concluding a namespace is missing.

### What `musicServiceAccounts:1` actually supports

Probed by name; everything not listed returned `ERROR_UNSUPPORTED_COMMAND` (`getAccounts`,
`getServices`, `getMusicServices`, `getAvailableServices`, `getRegisteredServices`, `list`,
`getAll`, `refresh`, `create`, `add`, `getSessions`, and the obvious variants).

- **`match`** — supported, and wants a `nickname`: `Parsing terminated:[1].nickname`. An account
  lookup, not a list.
- **`subscribe` / `unsubscribe`** — supported, reply body empty.
- The event that follows a subscribe is **`musicServicesChanged`**, and it carries only version
  markers, not content:
  ```json
  {"_objectType":"musicServicesChanged",
   "availableServicesVersion":{"version":"58"},
   "registeredServicesVersion":{"version":"2026-08-31T13:38:41.240774449"}}
  ```
  So the Control API's role here is **cache invalidation**: it tells a controller that the
  available or the registered set has moved, and the controller re-reads the actual lists
  somewhere else. The word `registeredServices` is the first direct evidence that the household
  distinguishes "all services Sonos knows" from "services this household has", which is exactly
  open question 3 — but this namespace will not hand the second one over.

### The UPnP action lists, which should have been read first

Straight from the SCPDs, and definitive — no guessing at command names required:

- **`MusicServices:1`** (`/xml/MusicServices1.xml`): `GetSessionId(ServiceId, Username) → SessionId`,
  `ListAvailableServices`, `UpdateAvailableServices`. That is the whole service.
- **`SystemProperties:1`** (`/xml/SystemProperties1.xml`): `GetString`/`SetString`/`Remove`,
  `GetWebCode`, `AddAccountX`, **`AddOAuthAccountX`**(`AccountType`, `AccountToken`, `AccountKey`,
  `OAuthDeviceID`, `AuthorizationCode`, `RedirectURI`, `UserIdHashCode`, `AccountTier`,
  `AccountNickname`), `RemoveAccount`, `ReplaceAccountX`, `EditAccountPasswordX`,
  `SetAccountNicknameX`, `RefreshAccountCredentialsX`, `EditAccountMd`,
  `ProvisionCredentialedTrialAccountX`, `ResetThirdPartyCredentials`, `EnableRDM`/`GetRDM`,
  `DoPostUpdateTasks`.

Two things fall out of that list:

- **`GetSessionId` is the credential path.** It is the one action that hands a controller something
  it can present to a service, and it takes the `ServiceId` straight out of
  `ListAvailableServices`. Tried here for `303` (Sonos Radio) and `284` (YouTube Music): both
  return **UPnP error 806**. Whether 806 means "no account registered for that service on this
  household", "`Username` may not be empty", or something else is **not settled** — and this is
  the office household, which has one linked service and three favorites. Re-run it at home
  against a household with several services linked, and against a service that definitely has an
  account. That single call is now the pivot for the whole feature.
- **There is still no enumeration action anywhere.** `AddAccountX` and friends write accounts;
  nothing reads the list back. `/status/accounts` is gone on this firmware. So enumerating
  registered services remains unsolved, and the `AddOAuthAccountX` signature shows what the
  fallback would cost: x2rock would run a device-link flow itself and *register* the account, which
  means holding an OAuth token — the thing this project has so far never had to do.

### Run this at home: `GetSessionId` (largely superseded, 2026-08-31)

**Read "SMAPI, read properly at last" before spending time on this.** The premise it was written on
was wrong twice over: search is no longer blocked (32 anonymous services work today with no
credential), and UPnP 806 most likely means "this service does not do sessions" rather than "no
account here" — SMAPI `getSessionId` is the legacy username-and-password path, and neither service
tried at the office uses it.

What is still worth ten minutes at home is the *narrow* version: run it against a service whose
descriptor shows an auth policy that could plausibly involve a session, and treat a second 806 as
confirmation rather than as news. The real question behind it — whether any read path to a
`loginToken` exists at all — is not answered by this call.

**1. Find which services this household actually has.** There is still no enumeration action, so
work backwards from favorites — each carries the service in its `cdudn`:

```sh
P=<player-ip>
curl -s -X POST "http://$P:1400/MediaServer/ContentDirectory/Control" \
  -H 'Content-Type: text/xml; charset="utf-8"' \
  -H 'SOAPACTION: "urn:schemas-upnp-org:service:ContentDirectory:1#Browse"' \
  --data '<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><ObjectID>FV:2</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>100</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>' \
  | grep -oE 'SA_RINCON[0-9]+' | sort -u
```

Each `SA_RINCON<N>` maps to a service id by `N >> 8` — `77575 → 303`, Sonos Radio. That held for
the one data point here and is worth confirming against a second before trusting it. Cross-check
the ids against `ListAvailableServices`, which lists all 108 services with their names.

**2. Call `GetSessionId` for each of those service ids.**

```sh
for svc in <ids from step 1>; do
  echo "── $svc"
  curl -s -X POST "http://$P:1400/MusicServices/Control" \
    -H 'Content-Type: text/xml; charset="utf-8"' \
    -H 'SOAPACTION: "urn:schemas-upnp-org:service:MusicServices:1#GetSessionId"' \
    --data "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:GetSessionId xmlns:u=\"urn:schemas-upnp-org:service:MusicServices:1\"><ServiceId>$svc</ServiceId><Username></Username></u:GetSessionId></s:Body></s:Envelope>" \
    | sed -e 's/&lt;/</g;s/&gt;/>/g' | grep -oE '<(SessionId|errorCode|errorDescription)>[^<]*'
done
```

**3. If every one answers 806, vary the input before concluding anything.** 806 was the same answer
here for a service the household *does* have and one it does not, so it is not yet known to mean
"unlinked". Two cheap variations:

- a non-empty `Username` — the argument exists, and nothing so far says it is optional;
- `musicServiceAccounts:1 match`, which wants a `nickname` and may be the way to learn what
  username or nickname the account carries:
  ```sh
  x2rock raw musicServiceAccounts:1 match '{"nickname":"<try one>"}'
  ```

**What each outcome means**

- **A `SessionId` comes back** → the credential exists on the LAN, no account, no OAuth. Search
  becomes a small feature: `ListAvailableServices` for the endpoint, the manifest for its shape
  (REST `search` endpoint or the older SOAP action), the presentation map for the categories, and
  this session id in the SMAPI `credentials` header. Write it up and build it.
- **806 everywhere, including for services that are definitely linked** → the LAN will not hand a
  controller a credential, and the only path left is registering an account ourselves with
  `SystemProperties:1 AddOAuthAccountX`. That means running a device-link flow and storing an
  OAuth token — the first secret this project has ever had to keep, and a decision to take
  deliberately rather than drift into. Stop and reconsider scope at that point rather than
  starting the SMAPI client.

Record the answer here either way; a negative result is what closes the question.


### Where this leaves the three questions

1. ~~Re-probe `musicService:1`~~ — **done, and closed.** No search there; the namespace is about
   accounts, and it only reports versions. The player will not run a search for us.
2. **The credential** — narrowed from "somehow" to one action: `GetSessionId`, currently answering
   806 on a thin household. This is the next thing to run, and it needs the home household.
3. **Enumerating linked services** — still open, and now known *not* to live in
   `musicServiceAccounts:1`, `MusicServices:1` or `SystemProperties:1`. Next candidates: whatever
   the app reads after a `musicServicesChanged`, and `ContentDirectory` under a service-scoped
   `ObjectID` (`S:` returns 0 children here, so not that one as written).


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
- **`FV:2` reveals *some* linked services, and is not a way to enumerate them.** Every favorite
  here carries `<desc id="cdudn">SA_RINCON77575_X_#Svc77575-0-Token</desc>`, and 77575 = 303·256 + 7
  — service 303, Sonos Radio. But that only names services which happen to have a favorite. The
  phone app on this household also lists YouTube Music, which has no favorite here and so leaves no
  trace in `FV:2`. **How to enumerate the linked set is still unknown**: `ListAvailableServices`
  returns all 108 services Sonos knows about rather than the household's, and `/status/accounts` —
  the S1-era endpoint for exactly this — returns an empty `ZPSupportInfo` on this firmware. Add it
  to the list below.
- Note the household difference: the office household has **3** favorites and one linked service.
  The 41-favorite count and the `Svc51463` token recorded elsewhere in this document are the home
  household. Numbers in this document are per-household; the mechanisms are not.

### What still stands

- **UPnP `Search` does not exist.** `GetSearchCapabilities` returned an empty `SearchCaps` again on
  2026-08-31. Search will not come from `ContentDirectory`.
- `musicService:1 search` returns `ERROR_UNSUPPORTED_COMMAND`. Re-probed 2026-08-31 with the new
  `x2rock raw`, and it holds — but the reason was misread the first time: the namespace resolves to
  `musicServiceAccounts:1` and is about accounts, not content. See "`x2rock raw`, and what it found
  in the account namespaces".

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
3. **How to enumerate the household's linked services.** Needed by any search UI worth using — a
   list of 108 services to pick from is not a feature. `/status/accounts` is gone; the answer is
   probably in `musicService:1` too, which makes it the same first probe as (1).
4. **Whether a service track can be enqueued** — the experiment named on 2026-08-29 and never run,
   because every favorite on the home household is a container or a station. Still the right test,
   and cheaper now: Sonos Radio search returns stations, and stations from that service are already
   known to play here.

`playbackSession:1` `loadStreamUrl` and `loadCloudQueue` are not a way *around* `AddURIToQueue`'s
refusals — they are what Sonos intends instead of it, and both are confirmed present on the LAN.
See "What Sonos's own sample app settles", which also has their full contracts.


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

1. **Which services the picker should offer, once there are more than one** (opened 2026-08-31,
   replacing "`x2rock browse`", which is built — see "`browse`, and the picker that stopped being
   about favorites"). `searchService` names a single service and `browseServices` an array, both by
   hand in `shell.json`. With 32 anonymous services and a growing number of linked ones, naming them
   by hand is the part that will not scale, and the widget has no way to ask what is linked. The
   pieces to build with are there: `x2rock accounts --json` lists what this machine holds a token
   for, and `x2rock search`/`browse` with no argument list what is reachable. What is undecided is
   whether the picker should discover services itself or keep the configured-by-hand bargain.

   Two loose ends that block nothing. **`match`** has never succeeded — see "`match`, and why
   nothing needs it yet". **Bandcamp** stays deferred until there is something in the collection.

   The 62 app-link services remain a separate call: Google gates the endpoint on an API key before
   user auth is even reached, and protected streams need `httpHeaders` or `contentKey`, which
   `loadStreamUrl` cannot carry.

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
