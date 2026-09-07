# Release process

GitHub Actions builds native release binaries for the four targets in the
product contract. SQLite is compiled into each executable through rusqlite's
`bundled` feature.

## MSRV evidence

The package declares Rust 1.88 as its minimum supported Rust version. At the
time this baseline was selected, Cargo metadata for the locked dependency graph
reported `rust_version = 1.88.0` for Ratatui 0.30.2 and its core, Crossterm,
widget, and terminal integration crates. CI installs Rust 1.88.0 and runs the
locked test suite, so a dependency update cannot silently raise the baseline.

## Artifacts

Pushing a `v*` tag that exactly matches the Cargo package version runs the
release workflow. Each native
runner:

1. builds `tmnotify` with `cargo build --locked --release --target TARGET`;
2. confirms `tmnotify --version` works;
3. packages it twice with the commit timestamp as `SOURCE_DATE_EPOCH` and
   requires byte-identical archives;
4. emits an archive and adjacent SHA-256 checksum; and
5. smoke-tests the extracted executable on the build host.

Archives contain exactly one executable plus README, licenses, domain/design
documents, this release guide, and accepted ADRs. The publishing job has the
only `contents: write` permission and runs only for a tag event. A manual run
builds and verifies artifacts but does not create or modify a GitHub Release.

The packaging script can also be exercised locally:

```console
cargo build --locked --release
SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)" \
  scripts/package-release.sh \
  x86_64-unknown-linux-gnu target/release/tmnotify dist
```

## tmux compatibility

The required Linux gate builds the floating-pane upstream commit verified by
the project and probes it only through a private temporary socket with
`-f /dev/null`. A scheduled workflow separately builds the then-current tmux
`master`, records its resolved commit, and runs the same isolated probe. No CI
command addresses a user's or runner's default tmux server.

The pinned compatibility commit is updated only after the capability spike is
repeated and its result is recorded in `docs/tmux-3.8-capability-spike.md`.
