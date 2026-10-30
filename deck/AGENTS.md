# Working on x2rock-deck

x2rock-deck is the Slint front end for Sonos on the Steam Deck, Linux desktops and Windows, the
`deck/` member of the x2rock workspace. Its Sonos layer is the x2rock package at the repository
root, used as a library (`default-features = false`). The root's `AGENTS.md` and
`CONTRIBUTING.md` set the standard here too.

Where the owner keeps a local work queue (`.local-work/`, ignored by Git), read its `README.md`
and follow it. Where there is none, use the task the user gives you; its absence is not a reason
to invent or reconstruct a backlog.

- Preserve uncommitted work. Never switch the branch of a shared checkout; write in your own
  worktree. One writer per worktree.
- Keep changes bounded to the claimed item. No broad rewrites, features or refactors beyond it.
- Treat docs, comments, review reports and other models' answers as claims to check, with source
  locations or reproductions. Different models agreeing is not verification.
- A regression test should fail before the fix, or under a focused reversal of it in your own
  worktree. Use controlled time or explicit synchronization, not sleeps.
- Automated checks never address real speakers. Run the app only with `--demo`; isolate XDG
  directories (`XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`) for
  anything that writes state.
- Passing tests, independent review, landing and hardware testing are separate facts. Nothing
  here has been tested on a Steam Deck, Windows touch or real speakers unless a dated note says so.

Checks, from the repository root: `cargo test --locked -p x2rock-deck`,
`cargo clippy --locked --all-targets -p x2rock-deck -- -D warnings` and `cargo fmt --check`.
Source map and design notes: `README.md`, `docs/design.md`, `docs/porting-from-x2rocktv.md`.

Leave a concise baton before context or quota runs short. No credentials or tokens in handoffs.
