#!/bin/sh
# Install x2rock for the Steam Deck (or any Linux desktop) from the unpacked
# release, for the current user only: no root, no package manager, nothing
# outside your home directory.
#
#   ./install.sh            install, add a menu entry, and add it to Steam
#   ./install.sh --remove   take it all out again
#
# The app goes in ~/Applications/x2rock, which SteamOS updates leave alone.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
dest="$HOME/Applications/x2rock"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
entry="$apps/x2rock-deck.desktop"

if [ "${1:-}" = "--remove" ]; then
    rm -rf "$dest" "$entry"
    echo "Removed x2rock. Its settings are in ~/.config/x2rock-deck if you want them gone too."
    echo "If you added it to Steam, remove it there: Library, right-click it, Manage, Remove non-Steam game."
    exit 0
fi

is_update=0
if [ "${1:-}" = "--update" ] || [ -f "$dest/x2rock-deck" ]; then
    is_update=1
fi

mkdir -p "$dest" "$apps"
install -m 755 "$here/x2rock-deck" "$dest/x2rock-deck"
install -m 644 "$here/x2rock-deck.png" "$dest/x2rock-deck.png"
sed -e "s|@BIN@|$dest/x2rock-deck|" -e "s|@ICON@|$dest/x2rock-deck.png|" "$here/x2rock-deck.desktop" > "$entry"
chmod 755 "$entry"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" 2>/dev/null || true

if [ "$is_update" = 1 ]; then
    echo "Updated x2rock in $dest. The existing Steam shortcut continues to launch the new version."
    exit 0
fi

echo "Installed x2rock in $dest, with a menu entry."

# On SteamOS, put it in the Steam library so Gaming Mode can open it.
if command -v steamos-add-to-steam >/dev/null 2>&1; then
    steamos-add-to-steam "$entry" && echo "Added to Steam: it is under Non-Steam in your library." && exit 0
fi
cat <<'MSG'
To open it from Steam's Gaming Mode: in Desktop Mode, open Steam, choose
Games > Add a Non-Steam Game to My Library, tick x2rock and press Add Selected
Programs. It is then under Non-Steam in your library.
MSG
