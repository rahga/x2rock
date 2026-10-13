#!/bin/bash
# Make a Sonos room follow this computer's volume keys.
#
#   scripts/follow-sink.sh [room] [max]
#
# Whenever the default output's level or mute changes - the keyboard's volume
# keys, the bar's slider, anything that moves the sink - the room is set to
# match, with the sink's 100% landing on `max` on the speaker (50 unless
# given). Mute follows too. One direction only: the speaker does not move the
# computer. The room defaults to $X2ROCK_ROOM, as every x2rock command does.
#
# This is the whole of what a "volume bridge" app does, written as a consumer
# of the CLI rather than a feature of it - see "The computer's volume keys,
# driving a room" in docs/architecture.md for why it lives here and not in the
# daemon. It talks the PulseAudio protocol through `pactl`, which PipeWire
# serves via pipewire-pulse on every current desktop, so it needs nothing
# beyond what Omarchy and Ubuntu already ship.
#
# `pactl subscribe` says several things per keypress - the client that asked,
# the sink, the card - so every event just re-reads the sink and nothing is
# sent unless the level or the mute actually changed. Set X2ROCK=echo to watch
# what it would send without touching a speaker.
set -uo pipefail
# pactl's labels and event descriptions are parsed below.
export LC_ALL=C

ROOM=${1:-${X2ROCK_ROOM:-}}
MAX=${2:-50}
X2ROCK=${X2ROCK:-x2rock}
[ -n "$ROOM" ] || { echo "usage: $0 <room> [max]   (or export X2ROCK_ROOM)" >&2; exit 2; }
if [[ ! "$MAX" =~ ^[0-9]{1,3}$ ]] || (( 10#$MAX > 100 )); then
    echo "$0: max must be a whole number from 0 to 100" >&2
    exit 2
fi
MAX=$((10#$MAX))
command -v pactl >/dev/null || { echo "$0: pactl not found - PipeWire's pipewire-pulse or PulseAudio is needed" >&2; exit 2; }

sink_level() {
    # "Volume: front-left: 25897 /  40% / -24.19 dB, ..." -> 40. First channel;
    # a stereo sink the keys move stays balanced anyway.
    pactl get-sink-volume @DEFAULT_SINK@ | sed -n '1s/[^/]*\/ *\([0-9]*\)%.*/\1/p'
}
sink_muted() { pactl get-sink-mute @DEFAULT_SINK@ | sed 's/^Mute: //'; }

last_level=; last_muted=
apply() {
    local level muted target
    level=$(sink_level) || return
    muted=$(sink_muted) || return
    [[ "$level" =~ ^[0-9]+$ ]] || return
    case $muted in yes|no) ;; *) return ;; esac

    # Setting Sonos volume unmutes it. While the sink is muted, defer its
    # level until unmute rather than briefly making the room audible.
    if [ "$muted" = yes ]; then
        if [ "$last_muted" != yes ]; then
            "$X2ROCK" -r "$ROOM" vol mute || return
            last_muted=yes
        fi
        return
    fi
    # Over 100% (PipeWire allows it) still caps at max.
    [ "$level" -gt 100 ] && level=100
    target=$(( (10#$level * MAX + 50) / 100 ))
    if [ "$target" != "$last_level" ]; then
        # Remember only successful writes, so the next event retries failures.
        "$X2ROCK" -r "$ROOM" vol "$target" || return
        last_level=$target
        last_muted=no
    elif [ "$last_muted" != no ]; then
        "$X2ROCK" -r "$ROOM" vol unmute || return
        last_muted=no
    fi
}

echo "following the default sink into $ROOM: sink 100% is speaker $MAX" >&2
apply
# 'server' covers the default sink changing under us; 'sink' covers the level.
pactl subscribe | while read -r line; do
    case $line in
        *"on sink #"*|*"on server"*) apply ;;
    esac
done
