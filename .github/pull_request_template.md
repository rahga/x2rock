<!--
Thank you for your contribution!
Before submitting, please ensure your changes follow the guidelines in CONTRIBUTING.md.
x2rock prioritizes demonstrated user value and maintainability over cosmetic churn or line counts.
Never include credentials, passwords, or private account tokens in PRs or tests.
-->

### User problem and motivation
<!-- What concrete user-visible problem or defect does this solve? Why does it matter? -->

### Proposed changes and scope
<!-- What does this PR change? Keep changes bounded to the problem stated above. -->

### Reproduction and verification evidence
<!--
How was the problem reproduced and verified?
- For bug fixes: what reproduction was observed before the fix, and what regression check proves the fix?
- For documentation: what inaccuracy or missing task was corrected?
- Synthetic tests must be labelled synthetic; hardware observations should note dated device/model.
-->

### Checks performed
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `python3 scripts/test-follow-sink.py`
- [ ] Documentation updated if CLI/JSON surface or error codes changed (`README.md`, `skills/x2rock/SKILL.md`)
