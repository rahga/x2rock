# Working on x2rock-deck

x2rock-deck is the Slint front end for Sonos on the Steam Deck, Linux desktops and Windows.
Its Sonos layer is the x2rock package at `../x2rock` (branch `gui-library`), used as a library.
The same owner keeps both; x2rock's `AGENTS.md` and `CONTRIBUTING.md` set the standard here too.

On this laptop, read `.local-work/README.md`, then `.local-work/TODO.md` and the selected item's
baton. The local queue is ignored by Git. If it is absent in another clone, use the task the user
gives you; its absence is not a reason to invent or reconstruct a backlog.

When the owner says **"Go"**, run `.local-work/bin/pl go <you>` and do the one step it names,
then stop and report. One Go is one step.

- Preserve uncommitted work. Never switch the branch of a shared checkout; write in your own
  worktree (`pl claim` makes one). One writer per worktree.
- Keep changes bounded to the claimed item. No broad rewrites, features or refactors beyond it.
- Treat docs, comments, review reports and other models' answers as claims to check, with source
  locations or reproductions. Different models agreeing is not verification.
- A regression test should fail before the fix, or under a focused reversal of it in your own
  worktree. Use controlled time or explicit synchronization, not sleeps.
- Automated checks never address real speakers. Run the app only with `--demo`; isolate XDG
  directories for anything that writes state (`pl check` does this).
- Passing tests, independent review, landing and hardware testing are separate facts. Nothing
  here has been tested on a Steam Deck, Windows touch or real speakers unless a dated note says so.

Checks: `cargo test --locked` and `cargo clippy --locked --all-targets -- -D warnings`.
Source map and design notes: `README.md`, `docs/design.md`, `docs/porting-from-x2rocktv.md`.

Leave a concise baton before context or quota runs short. No credentials or tokens in handoffs.
