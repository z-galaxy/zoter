#![doc = include_str!("../README.md")]
#![doc(html_root_url = "https://docs.rs/zoter/0.1.0")]
// The crate builds nothing on platforms other than Linux, where systemd is.
#![cfg(target_os = "linux")]
#![deny(missing_docs, rust_2018_idioms)]

#[macro_use]
mod log;

mod handover;
pub use handover::{Handover, LeftOut, Stored};
mod layout;
mod limit;
pub use limit::raise_fd_limit;
mod memfd;
mod notifier;
pub use notifier::Notifier;
mod passed;
pub use passed::Passed;
mod queue;
mod restored;
pub use restored::{Restored, RestoredItem};
mod save;
pub use save::{Restore, Save};
mod store;
pub use store::Store;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod test_utils;
