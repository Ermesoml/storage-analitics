# Storage Analytics

Lightweight Rust TUI for browsing disk usage on Windows and Linux, with an
embedded OpenCode AI assistant.

## What it does

- Lists accessible Windows drives or Linux mounted filesystems with used, free, and total space.
- Lets you enter a drive and inspect the folders and files inside it.
- Computes folder sizes recursively, so each directory shows the size of all content below it.
- Lets you keep drilling into subfolders to find large areas you may want to clean up.
- Scans each level on demand instead of indexing the whole machine up front.
- Streams partial folder sizes while the recursive total is still being calculated.
- Stores folder fingerprints and recursive sizes in a local SQLite cache for faster repeated runs.
- Splits the terminal into a 60/40 layout: disk analytics on the left and an AI chat on the right.

## AI chat

The chat pane connects to an OpenCode server started by the app. It uses your
existing OpenCode configuration and providers, so every model you already have
configured is available from the model picker.

- The default model is `opencode-go/deepseek-v4.1-flash`. If that model is not
  available, the app uses the first configured model and shows a warning.
- Each prompt automatically includes the current view, the folder or drive being
  inspected, the selected item, the visible entries with their sizes, and the
  current scan progress. The assistant can then answer questions or use tools
  (`read`, `write`, `edit`, `bash`, `grep`, and so on) against the real files.
- Tool actions that require approval show a permission dialog in the app.
  Choose `y` to allow once, `a` to always allow, or `n` / `Esc` to reject.
- The assistant runs tools with the app's working directory as the OpenCode
  project root. Absolute paths from the attached context work for other drives.
- Use `Tab`, `Shift+Tab`, or the mouse to switch between the analytics pane and
  the chat pane.

### Chat controls

- `Enter`: send the prompt
- `Shift+Enter`: insert a newline
- `Tab` / `Shift+Tab`: switch focus between analytics and chat
- `Ctrl+M`: choose the model
- `Ctrl+N`: start a new chat session
- `Ctrl+C`: abort the current response
- `Ctrl+R`: retry the chat connection after a failure
- `Ctrl+T`: show or hide model reasoning
- `Up` / `Down`, `PageUp` / `PageDown`, `Ctrl+Home` / `Ctrl+End`: scroll chat history
- `Esc`: return focus to the analytics pane
- `Ctrl+Q`: quit

Mouse clicks switch focus; the mouse wheel scrolls the chat or moves the
selection in the analytics pane.

The first launch of the chat can take a few seconds while `opencode serve`
starts and loads your configuration. The analytics pane keeps working if
OpenCode is not installed or cannot start.

### Configuration

The app starts a local `opencode serve` process on a free loopback port with a
generated basic-auth password. You can override this behavior with environment
variables:

- `OPENCODE_BIN`: path to the OpenCode executable if it is not on `PATH`.
- `OPENCODE_SERVER_URL`: connect to an already-running OpenCode server instead
  of starting one (for example `http://127.0.0.1:4096`). If the external server
  uses HTTP basic auth, set `OPENCODE_SERVER_USERNAME` and
  `OPENCODE_SERVER_PASSWORD` as well.

## Controls

- `Up` / `Down` or `j` / `k`: move selection
- `Home` / `End`, `PageUp` / `PageDown`: jump to the start/end or move by a page
- `Enter`: open the selected drive or folder
- `Backspace` or `Esc`: go up one level
- `d`: return to the drive list
- `r`: refresh the current view
- `Delete`: permanently delete the selected item after confirmation
- `q`: quit

## Notes

- Reparse points such as symlinks and junctions are shown, but not traversed, to avoid loops.
- Linux discovers mount points from `/proc/self/mountinfo`, including `/` in containers, and omits virtual filesystems such as `/proc` and `/sys`.
- Recursive totals stop at nested mount points, including bind mounts. Real mounts are marked `mount` and can be opened separately; virtual mounts are marked `virtual` and skipped. Deletion rejects mounted paths and directories containing mounts.
- Linux cache keys preserve case-sensitive and non-UTF-8 paths. Control characters in names are escaped in the terminal.
- Some protected folders may report access errors. The scan still continues for the rest of the tree.
- Large folders can take time to total; the UI stays responsive, shows partial sizes early, and scans sibling folders with a small worker pool.
- Cached folder values are shown immediately as `(...cached)` and are then validated or recomputed in the background.
- The cache database is stored in the current working directory as `storage_analytics_cache.sqlite3`.
- The cache contains local directory paths, sizes, and timestamps. Keep it and its SQLite journal files private; they are excluded from Git.
- The folder fingerprint is metadata-based so the cache stays fast; a refresh updates the stored values when folder listings no longer match the cache.
- OpenCode's interactive question prompts are not implemented in the embedded
  chat yet; such prompts are declined so the assistant can continue with a
  normal chat message.

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
The AI chat requires OpenCode to be installed and configured (`opencode auth login`
or an existing provider configuration). The analytics pane works without it.

To produce a self-contained Windows executable locally:

```powershell
$env:RUSTFLAGS = "-C target-feature=+crt-static"
cargo build --locked --release
# target/release/storage_analytics.exe
```

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

## Automatic Linux releases

After this workflow is merged, each new first-parent commit on `main` gets a
GitHub release and a static Linux **x86_64** executable, packaged as `.tar.gz`
with `SHA256SUMS`. Versions start at `v0.2.0`, then `v0.3.0`, and so on. A merge
commit counts once; rebase merges and pushes with multiple commits produce one
release per main commit. No version-bump commits are generated by the bot.

Download both assets from [Releases](https://github.com/Ermesoml/storage-analitics/releases):

```sh
sha256sum -c SHA256SUMS
tar -xzf storage_analytics-v0.2.0-linux-x86_64.tar.gz
./storage_analytics --version
./storage_analytics
```

The original workflow actor and any rerun actor must both have repository admin
permission. Currently only `Ermesoml` has that access. The **Linux minor releases**
workflow can also be run manually from `main` to retry or recover missing releases.
Retries reuse the same commit/version; uploaded assets are published together.

CI tests Linux x86_64, Linux ARM64 and Windows. Pull requests cannot publish.
Keep contributor access at read/triage and accept contributions through pull
requests: GitHub's native release UI also permits users with write access to
create releases, and write users can change workflows. The checks here gate this
automation; they do not override GitHub's native repository permissions.
See [release design and recovery](docs/linux-and-releases.md) for details.
