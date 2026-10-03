# Dependency audit

The committed lockfile was checked with cargo-audit 0.22.2 on 2026-10-02.
No vulnerability-class advisories were reported. Three informational warnings
remain in the existing Ratatui 0.29 dependency tree:

- [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html):
  `paste` 1.0.15 is unmaintained. It is a build-time procedural macro dependency.
- [RUSTSEC-2026-0002](https://rustsec.org/advisories/RUSTSEC-2026-0002.html):
  `lru` 0.12.5 has an unsound `IterMut` implementation.
- [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253.html):
  `lru` 0.12.5 has a panic-safety issue in `pop()` when stored keys have a
  potentially panicking destructor and execution catches that panic.

The application does not use `lru` directly. Ratatui uses it for its layout cache
with `(Rect, Layout)` keys and `get_or_insert`/`resize`, without `IterMut`, custom
panicking key destructors or caught `pop()` panics. The described triggers were
not found in this application's usage. This is a usage assessment, not a claim
that the affected dependency is fixed.

Updating the TUI dependency stack to remove these warnings needs a separate
migration and regression check. Keep auditing the lockfile as advisories change.
