# turya

Fast, lightweight, self-improving microkernel agent harness (Rust alternative to Claude Code / OpenCode).

## Quick Install

```bash
# Binary install (Linux/macOS)
curl -fsSL https://raw.githubusercontent.com/thoughtoinnovate/turya/main/install.sh | bash
```

## Installation

### Option A: Install Script (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/thoughtoinnovate/turya/main/install.sh | bash
```

The script detects your platform, downloads the matching release binary plus its
`.sha256` checksum, verifies integrity, and installs to `/usr/local/bin`.
It doubles as an updater — re-running it upgrades when a newer release exists:

```bash
./install.sh --check              # report latest vs installed, change nothing
./install.sh --version v0.1.0     # pin a specific version
./install.sh --install-dir ~/.local/bin
```

### Option B: Manual Download

Download from [GitHub Releases](https://github.com/thoughtoinnovate/turya/releases):

| Platform | Binary |
|----------|--------|
| **Linux x64** | `turya-linux-x86_64-musl` |
| **Linux ARM64** | `turya-linux-arm64-musl` |
| **macOS Intel** | `turya-darwin-x86_64` |
| **macOS Apple Silicon** | `turya-darwin-arm64` |
| **Windows x64** | `turya-windows-x86_64.exe` |

```bash
# Example: Linux
curl -L https://github.com/thoughtoinnovate/turya/releases/latest/download/turya-linux-x86_64-musl -o turya
chmod +x turya
sudo mv turya /usr/local/bin/
```

> macOS Gatekeeper: binaries downloaded outside the App Store may be quarantined.
> If macOS refuses to run turya: `xattr -d com.apple.quarantine /usr/local/bin/turya`
> (proper notarization is a future milestone).

### Option C: Build from Source

```bash
cargo install --git https://github.com/thoughtoinnovate/turya.git
# or locally:
make install        # debug binary -> ~/.cargo/bin/turya
make install-release # release binary -> ~/.cargo/bin/turya
```

### Verify Installation

```bash
turya --version
```

## Updating

`turya update` applies the latest patch/minor release within your current major
and refuses to cross a major boundary. `turya upgrade` crosses majors after an
explicit confirmation (breaking changes possible):

```bash
turya update              # patch/minor in-place self-update
turya update --check      # report only, change nothing
turya upgrade             # cross-major (asks for confirmation)
turya upgrade --yes       # cross-major, non-interactive
turya update --version v0.1.2   # explicit pin (may downgrade)
```

Both verify SHA256 checksums, keep a `.bak` backup of the replaced binary, and
re-verify via `turya --version` afterwards.

## Cutting a Release

1. Bump the version in `crates/turya-cli/Cargo.toml` (single source of truth).
2. Commit: `git commit -am "chore(release): vX.Y.Z"`.
3. Tag and push: `git tag -a vX.Y.Z -m "turya vX.Y.Z" && git push origin vX.Y.Z`
   (or Actions → Release → Run workflow → tag input).
4. `release.yml` verifies tag == crate version, builds all 6 platform targets,
   and publishes the GitHub Release with binaries + checksums.
5. Hyphenated tags (`v0.2.0-rc.1`) are published as prereleases; `nightly`
   builds (manual workflow) never affect `turya update` / `install.sh`.

## Development

See [`.plans/README.md`](./.plans/README.md) for architecture docs and
[`.plans/EXECUTION_TRACKER.md`](./.plans/EXECUTION_TRACKER.md) for build progress.

```bash
make build   # debug workspace build
make test    # unit + integration tests
make e2e     # deterministic simulation suite (TURYA_SIM_MODE=1)
```
