"""Exercise the real bridge with fake pactl/x2rock; no audio server or speakers."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


BRIDGE = Path(__file__).with_name("follow-sink.sh")
MOCK = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

path = Path(os.environ["BRIDGE_FIXTURE"])
data = json.loads(path.read_text())
if Path(sys.argv[0]).name == "pactl":
    command = sys.argv[1]
    if command == "subscribe":
        for _ in data["steps"][1:]:
            print("Event 'change' on sink #1")
        sys.exit(0)
    if command == "get-sink-volume":
        data["step"] += 1
        path.write_text(json.dumps(data))
        level = data["steps"][data["step"]]["level"]
        print(f"Volume: front-left: 12345 / {level}% / -10.00 dB")
    else:
        muted = data["steps"][data["step"]]["muted"]
        if muted is None:
            sys.exit(1)
        print(f"Mute: {muted}")
else:
    assert sys.argv[1:4] == ["-r", "Test Room", "vol"], sys.argv
    step = data["steps"][data["step"]]
    action = sys.argv[4]
    ok = action not in step.get("fail", [])
    if ok:
        if action in ("mute", "unmute"):
            data["speaker"]["muted"] = action == "mute"
        else:
            data["speaker"] = {"level": int(action), "muted": False}
    data["calls"].append({"step": data["step"], "action": action, "ok": ok,
                          "speaker": data["speaker"].copy()})
    path.write_text(json.dumps(data))
    sys.exit(0 if ok else 1)
'''


class FollowSinkTests(unittest.TestCase):
    def run_bridge(self, steps, cap="50"):
        with tempfile.TemporaryDirectory(prefix="x2rock-bridge-") as directory:
            root = Path(directory)
            fixture = root / "state.json"
            fixture.write_text(json.dumps({
                "steps": steps, "step": -1, "calls": [],
                "speaker": {"level": 10, "muted": False},
            }))
            for command in ("pactl", "x2rock"):
                executable = root / command
                executable.write_text(MOCK)
                executable.chmod(0o755)
            result = subprocess.run(
                ["bash", str(BRIDGE), "Test Room", cap],
                env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                     "X2ROCK": str(root / "x2rock"), "BRIDGE_FIXTURE": str(fixture)},
                capture_output=True, text=True, timeout=10,
            )
            return result, json.loads(fixture.read_text())

    def test_volume_changes_stay_silent_until_unmute(self):
        result, data = self.run_bridge([
            {"level": 40, "muted": "yes"},
            {"level": 80, "muted": "yes"},
            {"level": 80, "muted": "no"},
        ])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(all(call["speaker"]["muted"] for call in data["calls"]
                            if call["step"] < 2), data["calls"])
        self.assertEqual(data["speaker"], {"level": 40, "muted": False})

    def test_cap_rounding_and_duplicate_events(self):
        result, data = self.run_bridge([
            {"level": level, "muted": "no"} for level in (51, 51, 150, 150)
        ], "030")
        self.assertEqual(result.returncode, 0, result.stderr)
        levels = [c["action"] for c in data["calls"] if c["action"].isdigit()]
        self.assertEqual(levels, ["15", "30"])
        self.assertEqual(data["speaker"], {"level": 30, "muted": False})

    def test_failed_volume_is_retried_on_the_next_event(self):
        _, data = self.run_bridge([
            {"level": 80, "muted": "no", "fail": ["40"]},
            {"level": 80, "muted": "no"},
        ])
        self.assertEqual(data["speaker"], {"level": 40, "muted": False})

    def test_failed_mute_is_retried_on_the_next_event(self):
        _, data = self.run_bridge([
            {"level": 80, "muted": "yes", "fail": ["mute"]},
            {"level": 80, "muted": "yes"},
        ])
        self.assertTrue(data["speaker"]["muted"], data["calls"])

    def test_failed_unmute_is_retried_without_changing_volume(self):
        _, data = self.run_bridge([
            {"level": 80, "muted": "no"},
            {"level": 80, "muted": "yes"},
            {"level": 80, "muted": "no", "fail": ["unmute"]},
            {"level": 80, "muted": "no"},
        ])
        self.assertEqual(data["speaker"], {"level": 40, "muted": False})
        self.assertEqual([c["action"] for c in data["calls"]],
                         ["40", "mute", "unmute", "unmute"])

    def test_zero_cap_and_silent_sink(self):
        for cap, level in (("0", 100), ("50", 0)):
            with self.subTest(cap=cap, level=level):
                result, data = self.run_bridge([{"level": level, "muted": "no"}], cap)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(data["speaker"]["level"], 0)

    def test_unreadable_mute_does_not_change_speaker(self):
        for muted in (None, "unknown"):
            with self.subTest(muted=muted):
                _, data = self.run_bridge([{"level": 80, "muted": muted}])
                self.assertEqual(data["calls"], [])

    def test_invalid_cap_is_rejected_before_any_commands(self):
        for cap in ("-1", "101", "loud", "1+1", "99999999999999999999"):
            with self.subTest(cap=cap):
                result, data = self.run_bridge([{"level": 80, "muted": "no"}], cap)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertEqual(data["calls"], [])


if __name__ == "__main__":
    unittest.main()
