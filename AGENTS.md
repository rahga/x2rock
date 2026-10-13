# Working on x2rock

The product already meets the owner's main goals. Current work is about making it useful to
other people: understandable installation, honest capability claims, reliable operation,
actionable diagnostics, and contributions that do not require the maintainer's memory.
The owner wants a high contribution standard, not an easy-commit backlog: prioritize demonstrated
user value and cheap, evidence-based rejection of low-value churn. Keep the private punch list local.

Read `CONTRIBUTING.md` for the source map and checks. On this laptop, read
`.local-work/README.md`, then `.local-work/TODO.md` and the selected item's baton. The local
queue is intentionally ignored by Git. If it is absent in another clone, use the task supplied
by the user; its absence is not a reason to invent or reconstruct a private backlog.

- Preserve existing uncommitted work. Dirty handoffs are allowed; record their exact state.
- Default to one writer per checkout. Independent reviewers can work alongside that writer
  against a frozen snapshot or separate worktree. Do not switch a shared checkout's branch.
- Keep changes bounded to the selected item. Do not start a broad rewrite, add features, or
  refactor solely to meet a line-count target.
- Treat historical docs, comments, review reports and model answers as claims to check.
  Preserve the owner's constraints; challenge technical assertions with source locations,
  reproductions or dated hardware evidence. Different models agreeing is not verification.
- Distinguish hypotheses, source-traced behavior, reproduced bugs and hardware observations.
  Label synthetic fixtures as synthetic; never represent them as captured speaker responses.
- Match checks to the change. A behavioral regression should fail before the fix, or under a
  focused reversal of it in an isolated copy. Never mutate the user's working tree to prove this.
- `cargo run` uses real user state unless isolated. Use temporary files, fake endpoints and
  isolated XDG directories for tests. Read the local hardware context before live writes;
  honor authorization already given in the current session, without importing permissions
  from x2rocktv or assuming the same rooms are free today.
- A passing test run, independent review, commit, installation and publication are separate
  facts. Record what actually happened. Do not label a change reviewed because its author
  reread it, or shipped because it builds.

Leave a concise baton before context or quota runs short. No credentials or raw account tokens
belong in handoffs, fixtures, reports or commits.
