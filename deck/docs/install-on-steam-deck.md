# Installing x2rock on a Steam Deck

x2rock runs in both of the Deck's modes. In Desktop Mode it is an ordinary window; in Gaming
Mode it is a non-Steam game, full screen, driven with the Deck's own controls.

## Install

1. Switch to **Desktop Mode** (Steam button, Power, Switch to Desktop).
2. Download `x2rock-deck-<version>-linux-x86_64.tar.gz` from the
   [releases page](https://github.com/rahga/x2rockdeck/releases) and unpack it (right-click
   it in Dolphin, Extract, Extract archive here).
3. Open the unpacked folder, right-click an empty space, **Open Terminal Here**, and run:

   ```sh
   ./install.sh
   ```

   It copies the app to `~/Applications/x2rock`, which SteamOS updates leave alone, adds a
   menu entry, and adds it to your Steam library where SteamOS can do that for it.

4. If the script says so, add it to Steam yourself: open Steam, **Games > Add a Non-Steam
   Game to My Library**, tick **x2rock**, **Add Selected Programs**.
5. Return to Gaming Mode. x2rock is under **Non-Steam** in your library. The first time it
   is opened from there it gives its tile Steam's artwork (capsule, hero and logo); a picture
   you set yourself is left alone.

To remove it: `~/Applications/x2rock` holds nothing else, and `./install.sh --remove` takes
the app and its menu entry out again.

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

The touchscreen works too: a tap is a press, and a finger held half a second is a long press.

Search has a keyboard of its own on the screen. Steam's keyboard (Steam + X) types into
Search as well.

## Controls in Desktop Mode

Steam's desktop layout turns the Deck's controls into keys: the D-pad and left stick are the
arrows, A is Enter, B is Escape. x2rock takes those as it takes the gamepad.

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
[github.com/rahga/x2rockdeck/issues](https://github.com/rahga/x2rockdeck/issues); please
don't post your email address there.
