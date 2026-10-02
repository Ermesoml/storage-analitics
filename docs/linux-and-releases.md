# Linux support and automatic releases

## Scope and acceptance criteria

The Linux port shares the existing TUI, scanner, SQLite cache and deletion controls
with Windows. Platform adapters provide mounted filesystems, capacities, link
detection, metadata fingerprints and path keys. Linux paths remain case-sensitive;
symlinks are shown without recursive traversal. Existing Windows APIs stay in the
Windows adapter.

Each Linux scan snapshots mount boundaries, skips virtual filesystems and stops
recursion at nested mounts to prevent bind-mount loops and duplicate totals. Real
mounts can be entered separately. Mount changes invalidate cached fingerprints;
mounted paths and directories containing mounts cannot be deleted by the app.

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
python3 -m unittest discover -s .github/scripts -p 'test_*.py' -v
```

Rust source uses the existing formatting and `io::Result` conventions. Platform
code validates procfs records and preserves raw path bytes. Tests use synthetic
temporary directories; runtime caches and credentials stay outside Git.

The PR changes application support and workflows. It does not merge itself, create
a production release, grant collaborators write access, or publish local disk data.

## Version allocation and recovery

`.github/release-base.json` fixes the existing main commit and `0.1.0` as the
baseline. Each following commit on main's first-parent history increments the
minor number, with patch zero. This makes allocation independent of job order,
push batching and retries. The package manifest remains at the baseline version;
release builds inject `STORAGE_ANALYTICS_VERSION`, which `--version` reports.

Every authorized run finds missing releases up to its triggering commit, including
commits missed by a previous failed run. The workflow queues up to 100 runs using
GitHub's `concurrency.queue: max` and does not cancel running releases. If a run
is canceled or the queue fills, use **Run workflow** on `main` to catch up.

The build job has read-only repository permission. It tests each source commit,
builds a static `x86_64-unknown-linux-musl` executable, verifies its version and
drives its TUI in a pseudo-terminal. A separate publishing job receives only the
manifest, archives and checksums; it does not execute the application. Only this
job gets `contents: write`. Checkout credentials are not persisted, and third-party
actions are pinned to commit SHAs.

The publisher rechecks both actors, verifies each manifest entry against main
history, rejects conflicting tags, creates exact-commit tags and draft releases,
uploads both assets, checks GitHub's SHA-256 digests, then publishes. A retry can
replace incomplete draft assets; it never overwrites published releases. GitHub's
`make_latest: legacy` selects the highest semantic version rather than the last
job to finish. Releases created with `GITHUB_TOKEN` do not trigger another release
workflow, and no extra token secret is required.

An owner-only temporary feature-branch workflow verified `GITHUB_TOKEN` with
`contents: write` can create/delete tags for existing plain and workflow-modified
commits, create/update a draft using the pre-created tag, and upload assets whose
API digest matches the local SHA-256. All test tags and drafts were removed; the
probe workflow is excluded from this PR's final tree. GitHub's token policy can
change; permission errors must fail closed rather than publish another source.

Protect main against force pushes and keep the baseline and version namespace
stable. Rewriting main or creating a conflicting `vN.N.0` tag makes automation
stop rather than silently reuse a version for different source. Retry temporary
dependency/network failures. Historical source commits that cannot build require
an explicit release-policy decision; changing a later commit does not repair the
historical source.

## Access boundary

The workflow validates `github.actor` and `github.triggering_actor` through the
collaborator permissions API and fails closed on lookup errors. Only admins can
publish through this workflow. It checks the upstream repository and `main` ref,
so a feature-branch dispatch is rejected.

GitHub itself allows write-role users to create releases and edit workflows.
There is no separate native "admin-only releases" permission on a personal
repository. Preserve the current owner-only write access; outside contributors
can fork and submit PRs. CODEOWNERS requests owner review for automation changes,
but enforcement requires repository branch rules and is not supplied by that file
alone. An admin who changes repository policy can always change this boundary.

## Workflow validation

GitHub documents `queue: max` in
[workflow concurrency](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency).
Actionlint 1.7.12 does not yet recognize that key. All other checks can be run with
the narrow compatibility exception:

```sh
actionlint -ignore 'unexpected key "queue" for "concurrency" section'
```
