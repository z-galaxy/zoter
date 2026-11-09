# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

For contribution conventions — commit-message format, atomic commits, code layout, and
more — follow the guidelines in [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Project Overview

zoter lets a service that systemd runs restart without dropping its clients: the instance that
stops hands its state and the connections of its clients over to the next one, through the file
descriptor store (the store) that systemd keeps for the service across its restarts. It was taken
out of busd, which restarts that way, so that any service can, zlink-based ones first. A protocol
hands its connections over through two traits of zoter, `Save` and `Restore`, which zbus
implements for D-Bus and zlink for Varlink: zoter knows nothing of either protocol.

The crate is Linux-only, as systemd is: on other platforms it builds nothing. Its API is
synchronous, and depends on no async runtime: pausing a server, and timing that out, is the work of
its caller.

The design, and why it is so, is in `docs/superpowers/specs/2026-11-09-zoter-design.md`.

It is a single crate at the repository root — not a workspace.

## Common Development Commands

```bash
# Full test suite, every feature on
cargo test --all-features

# Format code (requires nightly)
cargo +nightly fmt --all

# Lint with clippy
cargo clippy --all-targets --all-features -- -D warnings

# Check that the crate builds without its default features (no `tracing`)
cargo check --no-default-features

# Check that the crate builds, empty, on platforms other than Linux
cargo check --all-features --target x86_64-apple-darwin
cargo check --all-features --target x86_64-pc-windows-gnu

# Documentation
cargo doc --all-features
```

## Development Guidelines

- **MSRV**: 1.88.0, which zlink has.
- **Commit style**: Emoji prefix only, no package prefix — this is a single crate (e.g.,
  "🐛 Fix the count of the stored file descriptors").
- **Changelog**: `CHANGELOG.md` is managed by [release-plz] — do **not** hand-edit it. Write a
  good commit message and release-plz will generate the entry at release time.
- **Changelog-skip trailer**: end a commit message with a `Changelog: skip` git trailer to keep it
  out of the user-facing changelog (use for AI-workflow artifacts such as design docs and
  implementation plans).
- **Line length**: 100 chars, in code and docs alike.
- **Dependencies**: keep them few; the crate is meant to be pulled in by zbus and zlink.

[release-plz]: https://release-plz.ieni.dev/
