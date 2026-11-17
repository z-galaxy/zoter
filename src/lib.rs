#![doc = include_str!("../README.md")]
#![doc(html_root_url = "https://docs.rs/zoter/0.2.0")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/z-galaxy/zoter/ef25fb8c4c17c9d33ada3e9b3edea7830f898774/data/logo.png"
)]
// The crate builds nothing on platforms other than Linux, where systemd is.
#![cfg(target_os = "linux")]
#![deny(missing_docs, rust_2018_idioms)]

#[macro_use]
mod log;

mod error;
pub use error::{Error, Result};
mod layout;
mod limit;
pub use limit::raise_fd_limit;
mod memfd;
mod notifier;
mod passed;
pub use passed::Passed;
mod queue;
mod restored;
pub use restored::{Restored, RestoredItem};
mod save;
pub use save::{Restore, Save};
mod store;
pub use store::{Store, Stored};
#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod test_utils;
