# Storage Analytics

Lightweight Rust TUI for browsing disk usage on Windows.

## What it does

- Lists all accessible drives with used, free, and total space.
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
- `q`: quit

## Notes

- Reparse points such as symlinks and junctions are shown, but not traversed, to avoid loops.
- Some protected folders may report access errors. The scan still continues for the rest of the tree.
- Large folders can take time to total; the UI stays responsive, shows partial sizes early, and scans sibling folders with a small worker pool.
- Cached folder values are shown immediately as `(...cached)` and are then validated or recomputed in the background.
- The cache database is stored next to the app as `storage_analytics_cache.sqlite3`.
- The folder fingerprint is metadata-based so the cache stays fast; a refresh updates the stored values when folder listings no longer match the cache.

## Run

```powershell
cargo run
```
