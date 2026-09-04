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
    systemd/x2rock.service     # user unit for the daemon, beside logging.conf.example
                               # (a drop-in carrying the two diagnostic log flags)
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

#### The stored serial goes stale, and a bookmark cannot tell (found 2026-08-31)

A bookmark stores the account serial, and **a serial belongs to a registration, not to a service.**
Re-register and the number moves.

Observed within one day on this household: YouTube Music was `sn_3` in live playback, recorded in
the section above and hardcoded into `bookmarks.rs` tests. It is `sn_16` now. The household owner
had switched that subscription — the same person, a different plan — and the switch minted a new
registration with a new serial.

So a bookmark kept *before* an account switch names a serial that no longer exists, and nothing in
`from_id`'s refusals catches it: the id was well-formed when stored, and it stays well-formed after
the account it points at is gone. `from_id` guards the shape, and this is not a shape problem.

Nothing is broken today only by luck — every entry in `bookmarks.json` postdates the switch
(serials 15, 16 and 17). ~~The failure mode is untested, because testing it means replaying a
bookmark built on a dead serial, and no such bookmark exists to try.~~ **Tested 2026-08-31, in a
stronger form than this imagined**: the YouTube Music *registration itself* was removed, and the
entry died at `AddURIToQueue` with UPnP 800 — and the stored serial had nothing to do with it,
because the enqueue path sends none. What goes stale is the household's registration for the
service, not the number the bookmark remembers. See "The YouTube Music account was disconnected,
and the bookmark died at the door".

Two consequences beyond this feature:

- **`FV:2` serials are not all live.** A favorite embeds the serial current when it was *saved*, so
  the harvest described under "Where the registry can be read" mixes registrations with fossils.
  `sn_2` for YouTube Music is most likely one: a serial from before the switch, preserved in an old
  favorite. That makes the harvest a weaker enumeration source than that section credits — it is a
  lower bound on accounts that ever existed, not on accounts that exist.
- **The daemon records what plays regardless of whose account it came from.** Seven of the entries
  in this household's file are from an iHeartRadio session on `sn_15`, the household default,
  played from the Sonos app by someone other than whoever linked x2rock here.

  This is what `bookmarks remove` was added for (2026-08-31). Until it existed, taking something
  back out of the history meant hand-editing `bookmarks.json` — which is a poor answer for a file
  that fills itself with whatever anyone in the house plays.

~~The design question this raises, unanswered on purpose: **should a bookmark store the serial at
all?**~~ **Closed 2026-08-31, and the choice turned out to be illusory.** The trade-off as written
assumed storing a serial could *pin* a bookmark to the account it was kept from. It cannot: the
player ignores whatever serial the enqueue path carries (sid 284 with `sn=9`, never registered,
played) and resolves the household's current registration for the sid — demonstrated end to end
when the Espresso entry, recorded under `sn_16`, refused with no registration and then played as
`sn_20`, the same YouTube Premium subscription re-registered. A stored serial is provenance at
most; it cannot select an account, so there was never a resilience to give up. See "Re-added the
same day: `sn_20`, and the bookmark resurrected".

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
account and wants the `linkCode` from the browser link flow.

**This section originally read that as confirming the chain from the player's side** —
`getDeviceLinkCode` → user authenticates → `getDeviceAuthToken` returns `userIdHashCode` and the
link code → `match` registers the account and returns its id. It confirms no such thing. The
message is unconditional: it is returned identically for a service the household *has* installed
and one it has not, which was checked directly once a service could be added from the phone. See
"Four hypotheses this killed" under "The household's account registry, read at last". The chain
above may well be right — but this error is not evidence for it.

### Where this leaves YouTube Music

Search stays closed — `music.googleapis.com` answers 403 without Sonos's own encrypted API key, and
that key is not something to go after. (That instinct held up: the key turned out to be sealed in
an RSA/AES envelope opened only by the controller app and player firmware. See "The YouTube Music
`apiKey` is sealed, and that closes the question".) But the ids are readable and a track will
enqueue, so the shape of a feature x2rock *could* have is: **remember what played and start it
again**, referencing
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

Google gates the endpoint on an API key before the question of user auth even arises. Note the
error text is generic Google API-gateway boilerplate, not a Sonos-specific check — the same refusal
any Google endpoint gives with key enforcement on.

**Two claims that stood here have since been corrected — both amended 2026-09-01.**

*On the handoff.* This said app-link services expect the Sonos app to launch the *service's* mobile
app, so Linux would be stuck on a browser fallback a service need not offer. Wrong, or at least
unsupported: **the Sonos PC controller completes app-link too**, so any service that links on a
desktop Sonos client must serve a non-mobile flow. The browser path is load-bearing for a shipping
Sonos client, not a courtesy. That removes a wall this document claimed for two sessions.

*On the key.* This said the key "is not secret from us" because the manifest carries an `apiKey`
object with `cr` and `zp` values, making the 403 "probably answerable" and the rest a judgement
call. **The manifest carries no key.** It carries two encrypted envelopes — RSA-1024-wrapped,
AES payload, tagged with the fingerprint of a private key held in the controller app and the player
firmware. There is no string to decide about presenting. See "The YouTube Music `apiKey` is sealed"
below for the bytes, the entropy controls, and the decision that closes it.

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
controller at all.

That reframing was itself doubted and then restored on the same day, which is worth recording because
the doubt was reasoned and wrong. Mixcloud could be searched, browsed and handed to a room and would
not play, and the chain "its stream needs service-side key derivation → the player must resolve it →
the player needs a credential → the account must be registered" was written here as fact. It was
not: Mixcloud's refusal was **a missing percent-encode in x2rock's own URI**, and it plays with no
`match`, no account serial, and nothing registered by this tool. See "Found: it was a missing
percent-encode, and Mixcloud plays".

**The standing position, three services in: `match` is needed for nothing yet.** Search, browse and
playback all work without it, including playback of content whose stream x2rock cannot resolve
itself. It is still attempted on every link and has never once succeeded. Anything that appears to
need it should be suspected of being a bug on this side first — that is now the historical record
twice over.

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
that talking to a service never enters the daemon covers this too.

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

### There *is* a wall, and it is nowhere near where this document put it

> **Superseded — read "Found: it was a missing percent-encode, and Mixcloud plays" below.** The
> diagnosis in this section is sound about `loadStreamUrl` and wrong about Mixcloud being unplayable;
> the enqueue path plays it. Kept because the reasoning that overshot is instructive.

**Mixcloud search and browse work. Mixcloud playback does not.** `x2rock browse -s Mixcloud
trending --play 1` is accepted, the room takes the item and shows its title, and then the player
oscillates `BUFFERING` → `IDLE` with `positionMillis: 0` and `canPause: false`. It never starts.

`getMediaURI` is not the problem and neither is authentication. It returns a plain HLS URL with no
`httpHeaders`, no `contentKey`, no `deviceSessionKey` — and every part of that stream is publicly
fetchable without a credential: the master playlist answers 200, the variant answers 200, and a
segment answers 200 with 90 KB of audio. The problem is one line inside the playlist:

```
#EXT-X-KEY:METHOD=AES-128,URI="data:text/plain,/secure/hls_aes128/2/2/1/8/1268-...-f1b97737e761.m4a"
```

That `data:` URI's payload is a **63-byte path string**. AES-128 keys are 16 bytes. So a
standards-compliant HLS client fetches the key, gets something that cannot be a key, and stalls —
which is exactly what the speaker does. Mixcloud's own player evidently derives the real key from
that path; nothing else can.

Three corrections follow, and they matter more than the bug did:

1. **A plain URL from `getMediaURI` does not mean playable.** This document has been treating the
   absence of `httpHeaders` as the all-clear since "Auth is not the last wall" was written. It is
   not: the wall can sit *downstream of the URL*, inside the stream's own encryption, where no SMAPI
   field will warn you. Two services played, so the inference looked safe. It was luck.
2. **`contentKey` and `deviceSessionKey` are not a legacy path** — the paragraph this replaces
   guessed they were. Mixcloud is precisely a service whose stream needs service-side key
   derivation, and it sends neither, which is much better explained by **Sonos not playing Mixcloud
   through `loadStreamUrl` at all.**
3. So **`match` is back in play**, having been demoted an hour earlier. See below.

### What this does to `match`

The entry above concluded that nothing needs `match`, on the evidence that iHeartRadio's stream URL
carries its own identity and plays. Mixcloud is the counterexample: a service whose content this
project can find, name and hand to a room, and cannot make a sound with.

The mechanism that would work is already documented here and already shipping — for a *different*
feature. `bookmarks` enqueues service content by building DIDL with a
`SA_RINCON<type>_X_#Svc<type>-0-Token` cdudn, where **the literal trailing `Token` is a placeholder
the player resolves against its own stored credential.** That is how a kept YouTube Music track
plays while x2rock holds no YouTube credential at all: the player fetches the stream, not us.

Which suggests the real division of labour, and it is not the one built:

- `loadStreamUrl` works when the URL is genuinely self-sufficient — free radio, and anything whose
  segments need nothing.
- **Enqueueing with a cdudn** is what a service-side-encrypted stream needs, because it makes the
  *player* resolve the media, and the player is the thing with a service credential the service
  itself will honour.
- And the player only has that credential if the account is registered on the household — which is
  what `match` is for.

### The experiment, run (2026-08-31)

Done by hand-writing entries into `bookmarks.json` and using the shipping `x2rock bookmark` path, so
no code was changed to get the answer.

| # | URI / DIDL | Result |
|---|---|---|
| A | `cloudcast:2191563811`, sid 181, sn=1 | `AddURIToQueue` → **UPnP 800** |
| B | same, sn=2 | UPnP 800 |
| C | `cloudcast%3a2191563811`, sid 181 | UPnP 800 |
| **control** | the YouTube Music id that already works, sid 284, sn=2 | **plays** |
| D | YouTube Music id, sid 284, **sn=9** — a serial the household does not have | **plays** |
| **E** | **YouTube Music id, sid 181** — the working id under Mixcloud's service | **UPnP 800** |

D and E are the whole answer. **`sn=` is not load-bearing** — a serial that cannot exist still
plays, so the player is not resolving the account from it. And E changes *only* the service: the
identical object id that plays under 284 is refused under 181. So the refusal is about **the service
account, not the id shape**, and the earlier suspicion that `cloudcast:`'s colon was to blame is
wrong.

What was concluded from this, and why it was wrong:

The reading at the time was that D and E together isolated the *service account* — `sn=` ignored, and
the same id refused when only the service changed — which made `match` the blocker and the most
important unsolved problem in the project. That was published and is **withdrawn**.

**Test A was re-run with Mixcloud signed in through the Sonos app**, so the household genuinely holds
the account — the condition `match` would have created. It returned **UPnP 800 again, unchanged.**
The `loadStreamUrl` path still stalls at `IDLE`, and `getMediaURI` returns the identical URL. The
household's knowledge of the account changes neither path, so the account was never the blocker.

**Test E was confounded, and that is the part worth keeping.** Putting a YouTube Music object id under
service 181 changes two things, not one: the account *and* whether the id means anything to the
service being asked. A Mixcloud endpoint handed a YouTube object id has every reason to refuse it on
its own terms. E could not separate "wrong account" from "meaningless id", and it was reported as
though it could — a two-variable experiment written up as a one-variable one.

### Found: it was a missing percent-encode, and Mixcloud plays

`x2rock keep` on a Mixcloud show playing from the phone app harvested this:

```
object_id  cloudcast:2191051074      account  4
art_url    .../getaa?s=1&u=x-sonosapi-hls-static%3acloudcast%253a2191051074%3fsid%3d181%26flags%3d8232%26sn%3d4
```

The art URL leaks the player's own playback URI, and `GetPositionInfo` confirms it verbatim:

```
x-sonosapi-hls-static:cloudcast%3a2191051074?sid=181&flags=8232&sn=4     the player
x-sonosapi-hls-static:cloudcast:2191051074?sid=181&flags=65544&sn=4      what x2rock built
```

Two differences, and a four-way test isolated which one mattered:

| | colon encoded | colon raw |
|---|---|---|
| `flags=8232` | **accepted, plays** | refused, UPnP 800 |
| `flags=65544` | **accepted, plays** | refused, UPnP 800 |

**The colon. That is the whole bug.** The object id sits between the scheme and the `?`, where a URI
parser reads a colon as structure, and `uri()` never escaped it. Every id this had ever been tested
against came from YouTube Music — alphanumeric with `_` and `-` — so the encoding was a no-op and
its absence invisible. Fixed by percent-encoding the id in both the URI and the DIDL `id`, lowercase
hex as the player writes it, with a test pinning that a YouTube id comes through byte-identical.

`x2rock bookmark` now plays the Mixcloud show through the shipping path, and YouTube Music is
unregressed.

### Which corrects rather a lot

- **The hypothesis in the previous section was wrong.** `cloudcast:2191051074` *is* the object id —
  the player uses exactly that string. SMAPI ids and DIDL object ids are the same namespace here, and
  the `00032020` class prefix was right all along.
- **`flags` is not load-bearing.** The player uses `8232` for Mixcloud and `65544` for YouTube Music,
  and both values enqueue *and play* Mixcloud. Left at `65544` rather than guessed per service.
- **Neither is the account serial.** `sn=9`, a serial the household cannot have, plays fine.
- **"Mixcloud playback does not work" was wrong.** Mixcloud plays. What does not work is
  `loadStreamUrl`, and the reason is the nonstandard AES-128 key documented above — a 63-byte path
  where 16 bytes of key belong. The enqueue path sidesteps it entirely by making the *player* resolve
  the media, which is precisely what the cdudn is for.

### The design consequence, built (2026-08-31)

The guess in the paragraph this replaces — "use the enqueue path for anything from a service" — was
half right, and the half that was wrong matters: **neither mechanism subsumes the other.**

| | on-demand track (Mixcloud) | live stream (iHeartRadio) |
|---|---|---|
| `loadStreamUrl` | accepted, then **silently `IDLE`** | plays |
| `AddURIToQueue` + cdudn | plays | **refused, UPnP 800** |

A station is not queue material and the player says so outright; on-demand content whose stream
x2rock cannot resolve needs the player to resolve it, which only the queue path arranges. So
`play_item` now splits:

- **`kind == "stream"` → stream it.** `search`/`browse --json` already report `type`, and the widget
  passes it as `--kind`.
- **anything else, when the service has a cdudn → enqueue it**, and **fall back to streaming on any
  refusal.** The refusal is the player telling us this is not queue material, and it arrives
  immediately.
- **No cdudn → stream.** A service absent from the player's type list has no account to name, and
  `SA_RINCONNone_X_#SvcNone-0-Token` is not one. TuneIn is such a service, and its content is
  streams anyway.

**The fallback only works in that direction, which is why the order is what it is.** An enqueue
refusal is immediate and legible; `loadStreamUrl`'s failure is silent, arrives seconds later as
`IDLE`, and nothing in the reply distinguishes it from success. There is no fallback to build on top
of a mechanism that does not report failure.

Verified all three ways: a Mixcloud `cloudcast:` plays (it did not before), an iHeartRadio
`live_stations.` with `--kind stream` plays, and the same stream with **no** `--kind` tries the queue,
is refused, says so on stderr, and plays.

Two things dropped out of it. `sn=` is not sent at all on this path — nothing here has ever played,
so there is no serial to harvest, and the player does not need one: the real serial, a wrong one and
no `sn=` were each accepted and each played. And `bookmarks::service_uri`/`service_didl` now back
both `bookmark` and `play-item`, so there is one place where a service playback URI is built rather
than two that could drift.

Also fixed while here: UPnP 800 was glossed as "no such position in the queue", which is true of
`Seek` and of nothing else. 800 is UPnP's *undefined* error code, and that gloss confidently narrated
first a percent-encoding bug and then a stream that cannot be queued. It now says the player refused
without giving a reason, which is all anyone knows.

Worth noting what made this findable: **the player will tell you its own answer.** `keep` harvests
what the player itself built, and `GetPositionInfo` prints it. Three hypotheses were argued from
first principles and all three were wrong; one look at the player's own URI settled it.

### The hypothesis this replaced (wrong, kept for the shape of the mistake)

It was argued that **`cloudcast:2191563811` is not a DIDL object id at all.** It is an
*SMAPI* id, and nothing has ever established that the two namespaces are the same:

- The YouTube Music object id that works — `ALkSOiGTPQu20Hqb...` — was never obtained from SMAPI. It
  was harvested from the **player's own `r:resMD`** while the track played, which is what `x2rock
  keep` does and the only way this project has ever acquired a working one.
- `didl()` hardcodes the item-class prefix `00032020`, derived from that same single YouTube
  observation. A Mixcloud cloudcast may not be that class.

So both halves of the enqueue URI may be wrong for Mixcloud, and neither was ever verified against
anything but YouTube Music.

The step it called for was right even though the reasoning was not: play a Mixcloud show from the
app, then `x2rock keep`. The shapes turned out to be **identical**, and the answer was in the same
harvested record — one field over, in the URI the art link carries.

`match` is unexplained, still fails, and is not implicated in any of this.

One loose end worth noting: x2rock glosses UPnP 800 as "no such position in the queue", which is
wrong here. 800 is UPnP's undefined-error code and the gloss belongs to `Seek`, not
`AddURIToQueue`.

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


## The household's account registry, read at last (verified 2026-08-31)

Two questions had been open behind every `match` probe: whether a service token is scoped to the
service or to the household, and whether the household's registration is per-service or once. Both
are now answered, and neither was answered by `match` — which still has not succeeded even once.

### Token scope is per service. Settled by a control, not by a guess

The test that mattered was not "link a service and see if it searches", which passes under either
hypothesis and is the same two-variables mistake as test E. The discriminating form is *link one
service, then search a different one*:

| step | result |
|---|---|
| `search -s Mixcloud jazz`, nothing linked | `Mixcloud needs a linked account` |
| `link iHeartRadio` | linked; `match` refused as always |
| `search -s iHeartRadio jazz` | 51 results |
| `search -s Mixcloud jazz`, still unlinked | the identical error, byte for byte |

One variable moved and Mixcloud did not follow it. `credentials.json` holds exactly one entry, keyed
`6`, with its own token — so **one token per service, per machine**, as `credentials.rs` already
modelled. The `household` field on an `Account` describes the household a token was minted against;
it does not scope the token.

The stronger evidence is that the iHeartRadio search returned 51 results while its `account_id` was
`none`, `match` having failed. Search worked against a token the household had never registered. If
tokens were household-scoped that could not happen.

### Household registration is per *account*, and a service may have several

Read from the URIs in `FV:2` and `Q:0`, which carry `sid` and `sn` on every entry:

| serial | sid | service |
|---|---|---|
| sn_2 | 284 | YouTube Music |
| sn_5 | 6 | iHeartRadio |
| sn_6 | 201 | Amazon Music |
| sn_7 | 151 | *not in the 108-service catalogue* |
| sn_10 | 303 | Sonos Radio |
| sn_14 | 333 | TuneIn (New) |
| sn_15 | 6 | iHeartRadio, again |
| sn_17 | 181 | Mixcloud |

**`sid 6` appears twice.** One service, two accounts, one household. That settles the question
outright: registration is per account, not once per household, and the guess at `credentials.rs:50`
— "Sonos allows several, and choosing between them is a feature nothing has asked for yet" — is
correct and now has evidence behind it.

The serials are a household-wide counter incrementing per account added, which is why a Mixcloud
account added from the phone during this session landed at 17 rather than at 1.

**Confirmed by prediction rather than by hindsight (2026-08-31).** With the highest serial standing
at 17, the next account added from the phone — TIDAL, `sid 174` — was predicted to be `sn_18` before
it was added. It is: `getMetadataStatus` reported `accountId: "sn_18"` on the first track played
from it. **Household-wide rather than per-service** — a brand new service landing at 18 rather than
at 1 shows that much, and that half still stands. Whether the counter is *monotonic* is the half
that did not survive; see the paragraph below.

**"Not recycled" was claimed here, withdrawn, earned - and withdrawn again on 2026-09-01. It is
retired rather than re-litigated.** The history is the point, so it is left standing:

1. 17 → 18 with nothing deleted in between showed only that the counter increments. Claimed, then
   withdrawn as unsupported.
2. Re-earned by a deletion test: TIDAL (`sn_18`, the *highest* serial) removed from the phone,
   TuneIn added in its place, and the new account came back **`sn_19`**. Read as "a freed serial is
   not reused".
3. **Broken 2026-09-01.** The owner connected "TuneIn (New)" from the phone, and the station it
   played reported `accountId: "sn_5"` live, from `getMetadataStatus` on the container. Under
   high-water-plus-one a registration made that day should have been `sn_21` or above.
4. **And a second reading the same hour made it a pattern rather than an anomaly.** Virgin Radio UK
   (`sid 336`) was added next and came back **`sn_6`** - immediately after `sn_5`, from a
   high-water mark that stood at 20 the day before. Two consecutive low serials, issued in order.

5. **The discriminating test ran, 2026-09-01, and it was decisive.** Virgin Radio UK (`sn_6`) was
   removed from the phone while CBC Radio & Music held `sn_7`. Audible was then added and came back
   **`sn_8`** - stepping over the free `sn_6` rather than filling it.

**One model fits every live reading: `next = highest currently-live serial + 1`.** Not a persisted
counter, and not a free-slot scan. Derived each time from the maximum serial the household is
actually holding:

| highest live before | added | serial | fits |
|---|---|---|---|
| 17 | TIDAL | `sn_18` | yes - the prediction that started this |
| 19 | YouTube Music, re-added | `sn_20` | yes |
| 4 | TuneIn (New) | `sn_5` | yes |
| 5 | Virgin Radio UK | `sn_6` | yes |
| 6 | CBC Radio & Music | `sn_7` | yes |
| 7 *(`sn_6` freed, `sn_7` still live)* | Audible | **`sn_8`** | **yes - and lowest-free predicted 6** |

**This reconciles the whole argument, and shows which half of the old claim was wrong.**

- **"Not recycled" was right.** `sn_6` was free, known to be free, and skipped. That is a stronger
  result than the deletion test that first earned the claim, because the freed slot was *low* and
  the gap was unambiguous.
- **"Monotonic" was wrong**, and it is the half nobody tested. The counter is not persisted, so it
  **goes down** when the highest accounts are removed. That is the whole of the `sn_20` to `sn_5`
  drop between 08-31 and 09-01: services were cleared from the household in between, the highest
  live serial fell to 4, and the next registration took 5. No recycling was involved.

So a serial is unique among *live* registrations and says nothing across time. `sn_5` yesterday and
`sn_5` today can be different accounts on different services, and that is exactly what happened -
the 08-31 harvest attributed `sn_5` to iHeartRadio.

**The one observation that does not fit**, kept rather than filed away: TIDAL at `sn_18` was
removed and the next account was `sn_19`, where this model wants 18. The two happened minutes
apart, so a removal that had not yet settled would account for it - but that is an explanation, not
a measurement, and re-running it deliberately with a pause is what would close it.

**What would falsify the model:** add a service while the highest live serial is `N` and get
anything other than `N+1`.

**Why step 2 was weaker than it looked.** Removing the highest serial and getting highest-plus-one
is exactly what "next = high-water + 1" predicts, so it never distinguished "freed serials are
never reused" from "the high-water mark only moves up". `sn_5` is a *low* serial, freed long ago if
it was ever live, and it came back.

**No simple model survives all of it.** "Lowest free index" does not fit either: the 08-31 harvest
found gaps at 1, 3, 4, 8, 9, 11-13 and 16, so a lowest-free allocator would have answered `sn_1`
rather than `sn_18` when the prediction was made. Two successful predictions came out of a model
that the next observation contradicts, which is the shape of a rule fitted to too little data.

**The likeliest reason it kept flip-flopping is right above this paragraph: the harvest reads
fossils.** `FV:2` and `Q:0` carry the serial that was current when a favorite was *saved*, so the
table is a record of past registrations mixed with live ones and no way to tell which is which. It
was never able to answer this question, and three attempts to make it do so produced two right
predictions and one wrong model. **Treat serial allocation as unknown.** Anything that needs to
know an account's identity should read it live from `getMetadataStatus`, not infer it.

The same 09-01 reading shows the fossil problem directly: that harvest attributed `sn_5` to
iHeartRadio (`sid 6`) and gave TuneIn (New) `sn_14`, while today TuneIn (New) reports `sn_5` live.
Same household throughout - one player id, one network, one household in `networks.json` - so a
second household is not the explanation.

**None of this is a correctness problem, because the consequence it was carrying had already been
closed by other means.** The claim mattered only as the benign answer to the staleness hazard: a
bookmark holding a dead serial stays a dead pointer rather than coming back to life pointing at
somebody else's account. That worry is moot either way - **the player never consults the serial on
the enqueue path**, proven when a bookmark recorded under `sn_16` played under `sn_20` (see
"Re-added the same day: `sn_20`"). A bookmark cannot pin an account deliberately, so it cannot be
silently repointed accidentally. The serial is provenance and nothing else.

TIDAL had been in x2rock's "14 services can be linked" list all day while the household held no
TIDAL account, which is the offerable catalogue and the registry being independent, once more.

**And the same test showed the harvest is volatile, not merely incomplete.** Re-running it after
the TIDAL track started, `sn_17` was *gone* — Mixcloud's account still exists, nothing removed it,
but the queue entry that was the only URI naming it had been replaced. So the visible set changes
with whatever happens to be playing. An enumeration built on this cannot be cached and cannot be
trusted to be stable between two reads a minute apart, which is a sharper limit than "only accounts
with saved content appear".

#### The harvest showed a deleted account and hid a live one, at the same time

The removal test above put the worst case on the record. Immediately after TIDAL was removed and
TuneIn added:

| | in the harvest? | actually exists? |
|---|---|---|
| `sn_18`, TIDAL | **yes**, `sid=174&sn=18` in `Q:0` | **no** — removed minutes earlier |
| `sn_19`, TuneIn | **no** | **yes** — playing at that moment |

The dead one survives because removing an account does not rewrite the queue: four Dolly Parton
tracks still name `sn_18` in their URIs. The live one is absent because it is a radio stream, and
per "The design consequence, built" a station never becomes queue material, so nothing writes its
serial anywhere the harvest reads.

So the harvest is not a lower bound on live accounts. It is **a set that overlaps the real registry
without containing it or being contained by it.** Nothing built on it should present its results as
the household's accounts, and the earlier framing of it as "a lower bound" was too kind.

`sn_14` and `sn_19` are now both TuneIn (`sid 333`) — a second service with two accounts, this one
created under observation rather than found already present.

##### One serial, followed end to end

`sn_18` was watched from birth to invisibility, which is the whole problem in one row per hour:

| moment | in the harvest? | account exists? |
|---|---|---|
| TIDAL added from the phone, a track playing | yes | yes |
| TIDAL removed from the phone | **yes** | **no** |
| Kitchen's queue cleared, ~an hour later | no | no |

**Nothing about the removal made it disappear.** It went on being reported because 25 Dolly Parton
tracks in a queue nobody was playing still named it, and it would have gone on being reported
indefinitely. What finally cleared it was an unrelated `queue clear` — a coincidence, not a
mechanism. There is no event, no expiry and no reconciliation that retires a serial from the
harvest; only the deletion of whatever content happens to mention it.

The reverse held at the same moment: `sn_19` was live and playing throughout the last two rows and
appears in none of them, because TuneIn is a station and a station never enters the queue.

So the seven pairs the harvest currently reports are not the household's accounts. `sn_2` and `sn_5`
are unverified and may be fossils of exactly the kind this table watched form — and the only reason
`sn_18` is known to be one is that its death was observed. For any serial not watched being created
and destroyed, the harvest cannot say which column it belongs in.

#### `RemoveAccount` is declared, not demonstrably functional

`SystemProperties:1` declares `RemoveAccount(AccountType, AccountID)`, and an earlier note here read
that as "there is an API route to remove an account". That is a declaration read as a capability —
the same error as reading the 108-service catalogue as the household's list. Probed:

| call | answer |
|---|---|
| `RemoveAccount` type=44551 (TIDAL), id ∈ {18, sn_18, 174} | 806 |
| `RemoveAccount` type=44551, id=999999 | 806 |
| `RemoveAccount` type=99999 (no such type), id=18 | 806 |
| `GetWebCode` type=44551, and type=0 | 800 |

A real service type holding a real account fails **identically** to a type that does not exist, so
the type is not being resolved at all. With `/status/accounts` also returning an empty
`ZPSupportInfo`, the legacy account surface looks vestigial: declared in the SCPD, gutted in the
firmware.

Stated honestly: this does not separate "the right `AccountID` was never guessed" from "the action
does nothing", because 806 covers both. What it does establish is that **removing an account from
this side is not a route to rely on** — the Sonos app removed TIDAL in seconds, and that is what the
test above used.

`registeredServicesVersion` moved across both operations, `2026-09-01T01:36:42` → `T02:07:18`,
confirming it tracks the account set rather than the catalogue.

`sid 151` is an account for a service that `ListAvailableServices` does not list. The 108-service
catalogue is what a household may *add*, not what it has, and it is not a superset of what it has.

### Where the registry can be read, and what that method cannot tell you

`S:` browses empty on this firmware, `/status/accounts` returns an empty `ZPSupportInfo`, and
`musicServices:1` is not a namespace the player answers. `musicServiceAccounts:1` has no read
command at all — `getAccounts`, `getMusicServiceAccounts`, `list` and `getAccountList` are each
`ERROR_UNSUPPORTED_COMMAND`. What works is harvesting `sid`/`sn` out of `FV:2` and `Q:0` URIs, and
`playbackMetadata:1 getMetadataStatus` reports the live one directly as
`id.accountId: "sn_17"`.

Two limits, both load-bearing for anything built on this:

- **It is a lower bound, not the registry.** Only accounts with favorites or queue entries appear.
  An account nothing has saved from is invisible.
- **It cannot distinguish live entries from dead ones.** `sn_5` is an iHeartRadio account the
  household still lists and nothing plays from.
- **Some of them are fossils, not accounts.** A favorite embeds the serial current when it was
  saved, so a serial can outlive the registration it named. See "The stored serial goes stale, and
  a bookmark cannot tell" — on this household `sn_2` for YouTube Music is most likely one.

#### Searched properly: there is no listing anywhere (2026-08-31)

The first pass tried four command names and concluded "no read command". That was a guess dressed
as a search. Done properly, against a real player, every route fails:

| route | answer |
|---|---|
| `musicServiceAccounts:1` — `getAccounts`, `getMusicServiceAccounts`, `list`, `getAccountList`, `getAll`, `getVersion`, `getHouseholdAccounts`, `refresh` | `ERROR_UNSUPPORTED_COMMAND`, each |
| `musicServices:1` (plural) | `ERROR_UNSUPPORTED_NAMESPACE` |
| `musicService:1 getSessions`, `households:1 getHousehold` / `getHouseholds` | `ERROR_UNSUPPORTED_COMMAND` |
| UPnP `MusicServices:1`, from its own SCPD | three actions, and none of them lists accounts: `ListAvailableServices`, `UpdateAvailableServices`, `GetSessionId` |
| UPnP `SystemProperties:1`, from its own SCPD | account **mutations** only — `AddAccountX`, `AddOAuthAccountX`, `RemoveAccount`, `ReplaceAccountX`, `SetAccountNicknameX`, `EditAccountMd`, `RefreshAccountCredentialsX` |
| `SystemProperties GetString` on `R_SvcAccounts`, `R_Accounts`, `AccountList`, `Accounts`, `R_TrialAccount`, `sonos_accounts`, `McAccountsVersion` | UPnP 800 |
| ContentDirectory `S:` | empty result, 0 matches |
| `http://<player>:1400/status/accounts` | empty `ZPSupportInfo` |

Reading the SCPDs rather than guessing verbs is what makes this a search and not another four
guesses: `/xml/MusicServices1.xml` and `/xml/SystemProperties1.xml` enumerate every action the
player implements. **A Sonos player will add, remove, rename and re-credential an account, and will
not tell you which accounts exist.**

So the harvest above is not a stopgap until the real call is found. It is the only route.

#### The published reference confirms it (checked 2026-08-31)

The handoff note flagged the Control API row of the table above as the weak half of this result:
eight guessed command names, with no Sonos documentation read in that session. Checked against the
current published reference at `docs.sonos.com` (the `llms.txt` index enumerates every reference
page; the `match` page embeds the live Control API OpenAPI spec, `v1.55.0-alpha.16-production-cloud`,
updated 2026-06): the `musicServiceAccounts` namespace documents **exactly one command, `match`**,
plus the `MusicServiceAccount` object it returns. The only path under the namespace in the spec is
`/households/{householdId}/musicServiceAccounts/match`.

That agrees with the 53-path `OAS_production.json` from Sonos's sample app, which an earlier
session had already read (see "What Sonos's own sample app settles") — the negative result is now
confirmed against both the shipped spec and the live one. And SMAPI offers no way around it: its
published verb list (`docs.sonos.com/docs/smapi`) is the *service-side* interface — auth, browse,
playlists, reporting, `getMediaURI` — and nothing in it lists a household's accounts either.

There is no listing verb left to be guessed at. This section stops being provisional, and
`accounts --household` rests on a sound premise.

#### But the household does say *when* accounts change

`musicServiceAccounts:1 subscribe` succeeds — an empty reply, then an event:

```json
{ "_objectType": "musicServicesChanged",
  "availableServicesVersion":  { "version": "2234" },
  "registeredServicesVersion": { "version": "2026-09-01T01:36:42.389777473" } }
```

**`registeredServicesVersion` is a second version, and it tracks the accounts.** A timestamp rather
than a counter, and it matched when TIDAL was added from the phone minutes earlier.

This corrects the claim under "Four hypotheses this killed" that an account being added is
undetectable. What is true is narrower: `AvailableServiceListVersion` is blind to it — the number
the *catalogue* cache keys on did not move. `registeredServicesVersion` is not blind to it. Anything
caching a harvested account table has a signal to invalidate on, which is worth knowing before
building one, and it arrives by subscription rather than by polling.

`x2rock raw --watch <seconds>` is how this was read: a `subscribe` reply is empty and the state it
asked for turns up afterwards as an event.

### Choosing between several accounts is an open question

Told from the phone to play iHeartRadio into Kitchen, the household played from **`sn_15`** — the
higher of the two serials, and the more recently added. One observation cannot separate "highest
serial wins" from "the app's remembered default, which happens to be the newest here"; the two
predict identically here. Distinguishing them needs a household whose *older* account is the active
one, which is not worth manufacturing.

This matters more than the taxonomy does. Knowing the household holds an account for a service is
not enough to use it — with several present, choosing wrong plays from the wrong account.

### The two playback paths use two different identities (verified 2026-08-31)

Worse than "choosing wrong" — x2rock does not choose at all, and the two paths in the table under
"The design consequence, built" end up on *different accounts*.

`main.rs:865` passes `None` as the serial, so the enqueue path names no account. The player fills
one in. Enqueuing an iHeartRadio podcast episode and reading `Q:0` back:

```
podcast_show.96972136.101839702.mp3?sid=6&flags=8&sn=15
```

**`sn_15`** — the household's default for that service, and the same account the Sonos app played
from. Meanwhile `loadStreamUrl` carries x2rock's own token, whose `userIdHashCode` is
`13012528881`; iHeartRadio's browse tree names it outright in a container id,
`my_playlists_13012528881`. So:

| path | content | plays as |
|---|---|---|
| `loadStreamUrl` | stations | x2rock's linked token |
| enqueue + cdudn | on-demand | the household's default serial |

Which identity a request runs as is therefore decided by **the content type**, which is not a
property anyone would expect to select an account. On a household where the accounts belong to
different people — two here, the household's default being someone other than whoever linked
x2rock on this machine — listening history lands on whichever person the content type happened to
pick.

The earlier note that "a wrong `sn` and no `sn` were each accepted and each played" is still true
and was read too comfortably: the player accepting an omitted serial does not mean the serial does
not matter. It means the player substitutes one, silently, and that substitution has an owner.

Not a bug with an obvious fix — naming x2rock's own account on the enqueue path requires a serial
this tool cannot mint, since `match` has never succeeded, and the household's registry maps
services to serials but nothing maps a serial back to an identity from the controller side. Worth
recording before anything is built on the enqueue path.

YouTube Music also holds two accounts: `sn_2` in `FV:2`, `sn_16` on a queue item. Several accounts
per service is not an iHeartRadio quirk.

### Four hypotheses this killed, three of them this document's

- ~~`match` refuses because the service is not installed on the household~~. The "guest account"
  wording invited this reading, and the section "`match` wants a link code" concluded from it that
  the message "confirms the chain from the player's side". It does not confirm anything. After
  Mixcloud was added from the phone, `match` for Mixcloud returned **the identical refusal**, word
  for word, as `match` for iHeartRadio which was not added — with a link code and without one. The
  message is unconditional parameter validation and says nothing about installation state.
- ~~`AvailableServiceListVersion` will move when an account is added~~. It did not: `:2234` before
  and after. That version tracks the offerable catalogue only, so **the catalogue cache cannot
  detect an account being added or removed**, and the invalidation note under "The catalogue cache"
  should not be read as covering account state. (Narrowed later the same day: *that* version is
  blind to it, but `registeredServicesVersion` on the `musicServicesChanged` event is not — see
  "But the household does say *when* accounts change". The catalogue cache is still blind; the
  household is not.)
- ~~The single-use-code theory can be tested by calling `match` with an unredeemed code~~. It cannot,
  by anyone, from a controller. `match` requires a `userIdHashCode`; the hash arrives only from
  `getDeviceAuthToken`; that call is what redeems the code. **A valid hash and an unspent code cannot
  coexist.** The theory recorded in "`match`, and why nothing needs it yet" is not disproved — it is
  unfalsifiable by this route, which is a different and more useful thing to know about it.
- ~~The household is the thing that authorizes search~~. It is not; see the 51 results above.

### What this does to `x2rock accounts`

`x2rock accounts` prints `not registered on the household` for iHeartRadio while the household holds
**two** iHeartRadio accounts. The line is true about `match` and misleading as output: it reports on
this machine's registration attempt, not on the registry, and the registry is readable. Today's link
minted a third iHeartRadio identity that only this machine knows about.

### The standing position, restated

`match` remains needed for nothing, and the reason is now sharper than "nothing has needed it yet":
**the registration it performs is something the Sonos app does perfectly well**, and the result is
readable afterwards. A household that adds a service from the phone has an account serial x2rock can
find. The open work is not making `match` succeed; it is enumerating accounts without depending on
saved content, and deciding which account to use when a service has more than one.


## Rule: talking to a service never enters the daemon (decided 2026-08-31)

Talking to music services is allowed. Breaking the parts that do not need the internet is not.
Losing a name lookup must never cost the household its transport or its volume.

Written when `search` was the only command that left the LAN. **`browse` and `link` now do too**, and
the rule covers them unchanged: each is a CLI command with its own `Process` behind it, and none of
them is reachable from the daemon. Read "search" below as "any call to a music service".

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
  a fault in the widget. `browse` followed the same shape when it was built, with a `Process` of its
  own, for exactly this reason.

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

### `--scope player` was addressing the right player over the wrong socket (fixed 2026-08-31)

`--scope group` reconnects to the group's coordinator before sending, because only the coordinator
answers for a group. `--scope player` set `playerId` and did not do the equivalent, so a
player-scoped probe went out over whichever socket the session happened to open. The player it
reached answered:

```
ERROR_INVALID_OBJECT_ID — "Incorrect playerId"
```

for an id that was perfectly correct — it simply was not *that* player's id. Every player-scoped
namespace was unreachable without also passing `--ip` for the same room named in `--room`, and the
error pointed at the id rather than at the connection. `playerVolume:1 getVolume` on Living Room
failed with the default connection and succeeded with `-i 192.168.86.25`, which is what isolated
it. Now it opens a connection to the named player, mirroring the group branch.

**A player answers player-scoped commands only for itself.** That is the general fact, and it is
the same shape as the coordinator rule one level down.

### Which scope each namespace wants (verified 2026-08-31)

Sent `subscribe` to each namespace at all four scopes and recorded which one the player accepted:

| Scope | Namespaces |
|---|---|
| `group` | `playback:1`, `playbackMetadata:1`, `groupVolume:1` |
| `player` | `playerVolume:1`, `homeTheater:1`, `audioClip:1` |
| `household` | `groups:1`, `favorites:1`, `playlists:1`, `musicServiceAccounts:1` |

The wrong scope always answers `ERROR_MISSING_PARAMETERS` naming the key it wanted — `Missing
groupId`, `Missing playerId`, `Missing householdId` — so the error says which scope to use. Two
namespaces answer `ERROR_MISSING_PARAMETERS` at *every* scope: `playbackSession:1`, which wants
`--session`, and `settings:1`, whose required parameter is still unidentified.

The target key lives in the **header**, so putting `groupId` in `PARAMS` does nothing; the body is
only ever the command's own parameters. This is now in `raw --help`, with the table and five
worked examples, because the tool is driven far more often by an agent reading `--help` than by a
person who remembers last week's probe.

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

### `GetSessionId`: answered, and it answers nothing (closed 2026-08-31)

**Do not spend the ten minutes this section used to ask for.** It was finally run against four
services on one household, with an empty `Username`:

| Service | State on this household | `GetSessionId` |
|---|---|---|
| YouTube Music (284) | linked in the Sonos app, **plays today** | UPnP **806** |
| Mixcloud (181) | signed in through the Sonos app | UPnP **806** |
| iHeartRadio (6) | linked by x2rock only | UPnP **806** |
| Bandcamp (157) | linked by x2rock only | UPnP **806** |

**806 for the service that demonstrably works.** So the call does not distinguish a registered
account from an absent one, and it cannot be used to ask "does this household hold an account for
service N" — which was the only remaining use anyone had for it. The earlier guess that 806 means
"this service does not do sessions" survives, and `getSessionId` being the legacy
username-and-password path explains why every modern service refuses it.

The question behind it — whether any read path to a household's `loginToken` exists — is not answered
here and has no other candidate. Treat it as closed by exhaustion rather than by proof.

**There is still no way to enumerate a household's linked services.** Working backwards from
favorites' `cdudn` values remains the only method, and it only finds services something was saved
from.

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
  the same Beam read 2.0, then 5.1, then 2.0 again as the content changed.
- **The LFE count arrives as `numLFEChannels`, in caps.** It is the one field
  here that `serde(rename_all = "camelCase")` gets wrong - the derived name is
  `numLfeChannels`, so the field needs an explicit `rename`. With `default` on
  it this failed silently: every layout lost its `.1` and read 5.0, and a 2.1
  source looked like plain stereo to `is_surround`.
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

## Where this stopped (2026-08-31, end of session)

A handoff note, written because the next session is on a different account and inherits nothing but
this repository.

### Settled today, and where it is written up

- **A service token is scoped to the service**, per machine. `credentials.json` holds one entry per
  service id. Search never consults the household — iHeartRadio returned 51 results while its
  `account_id` was `none`.
- **Household registration is per account**, and one service may hold several: `sid 6` held `sn_5`
  and `sn_15` simultaneously, TuneIn now holds `sn_14` and `sn_19`.
- **Serials are `highest currently-live serial + 1`** - settled 2026-09-01 after three sessions of
  argument. **"Not recycled" was right**: `sn_6` was freed by removing Virgin Radio UK and the next
  account, Audible, took `sn_8` rather than filling it. **"Monotonic" was wrong**: the counter is
  derived, not persisted, so it *falls* when the highest accounts are removed - which is the whole
  of the `sn_20` (08-31) to `sn_5` (09-01) drop that looked like recycling. A serial is unique
  among live registrations and means nothing across time. Read an account live rather than
  inferring it from a favorite, which may be a fossil. See "Household registration is per
  *account*".
- **The two playback paths run as different identities.** `loadStreamUrl` uses this machine's token,
  the enqueue path lets the player substitute the household default. Content type selects the
  account, which is nobody's intent.
- **`match` has still never succeeded**, and is beside the point: the Sonos app performed four
  registrations today in seconds each.

All of it is in "The household's account registry, read at last" and the sections after it.

### The one thing that could undo a chunk of the above

**No Sonos or SMAPI documentation was consulted in this session.** Everything came from this repo,
live probing, and recollection. That splits the negative result in "Searched properly: there is no
listing anywhere" into two halves of very different strength:

- The **UPnP half is authoritative** — `/xml/MusicServices1.xml` and `/xml/SystemProperties1.xml`
  were read directly, and they enumerate every action the player implements.
- The **Control API half is guesswork** — eight command names on `musicServiceAccounts:1` that
  seemed plausible. If the real listing verb is one nobody thought of, "there is no listing" is
  wrong, and both that section and `accounts --household` rest on a false premise.

**Checking that against Sonos's published API reference is the first thing worth doing.** If a
listing command exists, `accounts --household` should use it and the harvest sections need
rewriting; if it does not, the section stops being provisional.

**Resolved 2026-08-31, by the inheriting session.** The published reference documents exactly one
command in `musicServiceAccounts` — `match` — and the live OpenAPI spec embedded in its reference
page carries a single path under the namespace. No listing verb exists in the published Control
API, and the SMAPI verb list is service-side and lists nothing either. "There is no listing
anywhere" stands in full, and with it `accounts --household`. Details under "The published
reference confirms it" in the harvest section.

### Also unfinished

- **Account selection when a service has several is unresolved.** Both observed cases used the
  higher serial, but that is confounded with "the one most recently set up". Separating them needs a
  household whose *older* account is active.
- **Whether a bookmark should store the serial at all** — see "The stored serial goes stale". A
  trade-off, not an oversight, and deliberately left open. *(Closed 2026-08-31 by the inheriting
  session: the choice was illusory — the player never consults the serial on the enqueue path, so
  a bookmark cannot pin an account even deliberately. See "Re-added the same day: `sn_20`".)*
- **`RemoveAccount` is declared but not demonstrably functional**, and 806 cannot distinguish a bad
  `AccountID` from a dead action. Removal was done from the phone instead.

### Live state changed on the household today

So the next session is not confused by it: TIDAL was added and then removed (`sn_18`, gone), Mixcloud
was added and left in place (`sn_17`), TuneIn was added (`sn_19`), Kitchen's queue was cleared of 25
TIDAL tracks that were unplayable anyway, and one podcast episode was enqueued to Living Room and
removed again. The iHeartRadio token this machine holds was linked today and is the same account as
the earlier session's (`13012528881`).

## The picker discovers linked services (decided 2026-08-31)

Open question 1 asked whether the picker should discover services itself or keep the
configured-by-hand bargain. Decided: **both, split along the line of who chose the service.**

- **Linked accounts are discovered.** `x2rock link <service>` is already an act of configuration —
  a deliberate, per-machine statement that this household uses the service — so requiring the same
  name typed a second time into `browseServices` configured nothing; it was only an opportunity for
  the picker and the credentials file to disagree, and its failure mode was real: a freshly linked
  service that never appears because nobody knew about the key. On picker open the widget now reads
  `x2rock accounts --json` — one local file; no player, no service, no daemon — and gives every
  linked account a Browse row after the `searchService` one, deduplicated case-insensitively,
  sorted by name.
- **Anonymous services stay configured by hand.** Nobody chose the 32 anonymous services, and a
  picker that lists a catalogue answers a different question from "what should this room play".
  `searchService` (default TuneIn) and an explicit `browseServices` remain how one of those earns a
  row.
- **An explicit `browseServices` wins verbatim**, `[]` included. The bargain was not removed; it
  became an override.
- **Search is untouched: one row, one configured service.** A search fanning out across N services
  is a different feature — the CLI takes one service per call, and interleaving N result lists is a
  design nobody has asked for. If it is ever wanted, the discovered list now exists to build from.

Mechanics worth recording: the accounts read re-runs on every picker open, like favorites, so a
`x2rock link` run in a terminal reaches the picker without a shell restart. Failure is silent and
falls back to exactly the old default — a row for `searchService` — because the picker with no
discovery is the picker as it was. And a linked account whose token has gone stale still gets its
row: the failure surfaces only when the row is opened, confined to `browseProc`, which is "one
failing container must not fail the listing" again at the level of services.

## The YouTube Music account was disconnected, and the bookmark died at the door (2026-08-31)

The household owner disconnected YouTube Music from the Sonos app on a phone. What that did,
observed the same day, from Kitchen (idle, empty queue, volume 3):

| enqueue attempt | household's account for that service | result |
|---|---|---|
| Espresso, YouTube Music (sid 284) — the id that enqueued and played yesterday | removed today | `AddURIToQueue` **UPnP 800** |
| Jolene, TIDAL (sid 174) | removed yesterday (`sn_18`) | `AddURIToQueue` **UPnP 800** |
| L-O-V-E, iHeartRadio (sid 6) — the control | still registered | enqueued and **played** |

- **The player validates the URI's service against the household's registrations at enqueue
  time.** Yesterday's DIDL experiment showed the serial is *not* checked — sid 284 with `sn=9`, a
  serial the household never had, played. Today shows what *is* checked: the same sid with its own
  valid id and no registration behind it is refused at the door. Together: **sid registration is
  necessary, sn is irrelevant**, and the check runs at insertion, not retention — yesterday's 25
  TIDAL queue entries sat in Kitchen's queue after their account died and had to be cleared by
  hand, while today the same tracks cannot get back in.
- **The failure mode "The stored serial goes stale" called untested is now tested**, in a stronger
  form than that section imagined: not a dead serial but a dead *registration*, and the stored
  serial turns out to have nothing to do with the failure — the enqueue path sends none
  (`main.rs:865` passes `None`). What a bookmark actually depends on is the household still holding
  *any* account for the service, and nothing in the bookmark can say whether it does.
- **800 is not specifically "no account".** Test E yesterday drew the same 800 for a foreign id
  under a *live* service (a YouTube Music id under Mixcloud's sid). So 800 at `AddURIToQueue` reads
  as "the player refuses to resolve this URI", covering at least an unregistered service and a
  foreign id — the CLI's "no reason given" is accurate and cannot be improved from this side.
- Serial bookkeeping: `sn_16` is confirmed as YouTube Music's last registration — named in the
  history entry's art URL. Yesterday it showed in the harvest on a queue item; today that queue
  entry is gone and the harvest names only `sn_2`, the pre-switch fossil in `FV:2`. So within two
  days the harvest went from naming the wrong serial *and* the right one to naming only the wrong
  one — the registration's death is invisible to it, and what it does show outlived what it
  missed.
- A third data point for the account-selection question, free with the control: the player filled
  **`sn_15`** for iHeartRadio (read live from `getMetadataStatus` while L-O-V-E played) — the
  higher and newer of the two serials again. Consistent with both prior observations, and still
  confounded the same way.

Kitchen was paused and its queue cleared afterwards; the household is as this found it, minus the
account its owner removed.

### Re-added the same day: `sn_20`, and the bookmark resurrected (2026-08-31)

The owner then re-added YouTube Music from the phone and played Coheed and Cambria into Living
Room. Two predictions were on the table, and both held:

- **The new registration is `sn_20`** — read live from `getMetadataStatus` on Living Room. Called
  in advance from the monotonic-counter model (`sn_19` was the high-water mark; `sn_18` stayed
  dead). *The prediction held and the model behind it did not: it was withdrawn 2026-09-01 when a
  new account came back `sn_5`. Two right calls from a rule the next observation broke - see
  "Household registration is per account".* That is the **second serial predicted before it existed**, after `sn_18`, and this one on
  a service that has now held `sn_2`/`sn_3`, `sn_16` and `sn_20` on one household. Per the owner it
  is the **same YouTube Premium subscription** each time — so this run is cleaner than the
  `sn_3`→`sn_16` plan switch: an identical service-side account, removed and re-added, minted a new
  serial. A serial belongs to a registration *event*, not to the service and not even to the
  account behind it.
- **The dead bookmark came back to life.** The same `x2rock bookmark Espresso` that answered 800
  yesterday enqueued and **played** in Kitchen — resolved to **`sn_20`**, a registration that did
  not exist when the item was recorded (its art URL still says `sn=16`). The enqueue-time check is
  live in both directions: the same id flipped from playing to refused to playing again within a
  day, tracking nothing but whether the household held *an* account for sid 284.

**This closes the design question "should a bookmark store the serial at all".** It cannot matter.
The player ignores whatever serial the enqueue path sends (`sn=9`, never registered, played) and
resolves the household's *current* registration for the sid (`sn_16` at record time, `sn_20` at
replay). A stored serial can neither pin a bookmark to the account it was kept from nor break the
replay when that account dies — the player never consults it. It is provenance at most, and the
resilience the serial-free path was credited with under "The stored serial goes stale" is now
demonstrated end to end rather than argued.

Kitchen paused and cleared again afterwards. Living Room was read, never touched.

## Plex: the first app-link service to fall (2026-08-31)

Asked for by name — "Señorita" was playing from Plex in the Dining Room, and search for Plex was
the request. Delivered the same day, and the route was nothing this document predicted.

Plex in the catalogue: `sid 212`, endpoint `https://sonos.plex.tv/v2.2/soap`, `Auth="AppLink"`,
`service_type 54279`. Its presentation map declares real search categories — `artists`, `albums`,
`tracks`, `playlists`, `podcasts` (plus a `CustomCategory` for episodes, which `categories()`
ignores for having no `id`) — and its manifest has no custom endpoints, so search is the classic
SOAP `search` this project already speaks. Only the credential was missing.

### Plex's SMAPI link half is dead — both flavours, whatever is sent

`x2rock link` grew the generic app-link attempt first: `getAppLink`, whose reply nests the same
`regUrl`/`linkCode` pair a device link uses (plus a `linkDeviceId`, now parsed and echoed into
`getDeviceAuthToken`). Worth having — it is how any of the 62 will be reached if it answers — but
Plex does not answer it:

| call | answer | varied |
|---|---|---|
| `getAppLink` | `Server.ServiceUnknownError`, always | household alone; + WSDL's `hardware`/`osVersion`/`sonosAppName`/`callbackPath`; + empty `loginToken`; + `deviceId` |
| `getDeviceLinkCode` | `Client.AuthTokenExpired`, always | bare; + empty `loginToken`; + `deviceId` = the player's `R_TrialZPSerial` (`34-7E-5C-31-AF-74:7`, read over UPnP `SystemProperties GetString`) |

The two *different* errors say `getAppLink` is unrouted on Plex's server while `getDeviceLinkCode`
reaches a handler that wants a token — for a call whose whole point is that there is no token yet.
Dead end, not a parameter puzzle.

### The actual door: Plex's SMAPI honours a plain Plex account token

The unlock came from prior art (python-plexapi's `sonos` module talks to `sonos.plex.tv` with the
user's own `X-Plex-Token`) plus a leak this household was already broadcasting: the player's art
URLs carry the Sonos integration's Plex token in the clear —
`getMetadataStatus.container.imageUrl` ends in `...&X-Plex-Token=<token>`. That token, stored as a
normal `credentials.json` entry for service 212 with an **empty key**, made everything downstream
work unmodified:

- `search -s plex -c tracks senorita` returned the very track the room was playing, id
  `c69ee188…::68163:track` — byte-identical to what `getMetadataStatus` reports.
- `browse -s plex` walks the household's own server (`Prime / Music`): playlists, artists, albums,
  hubs, `Other Sources`.
- `search … --play 1 -r "Dining Room"` played it, through the existing enqueue path
  (`SA_RINCON54279…` cdudn, household resolves its own `sn_13`). **The whole chain verified.**

So `loginToken/token` = any valid Plex account token, and the key is unused. Which means the link
flow does not need Sonos at all — and Plex publishes exactly the flow needed.

### `x2rock link plex`: the PIN flow, the first service-specific auth path

`src/sonos/plex.rs`. `POST plex.tv/api/v2/pins?strong=true` mints a pin; the person is sent to
`app.plex.tv/auth#?clientID=…&code=…` — code in the URL, so logging in is the whole interaction —
and the pin is polled until it carries `authToken`. From `run_link`'s point of view it is the same
shape as every other link (open a page, poll, store), dispatched by `chosen.id == "212"`, stored
through the same `from_device_auth` with no key and no hash. The `match` step is skipped silently
rather than with the no-hash warning: the household's own registration is what playback rides on,
and it already exists.

Decisions worth keeping:

- **The client identifier is stable**: `x2rock-<hostname>`. Plex files the token under it on the
  account's device list, so re-linking replaces a device instead of growing one per attempt, and
  revocation has a name to find.
- **plex.tv is spoken in its XML default**, one `<pin>` element with everything in attributes —
  `roxmltree` was already here and JSON would have bought nothing. An empty `authToken` attribute
  is *pending*, exactly parallel to `NOT_LINKED_RETRY`.
- **It earns the service-specific exception by being Plex's own published flow** — the one every
  third-party Plex client uses — not a scraped Sonos key. The YouTube Music `apiKey` question
  ("Open questions" below) is unchanged by this: presenting Sonos's key is still a different act.

### Two tokens, one asymmetry: root browse (settled 2026-09-01)

The first completed `x2rock link plex` run answered the caveat this section carried overnight. A
PIN-minted token **is** honoured by `sonos.plex.tv` — `search` works and browsing any *concrete*
container works — with exactly one exception: **`getMetadata root` answers
`Server.ServiceUnknownError`** (after one 6s timeout on first touch), on a request byte-identical
to one that succeeds under the household integration's own token. Omitting the `householdId` from
the loginToken changes nothing, so the difference is the token itself: the integration token
carries a server association Plex's bridge made when the owner linked Plex to Sonos, and root — the
"which server, which library" enumeration — is the one call that needs it. Relevant context for
this household: **the Plex server has Remote Access off**, so a fresh client's server discovery has
nothing advertised to find; whether root works for a PIN token on a published server is untested
here.

So the two tokens split like this, verified side by side on 2026-09-01:

| | PIN token (`x2rock link plex`) | integration token (`--from-player`) |
|---|---|---|
| search | works | works |
| browse a container by id | works | works |
| browse `root` (CLI default, widget's Browse row) | **refused** | works |
| play a hit | works — the enqueue path never needed a token | works |
| provenance | x2rock's own; revocable at plex.tv as `x2rock-<hostname>` | the household's Plex↔Sonos link; dies on relink |

**`x2rock link plex --from-player`, built for the second column.** The same move as `keep`: read
what the player itself built. Every Plex art URL a player hands out carries the integration's token
(`…%26X-Plex-Token%3D<token>&width=300` in `getMetadataStatus` image URLs), readable by any LAN
controller unauthenticated, so storing it deliberately — with the trade written down in `--help` —
beats the hand-edit it replaces. It needs Plex on-screen in some room, and it taught one bug worth
keeping: the first version asked `metadata()` for every group over the session's one socket and got
nothing, because **group-scoped commands answer only on the group's coordinator** — the same rule
`raw --scope group` learned, resurfacing in a new caller. Each group is now asked through
`session::coordinator`.

On a household like this one (Remote Access off), `--from-player` is the token that keeps the
widget's Browse row working; the PIN token is the durable, self-owned one and covers search and
play in full. Whichever ran last is what is stored.

### Plex wraps playable tracks in `mediaCollection`, and the container rule bent

A *tracks* search comes back as `mediaCollection` elements with `itemType: track` — and the id
inside is playable (it is what the household's own playback reports). The rule "the element
decides" — learned from Mixcloud, where `canPlay` lies — would have offered every Plex track as a
place to open. Amended: **a declared leaf type outranks the wrapping**, for `track` and `stream`
only. `canPlay` stays untrusted. Pinned by a test with the verbatim Plex shape; an `album` in the
same reply stays a container.

### The default category, and the widget's new `searchCategory`

Plex has no `all`, so `search -s plex <term>` defaults to its first category — `artists` — and a
song title searched there finds nothing. Two mitigations, deliberately short of changing the
default rule (which would silently move iHeartRadio from stations to tracks):

- An empty result now names the shelf: `Nothing on Plex for "senorita" in artists. Also
  searchable: albums, tracks, playlists, podcasts.`
- The widget grew `searchCategory` in `shell.json`, passed to the CLI as `-c`. A picker pointed at
  Plex wants `"tracks"`; empty keeps the CLI default and every existing config keeps its behaviour.

## The YouTube Music `apiKey` is sealed, and that closes the ~~question~~ *key* (2026-09-01)

> **Header amended 2026-09-01.** This section closed the *sealed-key* question — that path is a
> dead end and stays one. It did **not** close the *YouTube Music* question, because it assumed the
> key was the only way past the 403. It is not: the endpoint accepts OAuth. The still-open task is
> "TASK: the OAuth identity probe", near the end of this section.


Open question 1 carried "decide about the API key" from the first session onward, on a premise
recorded in "But it is per-service, and YouTube Music is not one of them": the manifest carries an
`apiKey`, so the 403 is *probably answerable*, and the only thing in the way is a judgement call
about presenting a key Sonos distributes for its own clients.

**The premise was wrong.** The manifest does not carry a key. It carries a key sealed in an
encrypted envelope, and the envelope is opened by something compiled into the Sonos controller app
and the player firmware. There is no key to decide about presenting. This section records the
bytes, because the decision only holds as long as the evidence for it is checkable.

### Provenance, and how to re-fetch

The descriptor entry, from the cached `ListAvailableServices` in `services.json`:

```json
{"id": "284", "name": "YouTube Music",
 "uri": "https://music.googleapis.com/v1:sendRequest",
 "auth": "AppLink",
 "manifest_uri": "https://cf.ws.sonos.com/p/m/a3fd2ecc-6039-47fe-8ced-1939e070c432",
 "service_type": 72711}
```

Both documents are anonymous GETs on Sonos's CDN — no household, no token, no player involved.
This is the same read `smapi.rs:246` already does for every service:

```sh
curl -s https://cf.ws.sonos.com/p/m/a3fd2ecc-6039-47fe-8ced-1939e070c432   # manifest
curl -s https://cf.ws.sonos.com/p/p/a3fd2ecc-6039-47fe-8ced-1939e070c432   # presentation map
```

The manifest is small. Whole thing, minus the two base64 blobs:

```json
{"schemaVersion": "1.0",
 "presentationMap": {"uri": ".../p/p/a3fd2ecc-...", "version": 278},
 "strings":         {"uri": ".../p/s/a3fd2ecc-...", "version": 278},
 "apiKey": {"cr": "iGSZygAAALoAAQAAAAAAAAEAAAAUA444pCPrP+ks…",
            "zp": "iGSZygAAALoAAQAAAAAAAAEAAAAUA3ngEn8egjxX…"},
 "search-catalogs": [],
 "endpoints": [{"type": "reporting",
                "uri": "https://music.googleapis.com/v1/v2.3/report/", "version": "1"}]}
```

### The envelope, byte by byte

Each value is 356 base64 characters decoding to **266 bytes**. The first 21 bytes are
byte-for-byte identical between `cr` and `zp` (`iGSZygAAALoAAQAAAAAAAAEAAAAU` in base64, a clean
28-character boundary because 21 divides by 3). Every declared length matches the bytes that
follow it, and the fields account for all 266 bytes with nothing left over:

| Offset | Len | Content | `cr` | `zp` |
|---|---|---|---|---|
| `[0:4]`     | 4   | magic                       | `886499ca` | `886499ca` |
| `[4:8]`     | 4   | u32 = 186                   | — | — |
| `[8:13]`    | 5   | header `0001000000`         | — | — |
| `[13:17]`   | 4   | count = 1                   | — | — |
| `[17:21]`   | 4   | **len = 20**                | — | — |
| `[21:41]`   | 20  | **key identifier**          | `038e38a423eb3fe92ccd9361d5350f75a51aa427` | `0379e0127f1e823c579ff956ee600c9aa3c442c8` |
| `[41]`      | 1   | tag                         | `0xc1` | `0xb8` |
| `[42:46]`   | 4   | count = 1                   | — | — |
| `[46:50]`   | 4   | **len = 128**               | — | — |
| `[50:178]`  | 128 | **RSA-1024 wrapped key**    | `64fa8be554a30ac260f58b1b…` | `8d7326356bdac23a8c4f244b…` |
| `[178:182]` | 4   | count = 1                   | — | — |
| `[182:186]` | 4   | **len = 80**                | — | — |
| `[186:266]` | 80  | **AES payload**, 5 × 16B    | ciphertext | ciphertext |

A hybrid crypto envelope in the textbook shape: a symmetric key wrapped under RSA, a payload under
that symmetric key, tagged with a 20-byte (SHA-1-sized) identifier saying *which private key opens
it*. 128 bytes is exactly RSA-1024. 80 bytes is exactly five AES blocks. Neither number is a
coincidence and neither is a length that plaintext would land on.

### Why "it might just be obfuscation" does not survive

Shannon entropy of the two ciphertext regions, against controls of true random bytes at the same
lengths (200 samples each, because entropy at short lengths is well below 8.0 by chance and a naive
"6.9 is not 8.0, so it is not random" reading gets this backwards):

| Region | Length | `cr` | `zp` | random control |
|---|---|---|---|---|
| RSA block | 128B | 6.50 | 6.65 | **~6.55** |
| AES payload | 80B | 6.06 | 6.04 | **~6.03** |

The control column is a sampled mean over 200 draws, so it wobbles by a few hundredths between
runs (6.54 and 6.55 are the same number here); the `cr`/`zp` figures are exact and deterministic —
6.4979, 6.6542, 6.0625, 6.0375. Both regions sit inside the control range, and there is no
structure to find: no ASCII runs beyond incidental 4-character noise, no repeated blocks, no
padding pattern. Whole-buffer entropy is
*lower* (6.90 / 6.93 against a 266-byte control of 7.21) precisely because the header and length
fields are full of zero bytes — which is itself confirmation that the parse above is separating
structure from ciphertext correctly.

Reproduce:

```python
import json, base64, struct, os, math, collections, urllib.request
m = json.load(urllib.request.urlopen("https://cf.ws.sonos.com/p/m/a3fd2ecc-6039-47fe-8ced-1939e070c432"))
ent = lambda b: -sum(n/len(b)*math.log2(n/len(b)) for n in collections.Counter(b).values())
for k, v in m["apiKey"].items():
    b = base64.b64decode(v)
    assert struct.unpack(">I", b[17:21])[0] == 20 and struct.unpack(">I", b[46:50])[0] == 128 \
       and struct.unpack(">I", b[182:186])[0] == 80 and len(b) == 266
    print(k, "keyid", b[21:41].hex(), "rsa-ent %.2f" % ent(b[50:178]), "aes-ent %.2f" % ent(b[186:]))
print("control 128B %.2f" % (sum(ent(os.urandom(128)) for _ in range(200))/200))
```

### `cr` and `zp` — inference, not proof

The two key identifiers differ, so the two envelopes are sealed to two different keys. The names
are almost certainly Sonos's own vocabulary: **`cr` = Controller** (the CR100 and CR200 were
Sonos's handheld controllers) and **`zp` = ZonePlayer** (every player model is ZP80/ZP90/ZP100/
ZP120, and the UPnP device type is `ZonePlayer`). That reading fits the structure exactly — one
envelope the controller app can open, one the speaker firmware can open, same plaintext key inside.

Flagged as inference because nothing was decrypted to confirm it. What *is* proven is the part that
matters: **two distinct private keys exist, and neither is in the manifest.**

### What this makes the act

Before this section, using the key looked like presenting a string anyone can fetch — bad manners
toward Sonos at worst, and arguably not even that, since a Google API key is a project identifier
for quota and billing rather than an authentication credential (Google's own documentation says so,
and API keys ship in client bundles routinely). That framing was fair and it is now beside the
point, because **we do not have the key.** Obtaining it means extracting an RSA private key from
Sonos player firmware or the official controller binary.

That is defeating a technological protection measure, not reading a public document, and it is a
different category of act from everything else in this project. **Decided: no.** Not on
etiquette, and not as a close call.

The distinction that governs the rest of the project is unchanged and worth restating, because it
is the line every future service decision should be tested against:

- **Anonymous services** — the endpoint answers whoever asks.
- **Device link** (Bandcamp, iHeartRadio, Mixcloud) — the service publishes a flow *for third-party
  clients* and mints a token for you. You are yourself.
- **Plex** — the same, via Plex's own PIN flow, and the token lands on your device list as
  `x2rock-<hostname>` where you can revoke it. You are yourself, and you are visible.
- **The sealed key** — you are not yourself, nothing was minted for you, nothing can be revoked
  without breaking every Sonos player on earth, and getting it requires prying a private key out of
  a binary.

Every credential x2rock holds today was issued to x2rock or to its user. This one would be the
first that was not, and it is also the first that could not be obtained without circumvention.

### The registered-key proxy: right pattern, wrong door

Proposed 2026-09-01 and worth recording, because it answers the objection that the sealed key
cannot: run a small service holding a **registered** Google key, so traffic reads as *this project*
rather than as Sonos. That fixes attribution completely — Google gets a named party to rate-limit,
quota or revoke, and Sonos is out of the blast radius. If the barrier were only "whose quota is
this spending", this would be the answer.

It does not open this door, for two independent reasons:

1. **There is nothing to register for.** `music.googleapis.com/v1:sendRequest` is a partner
   endpoint provisioned to Sonos. There is no product to enable in Google Cloud Console and no key
   obtainable through self-service that would be authorised on it. A registered key is valid for
   APIs that are not this one.
2. **The public API returns the wrong identifiers.** YouTube Data API v3 is registerable, quota'd
   and entirely above board — and it answers with YouTube video ids. The Sonos object id is 48
   opaque characters (`ALkSOiGTPQu20Hqb6iEmeMhGFI_jhhXgHyx7WTjmO6bs1i3H`), not a video id and not
   derivable from one. Search results nothing can play are not search.

Keep the pattern. It is the correct architecture for any future service where a registerable key
exists, and it is a better answer than "decide carefully" for the class of problem it fits.

### The wall really is only the key

Worth knowing, so nobody re-opens this hoping the SMAPI side is also missing: the presentation map
specifies YouTube Music search **fully**, in nine categories across two groups.

```xml
<PresentationMap type="Search">
  <SearchCategories stringId="YouTube Music">
    <Category id="artists" mappedId="ARTISTS"/>     <Category id="playlists" mappedId="PLAYLISTS"/>
    <Category id="tracks"  mappedId="SONGS"/>       <Category id="albums"    mappedId="ALBUMS"/>
    <Category id="all"     mappedId="ALL"/>
  </SearchCategories>
  <SearchCategories stringId="Library">
    <Category id="playlists" mappedId="LIBRARY_PLAYLISTS"/>  <Category id="tracks" mappedId="UPLOADED_SONGS"/>
    <Category id="albums"    mappedId="UPLOADED_ALBUMS"/>    <Category id="artists" mappedId="UPLOADED_ARTISTS"/>
  </SearchCategories>
</PresentationMap>
```

So the search interface is defined and waiting; the 403 is the whole of what stands in front of it.
The manifest's `search-catalogs` is `[]` and its only endpoint is `reporting`, which is consistent
with search living on the service `uri` rather than a custom endpoint — the classic shape.

> **Superseded 2026-09-01 — the wall is not the key.** "The 403 is the whole of what stands in
> front of it" was right; "and the 403 needs the sealed key" was the unstated and wrong half. The
> endpoint accepts OAuth as an alternative identity, so the sealed key is one door, not the door.
> See "TASK: the OAuth identity probe" immediately below. The section above stands as the analysis
> of the *key*; it no longer stands as the analysis of the *wall*.

### TASK: the OAuth identity probe (open, needs a Google Cloud OAuth client — 2026-09-01)

Prompted by the observation that a Sonos app linking YouTube Music shows a ~9-character code and
hands it to **Google**, which authenticates the account and approves the connection. That is the
**OAuth 2.0 Device Authorization Grant** (RFC 8628), and it is Google's flow, not Sonos's — Sonos
is a registered partner operating inside Google's terms. Which reframes both the 403 and the sealed
key: the key is Google's *project*-identity requirement (a partner key Google requires be
protected, hence sealed in firmware), and the ~9-char code is Google's *user*-identity flow.

**Probed 2026-09-01, and it overturns "the wall really is only the key".** Three requests to
`https://music.googleapis.com/v1:sendRequest`:

| sent | answer |
|---|---|
| no identity | **403** `PERMISSION_DENIED` — "unregistered callers … use API Key **or other form of API consumer identity**" |
| `Authorization: Bearer <malformed>` | **401** `UNAUTHENTICATED` — "Expected **OAuth 2 access token, login cookie** or other valid authentication credential" |
| Bearer + dummy `?key=` | **401**, as above — the Bearer takes precedence |

The moment a Bearer is present the complaint changes from *no identity* to *this OAuth token is
invalid*. **So the endpoint evaluates OAuth as a first-class identity; the sealed API key is not
mandatory.** It also names "login cookie" — the SAPISID cookie `music.youtube.com`'s own web client
uses — confirming this is the YTM backend accepting the same auth its own clients do.

**What that leaves as the real, single unknown:** whether a token minted by a *self-service* Cloud
OAuth client is accepted, or whether the endpoint is pinned to Sonos's registered `client_id`
(and/or requires the calling project to have this partner API enabled, which self-service cannot
do — it is absent from Google's 528-entry public API discovery directory). Google can allowlist an
API to specific OAuth clients, and a partner endpoint very likely is.

**The experiment, one request decides it.** Clean — the user's own account, Google's own flow, no
circumvention, nothing built into the repo:

1. *(User action, cannot be automated.)* Create a Google Cloud project and an OAuth client of type
   "TVs and Limited Input Devices", with a YouTube scope (`…/auth/youtube` or `youtube.readonly`).
2. Device-grant harness (to build, kept **out of the repo** as a scratch script): request a
   `device_code`/`user_code` at `https://oauth2.googleapis.com/device/code`, print the ~9-char
   code and `verification_url`, poll `https://oauth2.googleapis.com/token` until the person
   finishes — the same shape as `smapi::device_auth_token`'s poll loop.
3. Fire one `getMetadata`/`getAppLink` SOAP body at the endpoint with `Authorization: Bearer
   <token>`.
4. Read the status:
   - **200** → the wall was only the client allowlist and a self-owned account clears it. YTM
     search becomes a normal SMAPI feature, no sealed key ever touched. Then, and only then, the
     *second* wall matters (below).
   - **403 `PERMISSION_DENIED`** → pinned to Sonos's `client_id`; closed, but for a nameable reason
     rather than the sealed key, and revisitable only if Google ever opens the client set.
   - **401 / scope error** → wrong scope; retry with another before concluding anything.

**The second wall, only reached if the first opens: the id namespace.** Even a 200 gives results in
whatever id space `sendRequest` returns; the Sonos player enqueues only its **36-byte opaque
object id** (`x2rock keep` harvests one: `00b9123a…`, which is not a videoId, not an `MPRE…` browse
id, and not a protobuf — `0x00` is not a legal protobuf field tag, so it is a wrapped/opaque Sonos
handle). If `sendRequest`'s own search returns those object ids directly, discovery feeds the
existing enqueue path and the feature is done. If it returns videoIds, the `videoId → objectId`
mapping is the next question, and the bytes look encrypted rather than encoded. **Do not spend the
id question until step 4 returns 200; a 403 makes it moot.**

Related, non-Cloud discovery path worth noting so it is not confused with this one: `ytmusicapi`'s
OAuth mode is itself a Cloud "Limited Input Device" client against InnerTube
(`music.youtube.com/youtubei`). It reaches the real YTM catalogue as the user, but returns videoIds
— so it runs into the same second wall, and it does not test the *Sonos* endpoint at all. The probe
above is the one that answers "does a Google Cloud account open YTM **on Sonos**".

### Objections this section already answers

Collected so they do not each cost a session:

- *"The key is public, so nothing is being taken."* The **container** is public. The plaintext is
  not, and the private key that reaches it is not.
- *"Maybe it is obfuscation, not encryption."* See the entropy table and the exact RSA-1024 and
  5-AES-block lengths. Obfuscation does not land on those numbers or those entropy values.
- *"Google API keys are not credentials anyway."* Correct, and no longer relevant. The barrier
  stopped being *what the key is* the moment it turned out we do not have it.
- *"RSA-1024 is weak."* Not weak enough to factor here, and succeeding would be the same act by a
  longer route.
- *"The Sonos PC app does app-link too, so a desktop path exists."* True — and it was a genuine
  correction to the older claim that app-link means handing off to the *service's* mobile app (see
  the amended paragraph in "But it is per-service"). It does not help: the desktop controller opens
  the same envelope with the same embedded key.
- *"Check another service's manifest — maybe one ships a plain key."* Possibly, and it would say
  something interesting about how uniformly Sonos seals these. It would not produce YouTube Music's.
- *"What would re-open this?"* Sonos publishing an API for third-party controllers, Google exposing
  a registerable endpoint that accepts Sonos object ids, or YouTube Music appearing among the
  device-link services. None are things to wait for. **But one thing did re-open it the same day**,
  and from inside rather than from Google or Sonos changing anything: the endpoint accepts OAuth,
  so the sealed key is not the only identity. See "TASK: the OAuth identity probe" above — the
  question is no longer *the key* but *the client allowlist*, and that is testable with a
  self-service account.

### What still works, and is enough

Playback was never blocked by any of this. `x2rock keep` stores the object id, the player resolves
the household's own credential at enqueue, and the track plays — demonstrated in both directions on
2026-08-31 when the household's account was disconnected (UPnP 800 at the door) and re-added
(`sn_20`, same id, playing again). Discovery stays in the Sonos app; repetition lives on the bar.
That is the honest shape of this feature and it is not a consolation prize.

## Telling a live stream from an item, and the mark it earns (2026-09-01)

The widget showed a station exactly as it showed a track: a name, and nothing to say whether it
ends. That difference decides whether seeking, a duration or a queue position mean anything, so it
is worth a glyph.

**What says so is `container.type`, and nothing else does.** Three captures off the Media Room the
same day, the third of which is the control:

| | YouTube Music track | TuneIn "Jazz Club" | Sonos Radio "Sound System" |
|---|---|---|---|
| how it started | queue | x2rock `loadStreamUrl` | **the Sonos app** |
| `container.type` | `track` | **`station`** | **`station`** |
| `container.id.objectId` | real | `-1` | **`97034`** (real, `sn_1`) |
| `currentItem` | present, with `durationMillis` | absent | **present, name + artist** |
| `durationMillis` | 179000 | — | **absent** |
| `playbackSession` | — | x2rock's `directControl` | **none** |

**The control settled it.** An earlier version of this section flagged that TuneIn station as
started *by x2rock*, which sends `"type": "station"` in its own `stationMetadata` - so the capture
could not rule out the player echoing back what it was handed. Sonos Radio's "Sound System",
started from the Sonos app, carries no `playbackSession` and a real object id: nothing x2rock sent
was in the loop, and the container type still reads `station`. **It is the player's own vocabulary.**

**It also killed two signals this section used to lean on.** A missing `currentItem` and
`objectId "-1"` were offered here as corroboration - the player's own invention rather than an
echo. They are **TuneIn's shape, not a live stream's**: Sonos Radio streams a named track by a
named artist, with a real id, and still has no duration and no end. Generalising from one service
was the mistake, and one capture from a second service was enough to catch it. Only
`container.type` and the absent duration survive all three, and the duration was never the question.

**What was deliberately not used: the absent duration.** `mpris:length` missing is the closest
thing MPRIS has, and it answers a different question. A track whose service simply did not send a
duration looks identical, and a client marking that one a station would be wrong about the source
rather than about the metadata. The flag says what x2rock knows, and the widget reads it strictly
(`=== true`), so an older daemon that sends no such key leaves every room unmarked rather than
marking them all.

It goes out as `x2rock:isLiveStream`, the same namespaced-metadata route as `x2rock:members` and
`x2rock:onTvInput` - MPRIS has no field for it and no way to add one except this. Verified end to
end on D-Bus: `b false` on a YouTube Music track, `b true` with `xesam:title "Jazz Club"` while
TuneIn played, and `false` again once the room was back on its queue.

### A live stream can have changing artwork, and Sonos Radio does

Noticed while the control was still playing, and worth writing down because the
naive expectation is the opposite - a stream has one logo and keeps it. Sonos Radio sends art per
*track*, and `to_metadata` already prefers `track.image_url` over the container's, so it updates on
its own as the stream advances.

How, from two captures on the same station:

```
…/fe9cd0a0bbfca70dd863c43e88e65ffa_bg_09.png ?…&mark=…dzcdn.net/…/a1c3d91c…/1000x1000….jpg
…/fe9cd0a0bbfca70dd863c43e88e65ffa_bg_09.png ?…&mark=…dzcdn.net/…/87d89262…/1000x1000….jpg
```

Same base image, different `mark`. The station supplies one constant background and `imgix`
composites the track's own cover onto it server-side, the cover itself coming from Deezer's CDN. So
the URL changes on every track and the widget refetches without being told to.

TuneIn, again the opposite shape: `art_url` is `null` outright. Three per-track fields separate the
two services - name, artist and art - and none of them says anything about whether the source is
live. Only `container.type` does.

### `canPause` is not about the command, and Sonos Radio half-honours it

The two live streams disagree about pausing, which is why the transport control asks the *player*
rather than asking whether the source is a stream:

| | `canPause` | `pause` gives | on resume |
|---|---|---|---|
| TuneIn "Jazz Club" | `false` | `IDLE` | starts again at the live edge |
| Sonos Radio "Sound System" | `true` | `PAUSED`, position frozen | **the same track, from the top** |

Measured 2026-09-01: paused at 90553 ms, held at 90813 ms across three seconds - a real hold, not a
drifting counter - and came back at **5250 ms of the same track**. So the state is honest and the
resumption is not. `canPause` reads as "can this be resumed where it left off", which is the
narrower question its name does not ask; Sonos Radio answers yes and then delivers only the first
half.

**Not worth correcting in the widget.** The transition is real, the row correctly shows paused, and
losing ninety seconds of a track that cannot be seeked anyway is a small cost. Making the button
read `stop` here would mean hard-coding "Sonos Radio behaves oddly" - service-specific guessing of
exactly the kind `stopRather` exists to avoid - and it would be wrong the day Sonos fixes it.

The load-bearing point for anyone tempted to simplify: **being live and being pausable are
different properties.** `stopRather` gates on `canPause` and must keep doing so. Folding it into an
`isLiveStream` check would put a stop button on this station, which pauses perfectly well.

### Sonos Radio names the track, so the station needs saying

`to_metadata` prefers `track.name` over `container.name`, which is right - the track is the more
interesting fact - but on Sonos Radio it means the row reads `Intervallo (from "Veruschka") (II) —
Ennio Morricone` and never says what it is playing *on*. TuneIn has the opposite shape: no track at
all, so the title already **is** the station and saying it twice is noise.

So the daemon decides rather than the widget: `x2rock:stationName` carries the container name only
when the stream is live *and* the name is not already the title. Empty otherwise, and the row's
extra line appears only when there is something in it.

### The picker asks a different question, and one answer is unverified

A picker row is a thing that is *not* playing, so it cannot read the daemon's flag. `isStreamRow`
reads the row's own `type` instead, and the three sources that feed the picker share field names
without sharing vocabularies - each passes through whatever its origin called the item:

| Source | A station reads | Status |
|---|---|---|
| `search`, `browse` | `stream` (SMAPI's `itemType`) | **verified** - a TuneIn jazz search answers 19, and browsing into Trending answers 50, all `container: false` |
| `favorites` | an **upper-case enum**: `STREAM`, `AUDIOBOOK` | **verified 2026-09-01** - and the prediction was wrong, see below |

Three marks now come out of one `markFor`, all verified against live CLI output rather than reasoned:
`stream`/`STREAM` and the untested `audiobroadcast` take the radio antenna, `show` takes a
microphone, and anything containing `audiobook` takes an open book - a substring because a favorite
says `AUDIOBOOK` while a playing Audible track says `chapter.audiobook`. Everything else, `track`
and `container` included, takes nothing.
| `bookmarks` | `stream`, being what x2rock stored | covered by the same check |

Unknown reads as "no" - an unmarked station is a smaller wrong than a marked album, and an older
CLI sending no `type` must not mark the whole list.

### The favorites half, tested - and the prediction was wrong

Run 2026-09-01 with the first favorite this household has ever had: "Virgin Radio Chilled UK",
saved from the Sonos app. `favorites --json` reports **`"type": "STREAM"`**, in capitals - not the
DIDL-Lite `object.item.audioItem.audioBroadcast` this document expected, and not lowercase either.

The mark appears correctly regardless, because `isStreamRow` lowercases before comparing and
`STREAM` falls through to the same `=== "stream"` branch the search and browse rows use. So the
feature was right by construction rather than by the reasoning written down beside it - which is
worth saying plainly, because the reasoning is what a later reader would trust.

A second favorite the same day settles the shape: an Audible audiobook reports **`AUDIOBOOK`**. So
the field is an upper-case enum of its own - `STREAM`, `AUDIOBOOK` - and not a DIDL-Lite class at
all. The `audiobroadcast` clause is therefore **untested, not reasoned**. It is kept in case some
service answers that way, but nothing on this household has produced it and the vocabulary now
looks like the wrong family entirely.

**A third shape, and the detector holds.** Audible's *playback* metadata reports
`container.type: "audiobook"` with `track.type: "chapter.audiobook"` and a real `durationMillis`
(58000 on "Book One: Dune"). Not `station`, so `is_live_stream` correctly leaves it unmarked - an
on-demand item with a duration and an end, which is the thing the radio glyph exists to distinguish
from. Four services have now been read live: `track`, `station` (twice, two very different shapes),
and `audiobook`.

The older recipe follows, still valid for re-running the check on another service.

### How to test the favorites half

Not done, because this household has **no favorites at all** (`x2rock favorites` says so) and
`audioBroadcast` appears nowhere in anything captured so far. The class is the DIDL-Lite standard
for a broadcast, which is a good reason to expect it and not the same as having seen it.

1. In the Sonos app, save a **radio station** as a Sonos favorite - a TuneIn one is enough, and
   "Jazz Club" is the station every other capture here used.
2. `x2rock favorites --json` and read the `type` field on that row. It comes from
   `BrowseItem::kind()`, which returns the `<upnp:class>` out of the favorite's own metadata.
3. If it contains `audioBroadcast` in any casing, the predicate is right and this section becomes
   verified. If it says something else, `isStreamRow` in `BarWidget.qml` is the one line to widen -
   the comment above it names this as the branch that would be wrong.
4. Either way the mark should appear beside that favorite in the picker. That is the actual
   check; step 3 only explains a failure.

Worth doing while a favorite exists anyway, since the household has none and several other
questions here are blocked on the same absence.

## `queueVersion` does not exist, and the queue view was stale because of it (2026-09-01)

Reported as "it got weird when I added a favorite twice then tried to remove it from a queue", and
the report was exactly right.

**The field the daemon reads is not sent.** `RoomState::queue_version` comes from
`playbackStatus.queueVersion`, and firmware 95.0-77060 does not send it. `getPlaybackStatus`
answers with `playbackState`, `positionMillis`, `itemId`, `playModes`, `availablePlaybackActions`,
`isDucking`, `previousItemId`, `previousPositionMillis` - and nothing else. Nor does it arrive in an
event: forcing a real pause and a real play left `x2rock:queueVersion` an empty string through both,
so it is absent from the event body too rather than merely from the polled response.

**So the widget's refresh never fired.** `onQueueVersionChanged: if (queueFor !== "") loadQueue()`
has never run. Queue edits were fire-and-forget on the strength of it, which left the panel showing
the list from before the edit.

**That is a correctness bug, not a cosmetic one.** Queue rows carry *positions*. Acting on a stale
list removes or moves a different track than the one on screen - which is what happened: two adds
while the panel was open, then a remove that took the wrong entry.

**Fixed the proportionate way.** Every edit the widget makes now re-reads the list when the process
exits - `queueEditProc` for remove/move, `queueItemProc` for the `+`. An edit made *elsewhere*
still goes unnoticed until the view is reopened, and the comment that promised otherwise ("including
from the Sonos app, which is what keeps this honest when someone else edits") is gone rather than
left to mislead.

**And then the real fix as well, at the owner's request.** The queue's true version *is* available -
the `UpdateID` on a `Q:0` browse, which `Upnp::update_id` already reads before every mutation. The
daemon now publishes it as `x2rock:queueVersion`, so the mechanism the widget was written against
finally exists.

**Where the trigger came from.** The local API has no queue namespace to subscribe to: `queue:1` and
`playbackQueue:1` both answer `ERROR_UNSUPPORTED_NAMESPACE`, and `playbackSession:1` has no queue
command. So the read rides a playback event the daemon was already handling, which keeps the
no-polling promise honest - a room doing nothing costs nothing, and the price is one small SOAP
browse per state change or track boundary.

Verified: the key went from `""` to `"84"`, and to `"85"` when a track was added.

**What it catches, and what is still missed.** Anything that moves playback is seen - the Sonos
app's Play Now, a track advancing, a queue cleared under a playing room. A *silent* append to a room
that keeps playing what it was emits no event, so it waits for the next one. Closing that needs UPnP
GENA eventing, which needs the players to reach an HTTP callback on this machine, which Omarchy's
default-deny firewall does not allow - the same wall the cloud-queue note runs into.

### A queue row with no metadata, and whose fault it is

Seen while the above was being fixed: one queue row rendered blank while the rows either side of it,
from the same service and added minutes apart, carried title, artist, album and duration. `now`
agreed - `title`, `artist` and `service` all `null` - so nothing was being dropped on the way
through; the player genuinely held no metadata for it.

**It is the Sonos app that makes these.** Per the owner, its `...` menu's **Play Now** adds an entry
carrying no metadata, while tapping the track on the now-playing view adds a normal one. Two paths
in first-party software, one of which produces a row nothing can describe. Worth writing down
because the obvious suspect was x2rock's own synthesized DIDL in `bookmarks::service_didl`, and that
is not it - items queued through `queue-item` come back with their titles intact.

Nothing here can repair such a row: there is no title to fetch. So both surfaces name it instead of
leaving a gap - `(no title from the player)` in `x2rock queue` and in the widget's queue panel,
where a blank line reads as corruption rather than as an answer. The wording says *whose* silence it
is, which is the useful part.

### What first-party Sonos does, for whenever this is picked up

Per the owner, and matching the split x2rock already implements: **playing to a device does not
change the queue** - a station or a podcast episode runs alongside it - **but playing an album adds
all of its tracks to the queue and plays them.** x2rock has the first half and not the second: a
container is somewhere to browse into, and there is no "play this album" that fills the queue with
its contents. `queue-item` adds one row at a time. That is the gap to close if queue work resumes.

## x2rock as an agent substrate (2026-09-02)

A distinct arc, and worth its own section because it changed what the CLI is *for*. The starting
premise was that Omarchy is "AI first", so the person driving x2rock is as likely to be an agent as
a human at a prompt. That reframes the CLI's output and errors as an API, and most of the work below
is making that API honest. It was built, then **dogfooded by a second Claude session driving the
CLI as an agent** - which turned up roughly a dozen concrete gaps a human never would, because a
human reads prose and an agent parses fields. The dogfooding loop is the most useful process finding
here: an agent leaning on the contract finds exactly the places the contract lies.

### The one-call snapshot: `status --json`

An agent's first move is almost always "what is the whole household doing", and answering it with
`rooms` then `now` per room then `vol` per room is N+ round trips with no atomic view. `x2rock
status` returns every group in one call, each room modelled on `now --json` (so a consumer that
parses `now` needs no new shape) plus volume, grouping, coordinator and `has_tv`. Two decisions
carried:

- **Query each group through its own coordinator.** A group command answered on the wrong socket is
  the same failure the daemon hit early ("one connection per coordinator"); the snapshot resolves a
  connection per group, reusing the session's where it coincides.
- **One unreachable coordinator is that room's problem, not the snapshot's.** Each room's fetch is
  fallible on its own; a failure tags that room with an `error` field carrying its identity,
  grouping and TV, and the other rooms still report. Proven by unplugging a speaker mid-`status`:
  the room came back error-tagged after the 5s connect timeout, the rest answered, and the whole
  array still returned. The per-room build is a pure function (`room_value`) so the error branch is
  tested without a network.
- **Bare array by default, envelope on `--full`.** A bare array is nicer at a `jq` prompt and is
  what existing callers expect, so it stays the default; `--full` wraps it in `{household, network,
  total, reachable, warnings, rooms}` for an agent that wants to know *which* household and network
  it is on and whether every room answered. The household round trip and the fingerprint are
  gathered only when `--full` asks.

### Errors an agent can act on, not parse

The error prose was already written to name its own fix (the unregistered-network message says to
run `x2rock discover`). The agent turn was to make that a *field*: a `Hint` (src/hint.rs) is an
ordinary `std::error::Error` carrying a stable `code`, an optional runnable `fix`, and optional
structured `data`. It flows through `anyhow` like any error - its `Display` is just the message, so
the daemon and the plain CLI are unchanged - and at the top level `main` splits into `main`/`run`:
a `--json` invocation that fails prints `{error, code, fix, …data}` on stderr and exits non-zero,
reading the code and fix by downcasting the error chain. A plain error is `{"code":"unknown",
"fix":null}` - still structured.

Codes hinted so far: `unregistered_network`, `unknown_room`, `needs_link`, `no_player`,
`too_many_rooms`. Two lessons in the shape:

- **`no_player` inherits a sharper inner code.** Wrapping a connect failure as "no player to play it
  on" would bury an `unregistered_network` underneath; the wrapper keeps a *connection-layer* inner
  code when there is one (better diagnosis; at the time both carried the same `discover` fix) and defaults to `no_player`
  otherwise. Restricted to connection-layer codes after a review noted the general version could
  pair a mismatched fix with the wrapper's message.
- **The `data` payload lets an error hand back what the caller would re-fetch.** `unknown_room`
  fills it with `rooms` (the whole list) and `did_you_mean` (Levenshtein near-misses), so a mistyped
  `-r "bedoom"` comes back with `["Bedroom"]` in one call instead of a fail / `x2rock rooms` / retry
  round trip. The `data` object cannot shadow `error`/`code`/`fix`, pinned by a test that tries.
- **A `fix` must not invite a harmful auto-run - so `unregistered_network` has none.** It originally
  offered `x2rock discover`, but a testing agent pointed out that the "when `fix` is non-null, run it
  and retry" contract then means: a laptop on a café network is asked to pause, gets
  `unregistered_network`, and *scans the café's network* - the exact road-warrior behaviour the
  design avoids. The fix is now `null`; the message says discovery is deliberate and offered, not
  reflexive. The rule is general: a `fix` is a *safe, local* remedy, and a command that scans someone
  else's network is not one to hand an agent that will run it unprompted. A later review generalized
  it to `no_player`: `connect` has already rescanned a known network by the time it reports one, so
  the fix just repeated the scan that came back empty — and the wrapper's fallback arm was minting a
  scan for failures nothing had classified (a stale `--ip` on café wifi). `x2rock discover` now
  appears only in messages, as an offer, never in `fix`; the hint constructors live in `hint.rs` and
  a test pins that neither network error carries a runnable scan. The generic no-remedy code
  was also renamed `error` -> `unknown`, so `{"error": …, "code": "unknown"}` no longer reads a code
  named `error` inside a field named `error`.

### The daemon stops narrating an unchanged state

A road-warrior daemon on an unrecognised network logged the same "unregistered network" line every
60s - ~2880 identical lines a day, burying the events worth seeing. A `StatusLog` coalesces on a
key that carries the **network fingerprint**, so a network switch flushes at once (the move is
exactly what a reader wants), and re-logs a held state once an hour with the count it held,
journald-style. A successful connect resets the coalescing so a later failure logs fresh. The
decision core takes the clock as an argument, so the window and heartbeat are pinned by tests
without waiting an hour. `X2ROCK_LOG_VERBOSE` restores every line and the backoff ramp for
debugging the reconnect machinery, which is the finickiest part of the daemon. `X2ROCK_LOG_EVENTS`
is its sibling rather than part of it - raw event bodies, at a rate that would bury the ramp.

Why this is safe against the AI-first goal: the *pull* (`x2rock rooms`, the sharpened
`unregistered_network` message that names `x2rock discover`) is the agent's actionable surface, so
the *push* log can go quiet without hiding anything. The log is observability; the CLI is the API.

### Repeatable `-r`: multi-room in one invocation

Setting three rooms' volume was three cold process starts, each re-resolving topology. `-r` is now
repeatable for the per-room-state commands - `vol`, `repeat`, `shuffle`, and transport - so `-r
Kitchen -r Bedroom vol 10` connects once and fans out, one result line per room; `--all` fans the
same across every group (resolved by each group's coordinator name, since the composite group name
is not addressable). A third agent asking for "quieter everywhere" is what surfaced `--all` as a
real gap rather than a doc fix - N `-r` flags derived from a prior call is not a clean answer to a
whole-house request. Worth stating for whoever documents grouping: `-r <any member>` of a group
resolves to the *group* (transport and group volume), and `--player` is the only way to reach one
speaker inside it - verified, and the reason the skill now leads its grouping section with it. Deliberately
agentic and nowhere else: a human uses `party` or one `-r`; an agent orchestrating "set the
downstairs to 10" wants one call. Several `-r` on a command that does not fan out is a
`too_many_rooms` error, not a silent act on the first; a fan-out stops at the first room that fails
and names it. Mechanically, `--room` became a `Vec` (the single room bound from the field, disjoint
from the moved `command`), and the vol/repeat/shuffle handlers were extracted into `apply_*` shared
by the single arm and the fan-out - so the `--player` scoping, fixed-volume refusal and mute rules
live in `apply_vol` once.

### The skill ships in the binary

An agent benefits from a written contract, but a skill only helps where it is installed. So the
agent skill is embedded (`include_str!`), one source of truth that cannot drift from the CLI, and
`x2rock skill` writes it into `~/.claude/skills/x2rock/` (or `$CLAUDE_CONFIG_DIR/skills/`, or
`--dir`), `--print` for a non-Claude agent. It leads with the two contracts this arc built:
`status --json` first, and read the error `code`/`fix`.

### What dogfooding surfaced (and the shape of it)

Two agent sessions driving the CLI found gaps a human review missed, all of the same shape - *a
field an agent would read is missing or lying*:

- `on_tv` and `audible` as fields, so "is the TV on" and "will this make a sound" are reads, not a
  `"TV Audio"` title match or a volume-vs-mute deduction.
- `service_id` alongside `service`, because the player leaves `service` null for some sources while
  carrying the sid; and then `status`/`now` resolving that sid to a name from the cached catalogue,
  because favorites names YouTube Music and now-playing did not. (Favorites was not "resolving" - the
  player simply populates the field there and not in now-playing metadata.) A second agent then found
  the sid itself wrong for **HLS/stream content**: two rooms on YouTube Music at the same moment, one
  reporting `serviceId` 284 (a track URI) and one 65435 (an `x-sonosapi-hls-static` stream), while
  *both* art URLs carried the correct `sid=284`. So the **art URL is the reliable sid** - the player's
  metadata object carries a wrong or internal id for HLS - and `service_id` now reads from the art
  URL first, the metadata object only as a fallback. One `status` call reproduced it, which is the
  whole argument for the snapshot: an A/B across rooms in a single reply.
- `favorites --json` marking `playable: false` for empty shells (no service and no type) - the
  dead-streaming-service favorites a long-lived household accrues. A heuristic, and it says so: it
  cannot see a live service that **recycled an id** (iHeartRadio swaps stations for seasonal ones at
  the holidays), which nothing can detect.
- `favorite "<name>"` naming the ids of two favorites that share a name rather than silently picking
  the first.

The through-line: the fixes were cheap; *finding* them needed an agent, because each is invisible
until something parses the contract instead of reading it.

## TuneIn (New): the first front-door AppLink completion (2026-09-04)

`x2rock link "TuneIn (New)"` (sid 333, `Auth="AppLink"`) completed end-to-end: `getAppLink`
answered with a `regUrl`/`linkCode` pair, the browser step was an ordinary consent page, and
`getDeviceAuthToken` minted a token on the first poll after approval. **This is the first of the 62
app-link services to fall through `getAppLink` itself** — Plex fell *around* it, through its own
PIN flow — and it vindicates the ask-anyway design: the generic attempt that Plex refused and
YouTube Music 403'd is exactly what linked here, unmodified.

### The URL says how the flow actually works

The post-approval confirm page was captured live, and its query string is the whole mechanism in
one line:

```
https://tunein.com/authorize/confirm/?client_id=oVzZq8nb&pairauthflow=true
  &redirect_uri=https%3A%2F%2Fsonos.platform.prod.us-west-2.tunein.com%2Faccount%2Flink
  &response_type=code&serial=<householdId>&showupsell=true&state=<base64>
```

- `response_type=code`, `client_id=oVzZq8nb` — a textbook OAuth authorization-code grant, with
  Sonos's client identity at TuneIn sitting in the open. What is *absent* matters most: no secret
  gate before consent. This is the exact spot where YouTube Music demands its sealed `apiKey` —
  TuneIn's client identity is a name tag, YTM's is a locked door.
- `state` decodes (plain base64 JSON) to `{HouseholdId, LinkCode, RedirectUri: null}` — the pairing
  nonce from `getAppLink` rides in the OAuth `state` parameter. The consent leg and the polling leg
  are joined by nothing but that 32-hex nonce.
- `redirect_uri` is **TuneIn's own Sonos-integration backend**
  (`sonos.platform.prod.us-west-2.tunein.com/account/link`), not sonos.com and not any device. The
  OAuth loop closes entirely server-side at TuneIn: browser consents → TuneIn issues the code to
  its own backend → the backend exchanges it, reads the `LinkCode` out of `state`, and marks the
  household's link approved → the `getDeviceAuthToken` poll flips from `NOT_LINKED_RETRY` to a
  token. **x2rock never touches the OAuth leg at all.**

TuneIn's success page reads "All set! Now use the Sonos app to listen to TuneIn — TuneIn account
linked to your device." Its backend cannot tell x2rock from the Sonos app; the link is fully
ordinary on their side. So "app-link expects the service's mobile app" is per-service *policy*,
not tier structure — the PC-controller correction (2026-09-01) predicted this, and a Linux desktop
browser has now demonstrated it. The flow-shape taxonomy grows to five: Bandcamp (code in
`regUrl`), iHeartRadio (typed code), Mixcloud (OAuth, `linkCode` in `redirect_uri`), Plex (SMAPI
link dead; own PIN flow), TuneIn (OAuth, `linkCode` in base64-JSON `state`, service-side redirect).

### What worked, and the one loose end

Search verified the same minute: `search -s "TuneIn (New)" "radio paradise"` returns the station
and its Mellow/Rock/Global mixes (all hits `type: stream`, `queueable: false`). The loose end:
**the `getDeviceAuthToken` reply carried no `userIdHashCode`**, so the household could not be told
about the account (`link` says so at the end). Playback through the household is therefore
untested — though these are streams, which go through the stream path rather than the enqueue
path's credential substitution, so the missing registration may never bite. Tested the same day,
from the office household: it does not bite for TuneIn. See the Radio Paradise section following.

Two small observations for the file: `serial=` hands the household id to TuneIn *before* consent —
a mild privacy leak inherent to the flow shape — and `showupsell=true` is TuneIn's, not Sonos's.

Next probes, priced by this result: Radio Paradise (308) and iBroadcast (310) as the small-operator
bets, Pocket Casts (233) for the podcast content type, and Spotify (12) as a one-call wall-pricing
probe for the major-streamer class (does it 403 like YTM, or answer like TuneIn?).

## Radio Paradise, and what plays without a household registration (2026-09-04)

Radio Paradise (sid 308) linked minutes after TuneIn — the second front-door `getAppLink`
completion — and it was linked **anonymously**: its consent page offers a "login anonymously"
button, and the token minted behind it has no account at all. That is a third client posture for
the taxonomy: TuneIn asks for a real account through ordinary OAuth, YouTube Music walls the
endpoint on a sealed client key, and Radio Paradise hands a token to anyone who asks. (Two small
practical notes: the catalogue's name is "Radio Paradise" — `link "Paradise Radio"` finds nothing —
and the reply again carried no `userIdHashCode`.)

It is also the first linked service where **search is impossible by design**: RP publishes no
search categories, so `search -s "Radio Paradise"` correctly refuses with "publishes no search
categories". The service is browse-only, and browse works fully: three bitrate containers (128k,
320k, FLAC), each holding seven channels — The Main Mix, Mellow Mix, RockIt!, The Globe, Beyond…,
Serenity, KFAT — `type: program`, `queueable: true`.

### The office household answered the registration question, in both directions

The laptop moved to the office the same morning — a *different* household, one that has certainly
never been told about either new account — which made the `userIdHashCode` caveat testable
directly. (The move itself was routine and worth a line: both gateway fingerprints were remembered,
the daemon reconnected 8 seconds after resume, and its one transient failure logged as
`no player: could not identify this network (no default gateway)` — correctly prefixed, since the
wifi simply had no gateway yet. The per-controller SMAPI tokens travel with the laptop: search and
browse for both services work from the office unchanged.)

- **Radio Paradise (`program` item): fails both paths.** `AddURIToQueue` → UPnP 800 — the exact
  signature of the disconnected-YouTube-Music case: no registration, so the player has no
  credential to substitute. The stream fallback then dies harder: RP answers `getMediaURI` with
  "Function 'getMediaURI' doesn't exist" — it implements no direct stream resolution, so its
  programs are playable only by a player that can resolve them itself.
- **TuneIn (`stream` item): plays.** `search -s "TuneIn (New)" "npr" --play 1 -r "Media Room"` went
  BUFFERING → PLAYING with `position_ms` advancing — real sound, no registration anywhere.

So the link-time warning "search works; playback through the household may not" resolves
precisely: **playback without a household registration works iff the service implements
`getMediaURI` and the item is a stream.** Anything the player must resolve itself needs the
registration x2rock cannot yet create. The anonymous RP link buys browse-only until then; the
TuneIn link is usable end to end.

## `playback:1` carries errors too, and they parsed as statuses (found 2026-09-04)

`X2ROCK_LOG_EVENTS` was added to catch a partial `playbackStatus` in the act - a body with no
`playbackState`, seen four times in a day on firmware 95.0-77060 and made harmless by treating the
missing field as *unchanged*. Sixteen hours and 143 bodies later it has caught **none**. What it
caught instead was a different body on the same namespace, and a worse bug.

**`playback:1` delivers two shapes, and only `_objectType` tells them apart.** A stream failure
sends a `playbackError`:

```json
{"_objectType":"playbackError","errorCode":"ERROR_PLAYBACK_FAILED","reason":"ERROR_CANT_REACH_SERVER",
 "serviceId":-1,"serviceName":"https:","trackName":"Apple Music Chill","itemId":"VXiDuCcc…"}
```

Note `serviceId: -1` and `serviceName: "https:"` - a direct stream URL, not a linked service. A
second error followed three seconds later carrying no `itemId` at all, so none of these fields can
be relied on.

**Why it mattered.** Every field of `PlaybackStatus` is optional and *none of them appears in an
error body*, so a `playbackError` deserialized perfectly into a status full of `None`s. Making the
event tolerant of a missing `playbackState` had therefore made it tolerant of a body that is not a
status at all: the error folded in silently as "nothing changed", and the only notice that the
music had stopped was discarded. Before that tolerance existed it at least failed serde loudly.

**And it did more than lose the error.** `availablePlaybackActions` and `playModes` were
`#[serde(default)]` - all-false - and were assigned *unconditionally*. All-false is not "unknown",
it is "this source allows nothing": the room published `CanPlay(false)`, `CanPause(false)`,
`CanGoNext(false)`, `CanSeek(false)`, and reported a repeating, shuffling queue as doing neither.
Media keys and desktop applets went dead for that room on every failed stream. The fix for the
missing `playbackState` had covered two fields of five and left these two resetting.

So both halves are now `Option`, `None` means unchanged for all five, and `playback_error()` reads
`_objectType` before anything tries to parse a status. The error is logged rather than published -
MPRIS has no property for "that did not play", and the journal is where the answer to "why did the
music stop overnight" belongs. The properties MPRIS is told about are read back off the room's
state *after* the update rather than off the body, so a field the body omitted re-announces what is
still true.

**Severity was limited by luck rather than by design.** Both times, a full `playbackStatus` arrived
in the same second, so the blanked window was under a second. Nothing guarantees that ordering, and
in the 04:34 burst the error came *last* - which would have left the wrong capabilities standing
until the next event.

The original quarry is still unaccounted for: a partial `playbackStatus` has been reproduced only
synthetically. The grep that hunts it needs the type filter too, or `playbackError` answers it:

```sh
journalctl --user -u x2rock.service --since today \
  | grep 'playback:1 {"_objectType":"playbackStatus"' | grep -v '"playbackState"'
```

## EQ is UPnP-only, and loudness was on the whole time (verified 2026-09-04)

A parity pass against the legacy Windows controller started with one checkbox: its EQ panel for
Media Room showed **Loudness ticked**, bass and treble centred. The speaker agreed - `GetLoudness`
answers `1` - so it has been on since the day it was unboxed, which is the factory default and not
something anyone here chose.

**The Control API cannot reach any of this.** The namespaces verified against real players are
`playback:1`, `playbackMetadata:1`, `groupVolume:1` (group-scoped), `playerVolume:1`,
`homeTheater:1`, `audioClip:1` (player), and `groups:1`, `favorites:1`, `playlists:1`,
`musicServiceAccounts:1` (household). There is no EQ namespace among them, and `playerVolume:1`
carries only volume, muted and fixed. The one door is UPnP `RenderingControl:1` on port 1400 -
touched once before, for the TV audio format, and found not to carry that either.

**The service publishes its own contract, so nothing here was guessed.**
`http://<ip>:1400/xml/RenderingControl1.xml` gives the argument names and the ranges:

| action | arguments | out |
|---|---|---|
| `GetBass` / `GetTreble` | `InstanceID` | `CurrentBass` / `CurrentTreble` |
| `SetBass` / `SetTreble` | `InstanceID`, `DesiredBass` / `DesiredTreble` | - |
| `GetLoudness` | `InstanceID`, **`Channel`** | `CurrentLoudness` |
| `SetLoudness` | `InstanceID`, **`Channel`**, `DesiredLoudness` | - |

- **`Bass` and `Treble` are `i2` with `allowedValueRange` -10..10, step 1.** Read off the device
  rather than inferred, and the CLI checks both levels before sending either, so a rejected treble
  cannot leave an accepted bass already applied.
- **Loudness alone takes a `Channel`** (`Master`), which bass and treble do not. Omitting it gets a
  UPnP 402, which reads like a bad value rather than a missing field - worth knowing because the
  asymmetry is invisible until the call fails.
- **The wire says `1` and `0`, not `true`/`false`**, so `parse::<bool>()` rejects it. The reply is
  compared as text instead.
- **The setters answer with an empty body**, so the CLI reads the tone back after writing rather
  than echoing what was asked for. What the speaker now holds is the only truthful report.
- **It is per player, never per group.** The app's own panel is titled "EQ Settings for <room>";
  two speakers playing together share a group volume and keep their own tone. So `x2rock eq`
  resolves `--room` to the *speaker*, the way `vol --player` does, and `--all` - which is
  per-group - does not apply to it.

Why the checkbox mattered beyond parity: loudness is a low-frequency lift that does most of its
work at low listening levels, which is exactly where this household listens - the earlier question
was whether a room could go quieter than volume 1. It cannot, in Sonos steps; turning loudness off
is the control that actually changes what volume 1 sounds like.

## Open questions

1. **The app-link barrier, and YouTube Music discovery specifically** (narrowed 2026-08-31 from
   "which services the picker should offer" — the picker half is decided, see "The picker discovers
   linked services" above).

   Two loose ends that block nothing. **`match`** has never succeeded — see "`match`, and why
   nothing needs it yet". **Bandcamp** stays deferred until there is something in the collection.

   The 62 app-link services remain a separate call — though no longer a uniform one: **Plex fell
   outright** (searched, browsed and played the same day it was asked for; see "Plex: the first
   app-link service to fall"), and `x2rock link` now asks any app-link service for a browser page
   via `getAppLink` rather than refusing on the tier alone. And **the barrier is now one wall
   shorter than this list used to claim.** "Protected streams need `httpHeaders` or `contentKey`, which
   `loadStreamUrl` cannot carry" was true and is no longer the whole story: the enqueue path does not
   resolve the stream at all, so the player supplies its own credential and protected content plays.
   A kept YouTube Music track demonstrates it — and the same day showed the condition it rests on,
   in both directions: the household's account was disconnected and the same id refused at enqueue
   with UPnP 800, then the account was re-added and the id played again, under the new serial. See
   "The YouTube Music account was disconnected" and the resurrection section after it. The
   mechanism is real but **conditional on the household holding an account for the service**,
   which x2rock can neither create nor detect in advance.

   So for YouTube Music specifically, **playback is exactly as solved as the household's
   registration is present — only discovery is missing on x2rock's side.**

   **The discovery half: the sealed-key path is closed, but discovery is not — re-opened
   2026-09-01.** This entry first said one judgement call stood between here and a search (present
   the manifest `apiKey` or not); that was false, because the `apiKey` is two **encrypted
   envelopes** (RSA-1024-wrapped key, AES payload, tagged with the fingerprint of a private key in
   the Sonos app and player firmware), so there is no key to present and getting one means
   extracting a private key from a binary — circumvention, out of scope. Then the *same day* the
   endpoint itself corrected the framing: `music.googleapis.com/v1:sendRequest` returns **401
   "Expected OAuth 2 access token"** the instant a Bearer is presented, so it accepts OAuth as an
   alternative identity and **the sealed key is one door, not the door.** The live task is now
   narrow and testable — does a self-service Cloud OAuth client's token clear the endpoint, or is
   it pinned to Sonos's `client_id`? — and it needs the user to create the OAuth client. Full
   probe, decision tree and the second wall (id namespace) are in **"TASK: the OAuth identity
   probe"** inside "The YouTube Music `apiKey` is sealed". The **registered-key proxy** recorded
   there stays the right pattern for a future service with a registerable endpoint; it does not fit
   this one.

   Also corrected there: **the Sonos PC controller completes app-link**, so the "hand-off needs the
   service's mobile app, which Linux cannot do" wall this entry used to claim was never real. The
   presentation map specifies YouTube Music search in full, nine categories across two groups — the
   interface is defined and waiting, and an accepted identity for `sendRequest` is the whole of
   what stands in front of it.

   What remains open under this heading is only the *rest* of the app-link tier — and one of them
   has now answered: **TuneIn (New) linked through `getAppLink` outright** (see "TuneIn (New): the
   first front-door AppLink completion"), proving the tier is per-service policy rather than a wall.
   Asking costs nothing. YouTube Music remains not one of them.

   Worth noting what is *not* a route, so it is not re-tried: the Control API has no content
   discovery anywhere in its 53 paths, UPnP `Search` reports empty `SearchCaps`,
   `musicService:1 search` does not exist and that namespace is about accounts, and `GetSessionId`
   answers 806 even for a service that plays. And the object id cannot be derived from outside —
   `ALkSOiGTPQu20Hqb6iEmeMhGFI_jhhXgHyx7WTjmO6bs1i3H` is 48 opaque characters, not a YouTube video
   id, so searching YouTube by another route gives nothing a player would accept.

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

- ~~Should a bookmark store the account serial?~~ — **closed 2026-08-31: the question dissolved.**
  The player ignores the serial on the enqueue path and resolves the household's current
  registration for the sid, so storing one can neither pin nor break anything — proven when the
  same bookmark played as `sn_16`'s successor `sn_20` after a remove-and-re-add of the same
  YouTube Premium subscription. Provenance at most. See "Re-added the same day: `sn_20`, and the
  bookmark resurrected".

- ~~Should x2rock present Sonos's API key to unlock YouTube Music search?~~ — **closed
  2026-09-01: the question dissolved, like the bookmark-serial one before it.** There is no key in
  the manifest to present — only two encrypted envelopes whose private keys live in the controller
  app and the player firmware, so the act was never "send a public string" and always "extract a
  key from a binary". Recorded with the byte layout, entropy controls against random baselines, a
  reproduction script, and the objections it pre-answers, in "The YouTube Music `apiKey` is sealed,
  and that closes the question". Two older claims died with it: the manifest does not carry a
  usable key, and app-link does not require a mobile hand-off — the Sonos PC controller does it too.

- ~~Should the picker discover services itself, or keep the configured-by-hand bargain?~~ —
  **decided 2026-08-31: discover what was linked, hand-configure what was not.** Linking is itself
  configuration, so the picker reads `x2rock accounts --json` on open; the anonymous services stay
  behind `searchService`/`browseServices`, and an explicit `browseServices` overrides everything.
  See "The picker discovers linked services (decided 2026-08-31)".

- ~~Music search is out of scope~~ — **decided 2026-08-29, reversed 2026-08-31.** The 08-29 entry
  gave two reasons. The first, that search "needs SMAPI, with per-service endpoints and
  authentication", was wrong: it read *service* authentication as a *Sonos account*, when a service
  is linked to the household and the LAN gives up the endpoint for free. The second — that
  `AddURIToQueue` refuses service-backed containers and stations, so a search might have had
  nothing it could enqueue — **is refuted, and this summary said otherwise long after the body of
  the document had settled it.** A service *track* enqueues and plays: first from a phone-started
  album (see "A service *track* can be enqueued"), then from a search hit, and finally as the
  mechanism `play-item` now uses for all on-demand content. Only containers and stations are
  refused, which is the distinction `upnp:class` draws.
  The entry is kept rather than deleted because the way it went wrong is worth remembering:
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
