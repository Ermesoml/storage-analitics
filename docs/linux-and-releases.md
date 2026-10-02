# Linux support and automatic releases

## Scope and acceptance criteria

The Linux port shares the existing TUI, scanner, SQLite cache and deletion controls
with Windows. Platform adapters provide mounted filesystems, capacities, link
detection, metadata fingerprints and path keys. Linux paths remain case-sensitive;
symlinks are shown without recursive traversal. Existing Windows APIs stay in the
Windows adapter.

The release workflow creates a Linux executable for each new first-parent commit
on `main`, with a minor version bump. A merge commit counts once; a rebase or a push
containing several commits produces one version per commit. Publication requires
an admin actor, including on retries. Feature branches and pull requests cannot
publish releases. Tags and the executable's `--version` identify the release;
automation does not create additional version-bump commits on `main`.

## Implementation order

1. Extract the OS adapter and implement Linux mount discovery and metadata.
2. Exercise scanning, cache isolation, symlinks and TUI navigation in Ubuntu Docker.
3. Add read-only PR checks and admin-gated automatic minor releases.
4. Verify version allocation, retries and access checks; open a PR for review.

## Verification commands

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
python3 tests/smoke_tui.py target/release/storage_analytics
docker build -t storage-analytics-ubuntu -f tests/docker/Dockerfile .
```

Rust source uses the existing formatting and `io::Result` conventions. Platform
code validates procfs records and preserves raw path bytes. Tests use synthetic
temporary directories; runtime caches and credentials stay outside Git.

The PR changes application support and workflows. It does not merge itself, create
a production release, grant collaborators write access, or publish local disk data.
