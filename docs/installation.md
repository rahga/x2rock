# Installation, upgrades and rollback

The [quick start](../README.md#quick-start) installs the current Git default branch.
To keep an identifiable build or return to a known working one, select a full commit ID and
keep the old executable before replacing it. `--locked` fixes the crate dependency versions;
the source commit and Rust toolchain are separate inputs.

This procedure uses Bash, Git, Cargo and standard Linux file tools. See
[requirements](../README.md#requirements) for the compiler, speakers and desktop prerequisites.
It installs to `~/.local/bin/x2rock`; if you use a package manager or another path, use that
installation's upgrade mechanism instead. Check `type -a x2rock` for competing copies.

## Build a specific revision

Choose a full commit ID from the repository's history and replace the placeholder below.
Build in a new directory so this does not switch or overwrite an existing checkout:

```sh
set -eu
revision=REPLACE_WITH_FULL_COMMIT_ID
build_dir=$(mktemp -d)
git clone https://github.com/rahga/x2rock "$build_dir/source"
git -C "$build_dir/source" checkout --detach "$revision"
git -C "$build_dir/source" rev-parse HEAD
rustc --version
cargo --version
cargo install --locked --path "$build_dir/source" --root "$build_dir/stage"
"$build_dir/stage/bin/x2rock" --version
sha256sum "$build_dir/stage/bin/x2rock"
```

Continue in the same shell to keep `build_dir`. If any command fails, stop before installing.
Keep the resolved commit ID, toolchain versions and executable's SHA-256 with your build
record. A hash identifies the resulting bytes; this procedure does not promise identical
bytes across toolchains or machines. `--version` normally includes a Git description;
a build without Git metadata can report only the crate version, such as `0.2.0`, and the dirty marker is best effort.
A clean checkout, full commit ID and executable hash give a more complete record.

## First installation

After building, if there is no existing installation to preserve:

```sh
install -Dm755 "$build_dir/stage/bin/x2rock" "$HOME/.local/bin/x2rock"
export PATH="$HOME/.local/bin:$PATH"
"$HOME/.local/bin/x2rock" --version
```

Then [discover your speakers](../README.md#quick-start), followed by
`~/.local/bin/x2rock service install --enable` for desktop integration. CLI commands also work
without the daemon. On a headless machine follow the [headless setup](../README.md#on-a-headless-box),
including lingering. Discovery is a deliberate LAN scan; the build and version checks above
do not contact speakers.

## Upgrade an existing user service

These steps assume the generated user service is already installed and should be running.
Use the same headless or desktop mode as the original installation. For a CLI-only installation,
back up and replace the executable, skipping all unit and systemctl steps.

First check the current installation. `service status` shows the executable in the unit;
it should be the `~/.local/bin/x2rock` that this procedure replaces:

```sh
"$HOME/.local/bin/x2rock" --version
"$HOME/.local/bin/x2rock" service status --json
```

Save the executable and unit before replacing them. The backup directory is private and
outside the temporary build directory. It is a local rollback copy, not a support attachment:
custom unit settings can contain private values.

```sh
install -d "$HOME/.local/share"
backup=$(mktemp -d "$HOME/.local/share/x2rock-rollback.XXXXXXXX")
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
cp -p "$HOME/.local/bin/x2rock" "$backup/x2rock"
cp -a "$unit_dir/x2rock.service" "$backup/x2rock.service"
if [ -d "$unit_dir/x2rock.service.d" ]; then
    cp -a "$unit_dir/x2rock.service.d" "$backup/x2rock.service.d"
fi
"$backup/x2rock" --version > "$backup/version.txt"
sha256sum "$backup/x2rock" > "$backup/SHA256SUMS"
printf 'Rollback directory: %s\n' "$backup"
```

Keep that directory's path. Replace the binary by writing a new file beside it and renaming
it into place. A running daemon can continue using its old executable until the restart:

```sh
install -m755 "$build_dir/stage/bin/x2rock" "$HOME/.local/bin/x2rock.new"
mv -fT "$HOME/.local/bin/x2rock.new" "$HOME/.local/bin/x2rock"
"$HOME/.local/bin/x2rock" --version
"$HOME/.local/bin/x2rock" service install
systemctl --user daemon-reload
systemctl --user restart x2rock.service
```

For a headless installation, add `--headless` to `service install`. An existing headless
drop-in and household selection are preserved on a plain re-run too. This command refreshes
the unit and calls `daemon-reload`; without `--enable`, it does not restart the daemon.
The explicit `daemon-reload` above must succeed before the restart. If installation refuses
hand-edited settings, stop and compare those settings with
`~/.local/bin/x2rock service install --print`; `--force` would overwrite them.

Check the running daemon separately from the new CLI:

```sh
"$HOME/.local/bin/x2rock" service status --json
journalctl --user -u x2rock.service -n 30 --no-pager
```

Expect `installed`, `active` and `enabled` to be true, `exec` to name the intended executable,
and `stale` to be false. `stale` compares executable paths using `/proc`; false by itself does
not prove a version match when that information cannot be read. Find the latest startup line
in the journal: `x2rock <version> running from <path>`. Compare it with the CLI's version.
On the speakers' network, `x2rock rooms` and `x2rock status` then check household connectivity;
an active service alone does not prove any speakers are reachable.

If you installed the agent skill or shell completions, re-run `x2rock skill` with the original
target options and `x2rock completions --install` with the original shell after the upgrade.
The bar widget is installed separately; replacing the binary does not update its QML files.

## Roll back

Use the `backup` directory from above, or set it to the saved path in a new shell. These
commands restore the prior executable and unit, then restart the daemon:

```sh
set -eu
# In a new shell: backup=/absolute/path/to/the/saved/x2rock-rollback.directory
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
sha256sum --check "$backup/SHA256SUMS"
install -m755 "$backup/x2rock" "$HOME/.local/bin/x2rock.new"
mv -fT "$HOME/.local/bin/x2rock.new" "$HOME/.local/bin/x2rock"
cp -a "$backup/x2rock.service" "$unit_dir/x2rock.service"
if [ -d "$backup/x2rock.service.d" ]; then
    cp -a "$backup/x2rock.service.d/." "$unit_dir/x2rock.service.d/"
fi
systemctl --user daemon-reload
systemctl --user restart x2rock.service
"$HOME/.local/bin/x2rock" --version
"$HOME/.local/bin/x2rock" service status --json
journalctl --user -u x2rock.service -n 30 --no-pager
```

Compare the version with `version.txt` and check the latest journal startup line again.
This restores the files saved above; if you added other drop-ins during an upgrade, reconcile
those separately. It does not restore account files, bookmarks or other application state,
and compatibility of that state with arbitrary older revisions is not guaranteed. For an
upgrade that changes persistent formats, follow its migration instructions before proceeding.
Reinstall any exported skill or completions from the restored binary as needed.

## Platforms and release artifacts

As of 2026-10-23, this repository has no tagged release convention or prebuilt-release workflow.
Use a source commit as the build identifier. The procedure was exercised on x86_64 Omarchy
(Arch Linux), using Rust/Cargo 1.98.1 and glibc 2.44, with cached crates. The service commands
were exercised against a simulated systemctl in isolated HOME/XDG directories; that is not a
fresh desktop or headless boot test. CI runs builds and tests on Ubuntu and checks Rust 1.89;
it does not establish hardware support or binary portability.

An executable built here dynamically links system libraries; copying it to an older glibc
distribution is not a supported installation path. No ARM or musl release artifact has been
validated. A future downloadable release needs a named target and minimum runtime, a build on
that baseline, checksums and an actual startup check on that baseline before it is advertised.
