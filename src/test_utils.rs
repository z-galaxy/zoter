//! What the unit tests share.

use std::os::fd::OwnedFd;

use rustix::pipe::{PipeFlags, pipe_with};

/// The two ends of a pipe, read first.
pub(crate) fn pipe() -> (OwnedFd, OwnedFd) {
    pipe_with(PipeFlags::CLOEXEC).unwrap()
}
