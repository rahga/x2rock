# Changelog

## [0.1.0] - 2026-10-30

First release of `x2rock-deck`, a native Sonos controller for the Steam Deck, Linux desktops, and Windows tablets, built with Slint.

### Features

- **Rooms & Playback**: Play, pause, skip, seek, shuffle, repeat, per-room and per-speaker volume with paced repeat acceleration; grouping and party mode with barrier ordering to preserve sequence.
- **Music & Search**: Sonos favourites, playlists, queue, internet radio, and unified search across all household music services.
- **TV & Tone**: Soundbar TV input switching, Night Sound, Speech Enhancement, EQ (bass, treble, loudness), Sub and surround levels.
- **Household & Alarms**: Household alarm creation and editing, system update status.
- **Controls**: Full support for Steam Deck gamepad controls, keyboard navigation, and touchscreen tap/hold gestures.
- **Artwork**: LRU memory-bounded album art caching with 4-permit concurrency and separate thumbnail / full-art caches.
- **Theming**: Six dark themes, including dynamic album-art accent coloring.

### Important Notice: Beta & Hardware Status

- **Line-In is a beta feature and has not been tested on hardware**: No speaker in the development household has Line-In; it sends `playback:1 loadLineIn` naming the speaker, but live response behavior remains unverified.
- **Hardware checks owed**: The application was developed on Linux desktops and verified read-only against a single-room Sonos household (Media Room). Real Steam Deck physical controls and modes, Windows touchscreen hit testing, multi-room group barrier ordering, and soundbar settings have not yet been verified on physical hardware (tracked in [#7](https://github.com/rahga/x2rock/issues/7)).
