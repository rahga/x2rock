---
name: Bug report
about: Report a reproducible problem or unexpected failure in x2rock
title: ""
labels: ["bug"]
---

<!--
Thank you for reporting an issue! Please keep reports concise and factual.
You do not need a patch or automated test to report a bug.
IMPORTANT: Never include credentials, passwords, or unredacted account tokens.
-->

### Version and environment
- `x2rock --version` output (includes Git revision):
- Linux distribution / desktop environment:
- Sonos speaker model(s) involved (if applicable):

### What happened
- Command line or action executed:
- Observed behavior (include exact error message or `{code, error, fix}` JSON from stderr):
- Expected behavior:

### Steps to reproduce
1.
2.

### Diagnostics (optional but helpful)
<!--
Safe to paste as they are:
- `x2rock status` (text)
- `x2rock system --json --redact` (for speaker or hardware problems; masks serials and addresses)
- `journalctl --user -u x2rock --since "1 hour ago"` (default logging only)

If you paste `status --json` or `now --json`, delete `art_url` first: it holds LAN addresses and,
while Plex is playing, your household's Plex token. Do not paste logs captured with
X2ROCK_LOG_EVENTS or X2ROCK_DUMP_SMAPI unedited.
-->
