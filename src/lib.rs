#![doc = include_str!("../README.md")]
#![doc(html_root_url = "https://docs.rs/zoter/0.1.0")]
// The crate builds nothing on platforms other than Linux, where systemd is.
#![cfg(target_os = "linux")]
#![deny(missing_docs, rust_2018_idioms)]

mod save;
pub use save::{Restore, Save};

#[cfg(test)]
mod test_utils;
