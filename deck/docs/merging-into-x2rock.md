# Moving x2rock-deck into the x2rock repository

The minimal steps to make this package a Cargo workspace member of `rahga/x2rock`. **Nothing here
has been merged.** Each step was checked on 2026-10-30, against deck `ae1ad6b` and x2rock
`gui-library` `877ef86`, in a trial workspace built from `git archive` exports of both. That trial
was a scratch copy, not either repository. "Checked" below means a command was run in it and its
output is quoted. "Not checked" says why.

## 1. Where things go

The package moves to **`deck/`** at the root of x2rock. That's short, and nothing in x2rock uses
the name. x2rock's root manifest stays a package and gains the workspace:

```toml
# x2rock/Cargo.toml, at the top
[workspace]
members = [".", "deck"]
default-members = ["."]          # see section 3
resolver = "3"
```

```toml
# deck/Cargo.toml
x2rock = { path = "..", default-features = false }
repository = "https://github.com/rahga/x2rock"
```

What moves with it, and whether its paths change:

| Path | After the move | Change needed |
|---|---|---|
| `build.rs` | `deck/build.rs` | None. Cargo runs it from the package directory, and it names `ui/app.slint` and `dist/x2rock-deck.rc` relative to that |
| `ui/`, `dist/` | `deck/ui/`, `deck/dist/` | None. `include_bytes!("../dist/steam/…")` (`src/steam.rs:11-14`) and `include_str!("../ui/assets/service-logos.json")` (`src/logos.rs:8`) are relative to `src/` |
| `docs/` | `deck/docs/` | Keep them apart from x2rock's `docs/`. README links are relative, so they still work |
| `LICENSE` | `deck/LICENSE` | See section 2 |
| `.github/workflows/build.yml` | folded into x2rock's (section 5) | |
| `AGENTS.md`, `CLAUDE.md`, `GEMINI.md` | `deck/` | Their `../x2rock` and `gui-library` lines go |

**Checked:** with the two manifest edits above, `cargo metadata` resolves the workspace (exit
0). It warns `profiles for the non root package will be ignored` (section 3).

## 2. Licences

x2rock is **0BSD**. This package is **GPL-3.0-only**, because it takes Slint under Slint's GPLv3
option.

- Keep both per-package `license` fields as they are (`0BSD` in the root manifest,
  `GPL-3.0-only` in `deck/Cargo.toml`). Keep `deck/LICENSE` with the GPL text beside the root's
  0BSD `LICENSE`.
- The repository then holds two licences, one per package. The x2rock library and CLI stay 0BSD:
  nothing in them depends on the deck. The deck's binary is a GPL-3.0 work that includes the
  0BSD library, which 0BSD permits.
- Say so in x2rock's README: one line under its licence heading naming `deck/` and its licence.
  GitHub's licence detection reads only the root `LICENSE`, so without that line the repository
  page says 0BSD for everything.
- The deck's README licence paragraph (Slint under GPLv3, Roboto and Material icons under
  Apache 2.0) moves with it unchanged.

## 3. Cargo

**One `Cargo.lock`.** The two locks share 219 crates, and 63 of them are pinned differently.
Almost all are patch drift, and none are semver-incompatible within one major version. Resolving
the workspace from x2rock's lock (checked) keeps x2rock's pins, except 7 target-only crates
(`hermit-abi`, `js-sys`, `libredox`, the `wasm-bindgen` family) that Linux and Windows builds
never compile. It moves **46 of the deck's pins to older patch versions**, among them:

| Crate | Deck today | Workspace lock |
|---|---|---|
| tokio | 1.54.1 | 1.53.1 |
| serde_json | 1.0.154 | 1.0.151 |
| tokio-rustls | 0.26.6 | 0.26.5 |
| roxmltree | 0.21.2 | 0.21.1 |
| libc | 0.2.191 | 0.2.189 |
| rustix | 1.1.6 | 1.1.4 |
| syn | 3.0.9 | 3.0.5 |
| uuid | 1.29.0 | 1.26.0 |

So the first workspace commit puts the deck on a tree it hasn't been tested with. Run both test
suites on it (section 6). Then `cargo update` the workspace in a commit of its own, if newer pins
are wanted.

**`rust-version` and edition** are 1.89 and 2024 in both. They can move to `[workspace.package]`
and be inherited, but that's optional. **Resolver:** `"3"`, the 2024 edition's default, stated
explicitly because a workspace root should state it.

**Feature unification (checked).** `cargo tree -e features -i x2rock`:

- `-p x2rock-deck` builds x2rock without `cli`. `cargo tree -p x2rock-deck -i clap` finds no
  clap on Linux or on `x86_64-pc-windows-msvc`. The only zbus is Slint's own, through
  `i-slint-backend-winit`, which the deck has today.
- `--workspace` turns `cli` on: `x2rock feature "cli" └── x2rock feature "default"`. The deck
  then links an x2rock built with clap, crossterm, ratatui, mpris-server and zbus.

So set **`default-members = ["."]`**, and always build the deck with `-p x2rock-deck`. A plain
`cargo build`/`test` at the root then means x2rock, as it does today. *Not checked:* that
`--workspace` actually fails to build for Windows. A Windows check needs a C toolchain for
`ring` (the deck's CI gets it from the Windows runner, and cargo-xwin isn't installed here). The
Windows CI job is the test.

**Profiles: a real conflict.** Only the root's `[profile.*]` counts in a workspace, and x2rock's
release profile has `opt-level = "s"` and **`panic = "abort"`**. Cargo doesn't allow `panic` per
package. The deck relies on unwinding:

- a command job that panics frees its lane (`lanes::tests::a_job_that_panics_frees_its_lane`);
- the art loader's slot is freed through a `JoinError` (`src/art.rs`).

Under `abort`, either becomes a crash of the whole app. Give the deck a custom profile in the root
manifest, built with `--profile release-deck` (output in `target/release-deck/`):

```toml
[profile.release-deck]
inherits = "release"
opt-level = 3
panic = "unwind"
```

*Not checked* by a build. `cargo metadata` accepts it, but nothing was compiled with it.

## 4. Formatting

x2rock's CI runs `cargo fmt --check` with stock rustfmt (`ci.yml`, "Stock rustfmt, no
rustfmt.toml"). In the trial workspace it covers the deck too, and **fails**:

- **The deck:** 642 hunks across 22 files. Its code is hand-wrapped at a long width and was
  never run through rustfmt.
- **x2rock's own `gui-library` branch:** 29 hunks in 7 files, 20 of them in `src/play.rs`. So
  that branch already fails its `main` CI before the deck arrives.

Decide this first. Either run `cargo fmt` on both as their own commits before the move (it
reformats most of the deck), or exclude `deck/` from the check (`cargo fmt -p x2rock --check`).
The second keeps the deck's style, and the deck's own CI never ran rustfmt.

## 5. CI

Fold `.github/workflows/build.yml` into x2rock's `ci.yml` as separate jobs. Keep everything the
deck's workflow does:

- **`deck-linux`** on **ubuntu-22.04**, not latest: the binary must need a glibc no newer than
  SteamOS's.
  - Install `libfontconfig1-dev libxkbcommon-dev libwayland-dev libudev-dev libgl1-mesa-dev`.
  - `cargo test --locked -p x2rock-deck`, then `cargo build --release --locked -p x2rock-deck`
    (or `--profile release-deck`).
  - The six demo screenshots, uploaded as an artifact.
  - The `.tar.gz` packaging with `install.sh`, the desktop file, the icon, `LICENSE` and the
    install guide.
- **`deck-windows`:** the same, packaged as a `.zip`.
- **Release:** today `build.yml` publishes on any `v*` tag, and **x2rock already uses `v*` for
  the CLI** (`v0.2.0`). Give the deck its own prefix, **`deck-v*`**, and trigger only on that.
  Section 6 has what that means for the app.
- **Paths:** the `X2ROCK_REF` checkout and the second `actions/checkout` go away. So do the
  `path: x2rockdeck` / `working-directory: x2rockdeck` lines. Steps run at the root with `-p`,
  or in `deck/` for packaging. `Swatinem/rust-cache` takes the workspace root.
- **Running the token test:** the deck's token-refresh test runs only where the credential store
  is temporary (`services::tests::a_token_refreshed_by_a_search_ended_early_is_the_one_held`). A
  plain `cargo test` skips it, as the deck's CI does today. Set `XDG_STATE_HOME` to a temporary
  directory in the deck jobs, and it runs.

**Until the move:** `build.yml` checks out x2rock at `X2ROCK_REF: gui-library`, a moving branch.
Pin it to the commit the deck was last checked against:

```yaml
X2ROCK_REF: 877ef86b08f656609d14d3b2bc7ee72dc49daddf   # gui-library, 2026-10-30
```

## 6. What changes in the app

- **The update check would announce x2rock's releases.** `src/update.rs:10` asks
  `api.github.com/repos/rahga/x2rockdeck/releases/latest`. Pointed at x2rock, `latest` is the
  CLI's `v0.2.0`, which is newer than the deck's 0.1.0, so every copy of the deck would say a
  release is out. List `/releases` instead, take the newest `deck-v*` tag, and strip `deck-v` in
  `newer()`. Keep `update::tests::versions_compare_as_numbers` and add a case for a CLI tag.
- **Links** to `rahga/x2rockdeck`:
  - `src/update.rs:11` `RELEASES`;
  - `src/report.rs:15` `ISSUES`: Send a report's QR code opens a new issue there. In a shared
    repository, add a label or title prefix so deck reports are told apart;
  - `docs/install-on-steam-deck.md:10`, `:78` and `docs/install-on-windows.md:7`;
  - `README.md:100` (the `../x2rock` clone step goes away);
  - `Cargo.toml` `repository`.
- **Tests:** after the move, from the root: `cargo test --locked -p x2rock-deck` and
  `cargo test --locked` (x2rock), with isolated XDG directories as `pl check` sets them, then
  `cargo clippy --locked --all-targets -p x2rock-deck -- -D warnings` and the same for x2rock.

## 7. What the move needs from x2rock

The deck builds against `gui-library`, **16 commits ahead of x2rock's `main`**. They have to
reach `main` first, or come with the move. Oldest first:

| Commit | What it does |
|---|---|
| `a1c1cd6` | The Sonos layer as a library, the CLI behind the default `cli` feature |
| `91b247b` | `Clone` on the protocol's reply types |
| `70ef163` | `art::local_file`, public, for a front end in the same process |
| `05f13bf` | `Clone` on the UPnP data types |
| `b342f4c` | The play path moved into `x2rock::play` |
| `c113a63` | `netid`: the gateway MAC off Linux, through `netdev` |
| `7e957cd` | Saving the playing item as a favourite, in `x2rock::play` |
| `67f76e6` | Rating, in `x2rock::play`, taking `RatingKind` |
| `03f7d7e` | `discover`: SSDP before the subnet sweep |
| `e4a7fb1` | Reading the household's stored sign-ins, in `x2rock::import` |
| `8351a7e` | Replaying a recent item, in `x2rock::play` |
| `0119a50` | Building clean on Windows (a Unix-only import and `mut`) |
| `7c7c7df` | `upnp`: numeric EQ settings (SubGain, SurroundLevel …) |
| `5a088ac` | `smapi::Item` compares by value |
| `9fc59d1` | `upnp`: one queue slot, for naming what plays before the current track |
| `877ef86` | `art`: `failure_kind`, permanent against transient, for retry cooldowns |

Of these, **only `877ef86` was made by this punch list** (D6). D1-D5 and D7 changed no library
code. With the deck in the tree, these commits stop being "for a front end" and become changes
for a member of the same workspace. Their own messages are fine as they are.

## Order of work

1. Merge `gui-library` into x2rock's `main`, after deciding its formatting (section 4). Check
   that x2rock's own CI passes.
2. Pin `X2ROCK_REF` in the deck's `build.yml` until step 5 lands (section 5).
3. In x2rock: `git subtree add --prefix=deck <x2rockdeck> master`, or a plain copy, if the deck's
   history needn't come along. Then:
   - the workspace table, `default-members` and `release-deck` profile (section 3);
   - the deck's `x2rock` path and `repository`;
   - `deck/Cargo.lock` removed.
4. Resolve the one lock, then run both suites and both clippy runs (section 6).
5. CI jobs (section 5); the update check and links (section 6); a licence line in the README
   (section 2).
6. Tag `deck-v0.1.0` from the merged tree, and check that the release has both packages and that
   the deck's update check ignores `v0.2.0`.
7. Archive `rahga/x2rockdeck` with a pointer to `x2rock/deck`. Keep its existing release links
   working: GitHub keeps an archived repository's releases.
