# Changelog

## 0.1.0 (not yet released: when tagged, `## 0.1.0 (YYYY-MM-DD)`, which the release checks)

First release of `x2rock-deck`, a native Sonos controller for the Steam Deck, Linux desktops, and Windows tablets, built with Slint.

### Features

- **Rooms & Playback**: Play, pause, skip, seek, shuffle, repeat, per-room and per-speaker volume (a held button steps at most every 250 ms and 20 per hold); grouping and party mode, ordered so a regrouping waits for what was sent before it.
- **Music & Search**: Sonos favourites, playlists, queue, internet radio, and unified search across all household music services.
- **TV & Tone**: Soundbar TV input switching, Night Sound, Speech Enhancement, EQ (bass, treble, loudness), Sub and surround levels.
- **Household & Alarms**: Household alarm creation and editing, system update status.
- **Controls**: Gamepad (the Steam Deck's controls, or a pad Steam Input or the system presents as Xbox-style), keyboard, and touchscreen tap and hold. Untried on a Steam Deck; see below.
- **Artwork**: LRU memory-bounded album art caching with 4-permit concurrency and separate thumbnail / full-art caches.
- **Theming**: Six dark themes, including dynamic album-art accent coloring.

### What has not been tried

This release was developed on Linux desktops. Its only runs against real speakers were
read-only, on a single-room household (Media Room, 2026-10-30): home, search, Music Services,
a Deezer page, room settings and reading the household's sign-ins. Nothing below has been
tried; each is tracked in [#7](https://github.com/rahga/x2rock/issues/7).

- **Changing anything on a speaker from the app.** Play and pause, volume, queue edits,
  favourites, alarms, grouping and settings have never been sent to a real household.
- **A Steam Deck**, in Gaming Mode or Desktop Mode: the gamepad, Steam's keyboard (Steam + X),
  and installing with `install.sh`.
- **Windows.** The Windows build has only been built in CI, never run: touch, tap hit-testing
  and the volume slider are untried.
- **More than one room:** grouping and Play everywhere.
- **Soundbars** (TV input, Night Sound, Speech Enhancement, remote settings), **Sub and
  surround** levels, a book's chapters (Audible).
- **Line-In (beta).** No speaker in the development household has Line-In. The app sends
  `playback:1 loadLineIn` naming the speaker; what a speaker does with it is unknown.
- **A speaker dropping off and coming back**, and artwork memory over a long session (the
  64 MiB budget).
