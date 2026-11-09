//! Logging through `tracing`, or nothing without the feature of the same name.
//!
//! The macros are for the modules declared after this one, as `#[macro_use]` has it.

/// Log a warning, as `tracing::warn!` does, if the `tracing` feature is on.
macro_rules! warn {
    ($($arg:tt)*) => {{
        #[cfg(feature = "tracing")]
        tracing::warn!($($arg)*);
        // What would be logged is still used, so that nothing is unused without the feature.
        #[cfg(not(feature = "tracing"))]
        if false {
            let _ = format_args!($($arg)*);
        }
    }};
}

/// Log what helps debugging, as `tracing::debug!` does, if the `tracing` feature is on.
macro_rules! debug {
    ($($arg:tt)*) => {{
        #[cfg(feature = "tracing")]
        tracing::debug!($($arg)*);
        #[cfg(not(feature = "tracing"))]
        if false {
            let _ = format_args!($($arg)*);
        }
    }};
}
