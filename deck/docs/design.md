# The look, as numbers

What x2rock for Android TV draws, measured from its source and its screenshots, and what
this port draws to match. Lengths are the TV app's dp, which this app uses unchanged: the
window shows a canvas at least 960x540 of them, scaled to fill it (`zoom` in `ui/app.slint`).

Source of truth is the TV app; file pointers are into `/home/rah/x2rocktv/app/src/main/java/com/rahga/x2rock/`.

**Canvas.** 960x540 dp design, 48dp title-safe margin, pane padding 48 h / 32 v, rows 20dp apart.
Sidebar 340dp on `surfaceVariant` (`ui/screens/HomeScreen.kt:969`).

**Themes** (`ui/theme/Theme.kt:63-138`): primary / onPrimary / primaryContainer / onPrimaryContainer /
background / onBackground+onSurface / surface / surfaceVariant / onSurfaceVariant
- Ocean: 4FC3F7 / 003048 / 00607A / B3EBFF / 0A1929 / E3F2FD / 0D2137 / 1A3A50 / B3EBFF
- Ember: FF7043 / 3B1100 / 712600 / FFDBCF / 1A0A00 / FFF3E0 / 2D1000 / 4A1E00 / FFDBCF
- Forest: 66BB6A / 003910 / 005320 / A9F4B5 / 061209 / E8F5E9 / 0D2110 / 1B3A1E / A9F4B5
- Orchid: CE93D8 / 3E0056 / 5B0080 / F2DAFF / 120D16 / F3E5F5 / 1E1525 / 36204A / F2DAFF
- Default: Material dark baseline (swatch 6650A4). Artwork: Default with primary = cover accent
  (swatch E6A15A). The cover's shade tints the pane gradient under every theme.

**Focus language.** Buttons fill `primary` with `onPrimary` text when focused/pressed; cards get a
3dp `primary` border; selected row onSurface 10%, focused row 14%; nothing scales.

**Radii.** Cards 8, row art 4, pane art 8, full-screen art 12, NoticeBanner 12, search field 8,
VolumePill fully round.

**Room row** (`HomeScreen.kt:562-714`): padding 32/28 h, 6 v; inner 16/8; 56dp art (or 28dp TV/radio
glyph at 70% on a 10% tint); 14dp gap; name titleMedium + 2–3 bodySmall lines; right column
DotEqualizer (24dp box) over an 18dp badge row; non-playing rooms art at 0.62 alpha and lines in
onSurfaceVariant.

**Player pane** (`ui/screens/PlayerScreen.kt`, `HeaderVolume.kt`, `SeekBar.kt`, `PaneButtons.kt`):
header titleLarge room + " · state" titleMedium (primary if playing); VolumePill = − StepMark, 30dp
speaker icon, 84x28 wedge (track 25% ink), level in headlineSmall at the width of "100", + StepMark;
track block ≥168dp tall, art 168dp r8, 32dp gap, title displaySmall ≤2 lines, "artist • album"
bodyLarge, stream info bodyLarge; SeekBar 8dp (12 focused), 20dp knob, primary/secondary when
focused, position/duration bodySmall, middle slot for "Up next"/burst total/"‹› seek"; transport
row centred, 12dp spacing, Shuffle Prev Play/Pause(≥104dp wide, 36dp icon) Next Repeat then
thumbs or heart/ban, each drawn only when allowed; places row 16dp below, 16dp spacing, Queue
Browse Search Save-favourite Group Configure Full-screen, label under the focused one only;
Speakers section rows of 150dp name + VolumePill; TV variant with TwoLineButtons ≥220dp.

**Key grammar** (`HomeScreen.kt:751-809`, `components/DpadMenuKey.kt`, `MainActivity.kt:101-138`):
D-pad moves focus; ←/→ on a pill/tone row steps; on the seek bar bursts 10/20/30 s; on the theme
row cycles; Select clicks, on volume mutes; hold/Menu on volume opens the exact-level overlay
(5 10 15 20 25 30 40); hold on Play/Pause = thumbs up; hold/Menu on a row = item menu; Back is
nested (panel, settings, moving room, pane→sidebar); media keys are global; volume bursts
+2/+3/+5 and finer below 10. Hold-to-move: pick up by Select hold or Menu; Up/Down moves; Select
or Back drops; Left offers "Play everywhere?", Select confirms, second Left also evens volume.

**Components** (`ui/components/*`): DotEqualizer 4x5 dots, lit 80% / unlit 14%, primary at 80%
tint, 4.4 s loop; DotScanner 8 blocks of 12dp, 5dp gaps, 1.8 s sweep with trail; StepMark alpha 0
unless focused or touch mode, 40% when muted; NoticeBanner bottom-centre 32dp up, ≤720dp,
inverseSurface/inverseOnSurface, padding 24/14, never focusable; MediaRow padding 16/10, optional
40dp number, 56dp art, 16dp gap, titleMedium marquee, bodyMedium subtitle, 2dp primary border when
active; ItemMenu 520dp, 24dp padding, list ≤340dp of full-width AppButtons, section titles
titleSmall primary; RoomPanel 480x≤520, 24dp padding, rows 8dp apart, labelMedium 60% section
labels; SettingsPanel 380dp over a 60% scrim.

**Where this port departs from the TV's metrics, and why.** The alarm editor's hour and minute
are two cells of a list grid, turned by Up and Down, rather than the TV's displayMedium wheels:
the list screen already owns focus and scrolling, and a cell reads at arm's length. Send a
report's code sits in a 300dp column rather than 360dp, so the report's blocks keep room on the
960dp canvas; it is drawn at whole modules with its own four-module quiet zone, and only ever
scaled down.

**Service logos**: `tools/service-logos.py` output (`ui/ServiceLogos.kt`) → `service-logos.json`
in the app, matched on letters+digits (exact, else longest prefix ≥5), fetched at `?w=112&fm=png`.

**Dropped (Android-only)**: TV home-screen channel, `MediaSession` (replaced later by MPRIS on
Linux), deep links, Leanback launcher, system IME/voice. **Kept with a new backing**: QR report
(Rust `qrcode`), accent extraction (Rust), seeds per network (core), ratings cache (core).

