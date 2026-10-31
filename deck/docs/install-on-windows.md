# Installing x2rock on Windows

## Status (as of 2026-10-30)

**The Windows binary has only been built in CI and has not yet been run on physical Windows
hardware or Windows tablets.** Touch input, on-screen keyboard, volume controls, and network
prompt behaviour are implemented from Slint and Windows specifications, but await testing on
real devices. Hardware checks are tracked in [#7](https://github.com/rahga/x2rock/issues/7).

x2rock is built for Windows 10 and 11, including tablets, with touch, a keyboard or an Xbox
controller.

1. Download `x2rock-deck-<version>-windows-x86_64.zip` and `SHA256SUMS` from the
   [releases page](https://github.com/rahga/x2rock/releases). You can check its integrity
   in PowerShell:

   ```powershell
   Get-FileHash x2rock-deck-*-windows-x86_64.zip
   ```

   Unzip it anywhere.
2. Run `x2rock-deck.exe`.

**SmartScreen** may say "Windows protected your PC", since the build is not signed. Choose
**More info**, then **Run anyway**.

**Windows Firewall** may ask whether x2rock may use the network. Allow it on **private**
networks: it has to reach the speakers, and reading the household's music-service sign-ins
needs one speaker to connect back to it once.

The speakers are found on the local network. Your network has to be set to **Private** in
Windows (Settings, Network & internet, your connection, Network profile type) for them to
answer.

## Controls

| Xbox controller | Keyboard | Touch | Does |
|---|---|---|---|
| D-pad, left stick | Arrows | | Move |
| A | Enter | Tap | Select |
| A held | Enter held | Hold | Long press |
| B | Esc, Backspace | Tap outside a panel | Back |
| Y | Tab, M | Hold | A row's actions |
| X | Space | | Play or pause |
| LB / RB | `-` / `=` | − and + by the level | Volume |

On a tablet the − and + beside each level are always shown, and Search has its own keyboard
on the screen.

Settings are kept in `%APPDATA%\rahga\x2rock-deck\config\prefs.json`.

For music services that need an account, x2rock reads the household's own sign-ins off a
speaker, which needs the *speaker salt* (not part of x2rock's source): its 32 hex digits go in
`%APPDATA%\x2rock\config\household-salt`.
