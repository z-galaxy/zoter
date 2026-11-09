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

## Architecture Overview

```
src/
├── lib.rs         # Re-exports the API below; the README is the crate docs
├── log.rs         # warn!/debug! through `tracing`, or nothing without the feature
├── save.rs        # Save and Restore: what an item of a hand-over is, a record and FDs
├── passed.rs      # Passed: the FDs that systemd passes, as sd_listen_fds(3) takes them
├── notifier.rs    # Notifier: $NOTIFY_SOCKET, as sd_notify(3) has it, storing FDs and barriers
├── limit.rs       # raise_fd_limit
├── handover.rs    # Handover: a hand-over that a store started; Stored and LeftOut: what went
├── store.rs       # Store: handover, take_over and storing, fitting the items, the reserved FDs
├── store/tests.rs # Its tests, against testing::Manager in the test process
├── restored.rs    # Restored and RestoredItem: what was handed over, read in place
├── layout.rs      # The format of the memfd: header, table of the items, records
├── memfd.rs       # Sealed memfds, written and read in place through mappings (the unsafe code)
├── queue.rs       # Sockets whose queues hold any number of FDs, as one FD of the store
├── testing.rs     # [testing feature] Manager: a fake service manager for tests
└── test_utils.rs  # What the unit tests share, such as running a test in a process of its own
tests/restart.rs   # [testing feature] A restart end to end: the test binary is the service
```

`Handover::store` lays a hand-over out in one memfd, stored under `<key>/state`, and puts the FDs
of the state and of the items in the queues of sockets, stored under `<key>/fds`, as many as that
takes. `Store::take_over` takes the names of its key out of `Passed`, removes them from the store
first, maps the memfd, and takes the FDs out of the queues by their indexes. The tests that take
FDs from 3 on, or change the environment or the limit of open FDs, run in a process of their own
(`test_utils::in_own_process`).

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
