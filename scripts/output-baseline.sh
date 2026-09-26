#!/bin/bash
# Capture what the installed x2rock prints for a fixed list of commands, so a
# refactor of the command layer can be checked byte-for-byte: capture before,
# install the new build, capture after, diff.
#
#   scripts/output-baseline.sh capture <dir>     one file trio per command:
#                                                <dir>/<name>.{out,err,rc}
#   scripts/output-baseline.sh diff <before> <after>
#
# It runs against real speakers. It touches exactly two rooms - $K and $R,
# Kitchen and Dining Room unless overridden in the environment - and puts
# them back: volumes to what they were, crossfade/shuffle/repeat off,
# ungrouped. It reads every room once (`--all vol`) and writes to no other.
# The captured commands set fixed levels (9), so two runs compare; the
# restore at the end is not captured, since it varies with the house. There is deliberately
# no `party` here: that captures every room in the house, and did, three
# times, before it was taken out. Nor `tv`: the only soundbars are someone's.
#
# The three refactors of 2026-09-25/26 (outcomes, the session pool, the
# grouping/TV outcomes) each diffed clean against this list.
set -u
K=${K:-Kitchen}
R=${R:-"Dining Room"}

level() { x2rock -r "$1" vol --json 2>/dev/null | sed 's/.*"volume":\([0-9]*\).*/\1/'; }

capture() {
    D=$1
    mkdir -p "$D"
    was_k=$(level "$K"); was_r=$(level "$R")
    cap() { n=$1; shift; "$@" >"$D/$n.out" 2>"$D/$n.err"; echo $? >"$D/$n.rc"; }
    cap vol_read            x2rock -r "$K" vol
    cap vol_read_json       x2rock -r "$K" vol --json
    cap vol_plus5_json      x2rock -r "$K" vol +5 --json
    cap vol_minus5          x2rock -r "$K" vol -5
    cap vol_set_json        x2rock -r "$K" vol 9 --json
    cap mute                x2rock -r "$R" vol mute
    cap ramp_muted          x2rock -r "$R" vol 12 --ramp
    cap unmute_json         x2rock -r "$R" vol unmute --json
    cap vol_restore_r       x2rock -r "$R" vol 9
    cap fanout_two          x2rock -r "$K" -r "$R" vol 9
    cap fanout_two_json     x2rock -r "$K" -r "$R" vol 9 --json
    cap fanout_fail         x2rock -r "$K" -r "Nowhere" vol 9
    cap all_vol             x2rock --all vol
    cap all_vol_json        x2rock --all vol --json
    cap each_json           x2rock -r "$K" vol --each --json
    cap each                x2rock -r "$K" vol --each
    cap player              x2rock -r "$K" vol --player
    cap crossfade_read      x2rock -r "$K" crossfade
    cap crossfade_on_json   x2rock -r "$K" crossfade on --json
    cap crossfade_off       x2rock -r "$K" crossfade off
    cap repeat_read_json    x2rock -r "$K" repeat --json
    cap shuffle_read        x2rock -r "$K" shuffle
    cap shuffle_off_json    x2rock -r "$K" shuffle off --json
    cap repeat_off          x2rock -r "$K" repeat off
    cap group               x2rock -r "$K" group "$R"
    cap group_json          x2rock -r "$K" group "$R" --json
    cap normalize           x2rock -r "$K" vol normalize
    cap normalize_json      x2rock -r "$K" vol normalize --json
    cap player_member       x2rock -r "$R" vol --player
    cap ungroup             x2rock ungroup "$R"
    cap ungroup_json        x2rock ungroup "$R" --json
    # Back to where the house had them - not captured, see above.
    [ -n "$was_k" ] && x2rock -r "$K" vol "$was_k" >/dev/null
    [ -n "$was_r" ] && x2rock -r "$R" vol "$was_r" >/dev/null
    echo "captured $(ls "$D" | wc -l) files in $D; $K back to ${was_k:-?}, $R to ${was_r:-?}"
}

case ${1:-} in
    capture) capture "$2" ;;
    diff) diff -r "$2" "$3" && echo "byte-identical across $(ls "$2" | wc -l) files" ;;
    *) sed -n 2,17p "$0"; exit 2 ;;
esac
