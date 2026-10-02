# Storage Analytics

Lightweight Rust TUI for browsing disk usage on Windows and Linux.

## What it does

- Lists accessible Windows drives or Linux mounted filesystems with used, free, and total space.
- Lets you enter a drive and inspect the folders and files inside it.
- Computes folder sizes recursively, so each directory shows the size of all content below it.
- Lets you keep drilling into subfolders to find large areas you may want to clean up.
- Scans each level on demand instead of indexing the whole machine up front.
- Streams partial folder sizes while the recursive total is still being calculated.
- Stores folder fingerprints and recursive sizes in a local SQLite cache for faster repeated runs.

## Controls

- `Up` / `Down` or `j` / `k`: move selection
- `Enter`: open the selected drive or folder
- `Backspace` or `Esc`: go up one level
- `d`: return to the drive list
- `r`: refresh the current view
- `Delete`: permanently delete the selected item after confirmation
- `q`: quit

## Notes

- Reparse points such as symlinks and junctions are shown, but not traversed, to avoid loops.
- Linux discovers mount points from `/proc/self/mountinfo`, including `/` in containers, and omits virtual filesystems such as `/proc` and `/sys`.
- Linux cache keys preserve case-sensitive and non-UTF-8 paths. Control characters in names are escaped in the terminal.
- Some protected folders may report access errors. The scan still continues for the rest of the tree.
- Large folders can take time to total; the UI stays responsive, shows partial sizes early, and scans sibling folders with a small worker pool.
- Cached folder values are shown immediately as `(...cached)` and are then validated or recomputed in the background.
- The cache database is stored in the current working directory as `storage_analytics_cache.sqlite3`.
- The cache contains local directory paths, sizes, and timestamps. Keep it and its SQLite journal files private; they are excluded from Git.
- The folder fingerprint is metadata-based so the cache stays fast; a refresh updates the stored values when folder listings no longer match the cache.

## Run

```sh
cargo run --locked
# Start directly in a directory:
cargo run --locked -- /path/to/folder
# Show the build version without opening the TUI:
cargo run --locked -- --version
```

Linux and Windows builds use the pinned Rust toolchain and committed `Cargo.lock`.
Deletion bypasses the recycle bin/trash; cancel the confirmation to keep an item.

## Linux verification with Ubuntu Docker

```sh
docker build -t storage-analytics-ubuntu -f tests/docker/Dockerfile .
docker run --rm -v "$PWD:/workspace" -e CARGO_TARGET_DIR=/tmp/target \
  storage-analytics-ubuntu sh -c 'cargo test --locked && cargo build --locked --release && python3 tests/smoke_tui.py /tmp/target/release/storage_analytics'
```

The smoke test runs the actual TUI in a pseudo-terminal and creates only disposable
fixture files. For interactive browsing inside the container, run:

```sh
docker run --rm -it -v "$PWD:/workspace" -e CARGO_TARGET_DIR=/tmp/target \
  storage-analytics-ubuntu cargo run --locked -- /workspace
```

To test a static executable, build with `--target x86_64-unknown-linux-musl`
(or `aarch64-unknown-linux-musl` on ARM64), then run the same smoke test against
the executable in that target's release directory. The image includes musl tools.
See [dependency audit notes](docs/dependency-audit.md) for known transitive warnings.
