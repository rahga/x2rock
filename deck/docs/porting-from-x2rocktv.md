# Porting x2rock for Android TV to the Steam Deck

The record of what this port decided and what it found, so the next change does not have to
find it again. Begun 2026-10-29.

## What was decided

- **A reimplementation, not a port of the code.** The TV app is Kotlin and Jetpack Compose
  for TV; this is Rust and Slint. What carries over is the look (`docs/design.md`), the
  navigation rules and the wording, with the TV app's tests ported where they pin a string.
- **The Sonos layer is the x2rock CLI's**, as a library: one implementation of the protocol
  for every Linux and Windows front end, rather than a third copy beside the Kotlin `:core`
  and the CLI. Its package gained a library target and a default `cli` feature (branch
  `gui-library` of `rahga/x2rock`); a front end depends on it with `default-features = false`
  and gets neither clap, the TUI nor D-Bus.
- **Slint, under its GPLv3 licence.** Declarative components with states and animations, a
  software renderer for headless screenshots, Linux and Windows first-class.
- **Plain binaries, no Flatpak.** One self-contained executable per platform, installed for
  the user (`dist/install.sh`), added to Steam for Gaming Mode.

## Changes made to x2rock (the CLI) for this

All on its `gui-library` branch, every test passing after each, and on `main` since the deck
moved into that repository (2026-10-30):

1. A library target: the Sonos layer and its helpers, with the CLI, daemon, MPRIS and TUI
   left in the binary behind a default `cli` feature.
2. File modes and the state directory compile off Unix (modes only where they exist; the
   local data directory where XDG has no state directory).
3. `Clone` on the protocol's reply types and the UPnP data types, for a front end that keeps
   them.
4. `art::local_file`, the art cache's one entry point for a front end in the same process.
5. **`x2rock::play`**: the play path (`play_item` and every route it takes: the queue, a radio
   program as the room's source, a stream session, `loadContent`) moved verbatim out of the
   `content`, `stream`, `playback` and `services` commands, which re-export it. A service item
   plays here exactly as `x2rock play-item` plays it. Saving the playing item as a favourite
   (`save_playing_favorite`) and rating (`rate`, taking the library's `RatingKind`, and
   `rating_offers`) followed.
6. **`x2rock::import`**: reading the household's stored sign-ins (`link --from-household`'s
   core), returning what it kept.
7. **`discover::ssdp`**, a first try ahead of the subnet sweep.
8. **`netid`** off Linux, through `netdev`, so Windows identifies its network.
9. **Numeric EQ settings** (`Upnp::eq_value`, `set_eq_value`): a Sub's and surrounds' on/off
   and levels, as the TV app's Configure sets them. Tested against the `GetEQ` reply the TV
   app captured off a Beam.

## What was found

- **Setting the window's scale factor resizes a winit window.** The first fitting of the TV
  app's 960x540dp canvas set the scale factor from the window's physical size; winit then
  resized the window to keep its logical size, the new size asked for a new factor, and the
  window shrank a step at a time to its minimum. The headless renderer cannot show this,
  since nothing resizes its window; a log of a real window did. The canvas is now one
  element scaled by `transform-scale` (`zoom` in `ui/app.slint`), and the window keeps the
  size and scale the system gives it.
- **The software renderer does not draw `transform-scale`**, so headless screenshots still set
  the scale factor, which is safe there. A real window (femtovg) was checked with the app's
  own `take_snapshot` (`X2ROCK_DECK_SNAPSHOT`), and touch with a synthetic tap
  (`X2ROCK_DECK_TAP`): a tap lands where it is drawn on the scaled canvas.
- **`grim` cannot capture from the sandbox this was built in**; the in-app snapshot is the
  way to see the real renderer.
- **Debug builds overflowed tokio's 2 MiB worker stacks** on the play and search routes. The
  runtime's workers have 8 MiB and the largest futures are boxed.
- **x2rock stores a refreshed token, and only stores it**: `play::after_refresh`, and the play
  routes inside `play_item` and `enqueue_item`, write a service's new token to the credential
  file but not to a `Credentials` a caller holds. The CLI loads the file each run and carries
  the returned token within it, so it never sees the difference; a process that keeps the
  accounts open does, and sends the replaced token next time. The services here read the file
  again after anything that may have refreshed one.
- **The CLI never scans on its own** (`session::connect` refuses a network it has not
  discovered); an app someone has just opened must, so the runtime sweeps the subnet when
  this network has no remembered household.
- **Palette's skin-tone band is real**: a cover whose only vivid colour is orange (hue 10-37)
  gets no vibrant accent, in AndroidX and in this port alike.
- **Live screenshots are read-only**: `--live` fetches lists and art and sends nothing that
  changes the household, which is what made checking against a real household safe.

- **A non-Steam shortcut knows its own app id**: Steam sets `SteamGameId` to it shifted up 32
  bits, over `0x02000000`, which is how the app finds where its library artwork goes.
- **The Windows build can be checked from Linux**: `cargo xwin check` and `build` for
  `x86_64-pc-windows-msvc` work with clang and the toolchain's `rust-lld` as `lld-link`; the
  first build was a console-subsystem executable, which a double-click would have opened with
  a console window beside it.

- **Send a report's code is drawn at whole modules, quiet zone included** (`report::qr_pixels`),
  and the screen only scales it down. It decodes (`zbarimg`) from screenshots at 1280x800,
  1920x1080 and 2560x1600. Its link goes to `rahga/x2rock`'s issues, where
  the deck now lives.
- **A review of the first build found two paths that sent commands nobody asked for**, both
  fixed with tests that fail without the fix: a Left held from the pane went on to pick the
  room up and confirm Play everywhere with every speaker at its level, and Select on the Group
  panel's first row sent the coordinator out of its own group. It also found focus held as a
  bare index (a vanishing Next left a press on Repeat), lists stored into whichever screen was
  on top, a failed subscription never retried, presses queued while reconnecting, and Return
  on the on-screen keyboard submitting. All fixed; the retry is untested on hardware.
- **Commands execute in per-target serial lanes, with barriers for topology changes**: Rapid
  adjustments (such as volume ramps, seek bursts, or EQ tone tweaks) can race when sent
  concurrently over async network calls. Commands now route through dedicated FIFO lanes per
  speaker or group (`src/lanes.rs`), allowing reads to proceed immediately beside them.
  Superseded scalar values replace pending ones in the lane. Grouping mutations (join, leave,
  split, party) run across a barrier that waits for pending operations to settle and holds
  back subsequent lane work until the topology change finishes.
- **The services lock is never held across asynchronous network I/O**: Holding the shared
  services mutex across network requests blocked simultaneous browsing and playback commands.
  Locks are now acquired strictly to read and commit cached state and credentials. Abandoned
  searches and closed search screens cancel in-flight requests, and refreshed tokens are retained
  immediately in memory as well as on disk.
- **Gamepad holds and repeats track each device independently**: Disconnecting a controller
  releases all its held actions and stops its repeat timer, preventing stuck keys. Held
  directions and volume buttons repeat after 400 ms then every 90 ms. Noise from analog stick
  axes is still delivered, but `Gamepads::step` runs due repeats on every event and sleeps until
  the next deadline, so axis events no longer delay repeats.
- **Screen-bound fetches carry monotonic request identities**: Screen list and search requests
  carry an `id: Option<u64>`. Screen views accept only responses matching their active request ID,
  preventing late responses from closed screens or stale searches from corrupting the view, while
  untagged background updates (such as rating offers, resume offsets, and local Favorites/Alarms
  edits) remain intentionally accepted. On network reconnect, only pending requests for active
  screens on the stack are re-issued.
- **Artwork memory budget and concurrency throttling**: Image fetches and decodes share a
  concurrency limit of 4, and completed buffers retain their permit until consumed by the UI.
  A 64 MiB LRU cache evicts decoded images by render recency, separating 168×168 thumbnails
  from 512×512 full covers. Failed loads retry after 30 seconds for transient errors or 30
  minutes for permanent errors (HTTP 404/410, corrupt images).

## Not yet done

- From the TV app: the item menu's **Add to playlist**, which needs SMAPI calls x2rock does
  not make yet (finding the account's writable playlists by `userContent`/`readOnly`, and
  adding to one), and an account to try it on. A portable's battery line (a beta there).
- **Checks owed on hardware for what was added here without one**: Sonos favourites added and
  removed from the item menu, a book's Chapters (Audible), a soundbar's remote settings, and
  the retry of a speaker that drops off.

- **Alarms are made and changed as on the TV, and that has only run in the demo and the
  tests**: room, time, how often and sound a step to a screen, then the summary with its
  volume. The time's hour and minute sit side by side, turned by Up and Down as the TV's
  wheels are. A change is written over a fresh read; a refusal keeps the editor open with it
  said ("Kitchen already has an alarm at 07:00", UPnP 801).
- **Line-In is a beta here as on the TV, and has never run on hardware**: no speaker in either
  household has one. It sends `playback:1 loadLineIn` naming the speaker; what a room on its
  line-in reports is unknown, so nothing recognises it. Release notes must say so.
- MPRIS on Linux, so the desktop's media keys and bar see the selected room. The x2rock
  daemon publishes every room over MPRIS already, beside this app.
- **The Sub and surround rows have only run in the demo.** They are offered only where
  `zones:1` names a speaker on `SW` (a Sub) or `LR`/`RR` (surrounds) in the room of the
  speaker the panel reads, and written to that speaker, as the TV app does. No household this
  was built against has either.
- Checks owed on hardware: a Steam Deck in both modes, a Windows tablet by touch, a household
  with a soundbar for the TV pane, and grouping on more than one speaker. The development
  household had one One SL.
- Specific checks owed on physical hardware from D2–D6:
  - Gamepad repeat cadence (400 ms initial delay, 90 ms repeat) and feel on physical Steam Deck controls.
  - Windows touchscreen hit testing and volume adjustment on scaled Slint canvas.
  - Live speaker regrouping and party mode barrier ordering across multiple physical zones.
  - Power-cycling a speaker under active subscriptions to verify reconnect recovery on a live network.
  - Long-running artwork cache memory stability under live album art traffic.
