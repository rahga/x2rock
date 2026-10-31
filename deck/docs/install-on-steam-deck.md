# Installing x2rock on a Steam Deck

## Status (as of 2026-10-30)

x2rock-deck has been developed and verified on Linux desktop systems and tested read-only
against a single-room Sonos household (2026-10-30). **It has not yet been run on physical Steam
Deck hardware**, in either Gaming Mode or Desktop Mode. Steam shortcut integration, artwork
placement, gamepad repeat pacing, on-screen keyboard interaction (Steam + X), and touchscreen
support are implemented according to SteamOS specifications and unit tests, but await testing on a
physical Deck. Hardware checks are tracked in [#7](https://github.com/rahga/x2rock/issues/7).

x2rock is designed to run in both of the Deck's modes. In Desktop Mode it is an ordinary window;
in Gaming Mode it runs full screen as a non-Steam game, driven with the Deck's own controls.

## Install

1. Switch to **Desktop Mode** (Steam button, Power, Switch to Desktop).
2. Download `x2rock-deck-<version>-linux-x86_64.tar.gz` and `SHA256SUMS` from the
   [releases page](https://github.com/rahga/x2rock/releases). You can verify the archive:

   ```sh
   sha256sum -c SHA256SUMS --ignore-missing
   ```

   Unpack it (right-click it in Dolphin, Extract, Extract archive here).
3. Open the unpacked folder, right-click an empty space, **Open Terminal Here**, and run:

   ```sh
   ./install.sh
   ```

   It copies the app to `~/Applications/x2rock`, which SteamOS updates leave alone, adds a
   menu entry, and adds it to your Steam library where SteamOS can do that for it.

4. If the script says so, add it to Steam yourself: open Steam, **Games > Add a Non-Steam
   Game to My Library**, tick **x2rock**, **Add Selected Programs**.
5. Return to Gaming Mode. x2rock is under **Non-Steam** in your library. The first time it
   is opened from there, it is designed to give its tile Steam's artwork (capsule, hero and logo);
   a picture you set yourself is left alone.

To remove it: `~/Applications/x2rock` holds nothing else, and `./install.sh --remove` takes
the app and its menu entry out again.

## Updating

When an update is released:

1. Switch to **Desktop Mode**.
2. Download the new `x2rock-deck-<version>-linux-x86_64.tar.gz` and `SHA256SUMS`, verify,
   and unpack it.
3. Open the unpacked folder, **Open Terminal Here**, and run:

   ```sh
   ./install.sh
   ```

It replaces the app in `~/Applications/x2rock` and updates the menu entry. The existing Steam
shortcut, custom artwork, controller layout, and settings are preserved.

## Controls in Gaming Mode

Steam gives a non-Steam game a gamepad. In its controller settings (the controller icon on
x2rock's page in the library), pick the **Gamepad** template if it is not already chosen.

| Deck | Does |
|---|---|
| D-pad, left stick | Move |
| A | Select; held, a long press |
| B | Back |
| Y, Menu (≡) | A row's actions, the exact volume |
| X | Play or pause |
| L1 / R1 | Volume of the selected room |
| L2 / R2 | Previous / next track |
| View (⧉) | Full screen on or off |

The touchscreen is designed to work too: a tap is a press, and a finger held half a second is a
long press.

Search has a keyboard of its own on the screen. Steam's keyboard (Steam + X) is intended to type
into Search as well.

## Controls in Desktop Mode

Steam's desktop layout turns the Deck's controls into keys: the D-pad and left stick are the
arrows, A is Enter, B is Escape. x2rock is built to take those as it takes the gamepad.

## Network

The Deck has to be on the same network as the speakers. The first launch looks for them on
the local network and remembers them for next time; a network with more than one Sonos
system shows the larger one.

Music services that need no account work at once. For those that need one, x2rock reads the
sign-ins the household already holds, off a speaker, the first time it opens the services (or
from **Settings > Services > Read sign-ins from the speakers**). Reading them needs the
*speaker salt*, which is not part of x2rock's source and is not documented: if you have it,
put its 32 hex digits in `~/.config/x2rock/household-salt` (mode 0600), or give it to the
x2rock command line tool with `x2rock salt`. The two share that file and the sign-ins.

## If something goes wrong

Run it from a terminal to see what it says:

```sh
~/Applications/x2rock/x2rock-deck
```

`X2ROCK_DECK_DEBUG=1` adds how the screen was fitted. Reports are welcome at
[github.com/rahga/x2rock/issues](https://github.com/rahga/x2rock/issues); please
don't post your email address there.
