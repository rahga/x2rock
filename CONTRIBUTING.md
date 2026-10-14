# Contributing

x2rock is a Linux Rust binary containing a CLI, an MPRIS daemon and a terminal UI.
You can build it and run the default test suite without Sonos hardware or an account.
The [README](README.md) documents current behavior; [architecture notes](docs/architecture.md)
record dated findings and decisions. Check their superseded-claims index before relying on an
older entry. The [custom D-Bus interface](docs/dbus-interface.md) is a proposal, not an implemented API.

Agents should also read [AGENTS.md](AGENTS.md). The maintainer's laptop has an ignored
`.local-work/` queue for assignments, reviews and handoffs; contributing from another clone does
not require that private queue.

## What makes a useful contribution

Start with a concrete problem for someone using x2rock. A useful patch explains the trigger,
current behavior, intended behavior, and evidence that the change helps. Small fixes are welcome
when they resolve a real problem; patch size and passing CI alone do not establish value.

For a bug fix, include a reproducer and, where practical, a regression that fails without the fix.
Protocol claims should identify captured evidence or clearly labeled synthetic tests. A documentation
change should correct a factual error or identify a task the existing instructions fail to explain.
For a feature, dependency replacement, broad refactor or changed public behavior, discuss the user
need and tradeoffs before investing in a large patch.

We generally decline cosmetic-only code churn, speculative abstractions, generic "best practice"
rewrites, bulk generated documentation/tests, and dependency updates with no stated reason.
Security and compatibility fixes have clear reasons; explain them. Tests must check meaningful
behavior, not merely restate the implementation or assert that new wording exists.

Using AI tools is not a disqualification. Contributors remain responsible for understanding and
verifying every submitted change. Review is based on evidence, correctness, scope and maintenance
cost. Maintainers may decline a technically valid patch whose benefit does not justify that cost.
Splitting such a patch into many small pull requests does not change that standard.

You do not need a patch or an automated test to report a bug. Give the version, steps, expected
and observed behavior, and available sanitized diagnostics. A clear report of a real failure is
more useful than a speculative fix. Never include credentials or account tokens.

## Triage and review

Every issue and pull request gets one of five dispositions:

- **Actionable**: a clear, bounded report of a real problem, or a change with demonstrated value,
  reproducible evidence and appropriate checks.
- **Needs specific evidence**: something needed to assess or reproduce it is missing. The reply
  names the one missing item, such as `x2rock --version`, the exact command, the error JSON or
  `x2rock system --json --redact`. It does not start open-ended debugging or rewrite coaching.
- **Duplicate**: closed with a link to the issue or pull request already tracking it.
- **Out of scope**: a documented non-goal (S1, control from outside the LAN, jobs only the Sonos
  app does), or a feature with no clear maintenance case.
- **Maintenance cost exceeds benefit**: speculative abstractions, cosmetic or style-only churn,
  dependency updates with no stated reason, or additions whose upkeep outweighs the user benefit.

A genuine bug is not turned away because its reporter cannot find the cause or write a test.
What happened, what was expected, and output from existing read-only commands are enough.

**Check diagnostics before pasting them.** `x2rock status` (text) and `x2rock system --json
--redact` are made for sharing. `status --json` and `now --json` are not redacted: their
`art_url` holds LAN addresses and, while Plex is playing, the household's Plex token. Remove
`art_url` before pasting either. Default daemon logs (`journalctl --user -u x2rock`) are fine;
logs captured with `X2ROCK_LOG_EVENTS` or `X2ROCK_DUMP_SMAPI` contain raw event and service
bodies, so do not paste them unedited. Never include credentials, tokens or passwords.

## Build and check

Use Rust 1.89 or newer and a C compiler for `ring`. No CMake or Sonos SDK is required.
Keep `Cargo.lock` in use so local checks build the same dependency set as CI:

```sh
cargo build --locked
cargo run --locked -- --help
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
python3 scripts/test-follow-sink.py
```

These are the checks in [CI](.github/workflows/ci.yml). With dependencies already cached,
add `--offline` to build, Clippy or test to avoid registry access. To check the minimum Rust
version locally with rustup:

```sh
rustup toolchain install 1.89 --profile minimal
cargo +1.89 check --all-targets --locked
```

Tests live beside the implementation in `#[cfg(test)]` modules. Run a subset by name, for example
`cargo test --locked sonos::http::tests`. File-writing tests use temporary directories; transport
tests use local sockets. A sandbox that forbids TCP listeners will fail the clip server and
WebSocket tests with `Operation not permitted`; run them in an environment that permits local
listeners. They do not need a real speaker.

The volume bridge tests need Python 3 and Bash. They run `scripts/follow-sink.sh` with mocked
`pactl` and `x2rock` commands, checking mute, volume caps and retry behavior without an audio server.

One test is ignored by default because it reads the machine's real default route. On a machine
with a default route, run it explicitly:

```sh
cargo test --locked netid::tests::finds_this_machines_gateway -- --ignored
```

## Where changes belong

| Area | Source |
|---|---|
| CLI arguments and dispatch | `src/cli.rs`, `src/main.rs` |
| Commands and shared outcomes/printing | `src/commands/` |
| Room resolution and reusable connections | `src/session.rs` |
| Stable error codes and recovery hints | `src/hint.rs` |
| LAN WebSocket and UPnP transports | `src/sonos/local.rs`, `src/sonos/api.rs`, `src/sonos/upnp.rs` |
| Service search, browse, linking and optional Sonos cloud content | `src/commands/services.rs`, `src/sonos/smapi.rs`, `src/sonos/cloud.rs`, `src/sonos/login.rs` |
| Daemon lifecycle and MPRIS publication | `src/daemon.rs`, `src/mpris.rs` |
| Discovery and remembered networks | `src/discover.rs`, `src/netid.rs`, `src/state.rs` |
| Persistent files and account/bookmark transactions | `src/store.rs`, `src/credentials.rs`, `src/bookmarks.rs` |
| Terminal UI | `src/tui/` |
| Bar widget | `quickshell/x2rock.sonos/` |
| Service installation and desktop integration | `src/service.rs`, `systemd/`, `desktop/` |
| Embedded agent documentation | `skills/x2rock/SKILL.md` |

When changing a command, check its plain-text output, `--json` output and error behavior. Update
the README and embedded skill if the documented surface changes. Tests check the skill's named
flags, error codes and status fields against the implementation; the skill is compiled into the
binary, so installed copies only change when reinstalled with a new build.

Use existing typed command outcomes where applicable, and keep service search, browse and speech
requests in the CLI. The daemon's internet exception is refreshing an explicitly configured Sonos
sign-in. New protocol behavior should be backed by a fixture or a dated hardware observation,
with the tested model and firmware recorded when relevant.

## Checking against hardware

`cargo run --locked -- <command>` runs the checkout's binary, but uses your normal x2rock state
and addresses real speakers when the command calls for them. Start with read-only commands such
as `status --json` and `system --json --redact`. Playback, volume, grouping and settings commands
affect everyone using those rooms.

[`scripts/output-baseline.sh`](scripts/output-baseline.sh) is an opt-in hardware regression tool,
not part of the default suite. It invokes the installed `x2rock` from `PATH` and changes volume,
mute, play modes and grouping in two rooms (`K` and `R`, defaulting to Kitchen and Dining Room).
It restores the recorded group volumes but leaves the rooms ungrouped, repeat/shuffle/crossfade
off, and `R` unmuted; it does not restore the full original state. Use it only with rooms prepared
for those changes, and inspect the script before capture.

For a bug report, include `x2rock --version`, the command, expected and actual behavior, and the
relevant error or daemon log. The version includes the build's Git revision. Hardware reports
benefit from `x2rock system --json --redact`; keep account tokens and unredacted credentials out
of reports and fixtures.
