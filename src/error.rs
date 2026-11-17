//! The error type of zoter's API, [`Error`], and its [`Result`] alias.

use std::{fmt, io};

use rustix::io::Errno;

use crate::store::MIN_STORE;

/// An error from zoter while a service takes the file descriptors that systemd passed, talks to
/// the service manager, hands over to its next instance, or takes over from the previous one.
///
/// Each variant converts into an [`io::Error`] of the [`ErrorKind`](io::ErrorKind) that its doc
/// names. So a service whose functions return [`io::Result`] can use `?` on an `Error`.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error, for example from the notification socket, a memfd or a queue socket. It is
    /// also the system's `EMFILE` when the file descriptors of a hand-over don't fit under the
    /// limit of open files.
    ///
    /// [`Store::take_over`](crate::Store::take_over) fails with `EMFILE` when the process's soft
    /// limit of open files is too low for the file descriptors of the hand-over.
    /// [`raise_fd_limit`](crate::raise_fd_limit) raises the soft limit, and the service's
    /// `LimitNOFILE=` raises the hard one. Converted into an [`io::Error`], it becomes the wrapped
    /// error itself.
    Io(io::Error),
    /// The environment describes the passed file descriptors, or the notification socket, in a way
    /// that makes no sense.
    ///
    /// [`Passed::from_env`](crate::Passed::from_env) fails with it for an invalid `$LISTEN_PID`,
    /// `$LISTEN_FDS` or `$LISTEN_FDNAMES`. [`Store::from_env`](crate::Store::from_env) fails with
    /// it for a `$NOTIFY_SOCKET` that is neither a path nor an abstract name, and for a `/` or an
    /// `@` alone, which names nothing. The fault lies with whoever set the environment, not with
    /// the service. Converted into an [`io::Error`], it has the kind
    /// [`io::ErrorKind::InvalidData`].
    Environment(String),
    /// A store key that zoter does not accept.
    ///
    /// [`Store::from_env`](crate::Store::from_env) fails with it. The message lists the allowed
    /// characters. Converted into an [`io::Error`], it has the kind
    /// [`io::ErrorKind::InvalidInput`].
    InvalidName(String),
    /// No service manager takes notifications from the process.
    ///
    /// [`Store::from_env`](crate::Store::from_env) fails with it if `$NOTIFY_SOCKET` is unset or
    /// empty. systemd sets it for a service whose unit is of `Type=notify` or has a
    /// `FileDescriptorStoreMax=` above 0, among others. So this is the error of a process that
    /// systemd doesn't run, and of a service whose unit has neither. Such a process has no store
    /// to restart through. Converted into an [`io::Error`], it has the kind
    /// [`io::ErrorKind::Unsupported`].
    NoManager,
    /// systemd keeps no file descriptor store for the service.
    ///
    /// [`Store::from_env`](crate::Store::from_env) fails with it if `$FDSTORE` is unset or not a
    /// number, for a service that systemd takes notifications from. systemd sets `$FDSTORE` for a
    /// service whose unit has a `FileDescriptorStoreMax=` above 0, from version 254 on. Converted
    /// into an [`io::Error`], it has the kind [`io::ErrorKind::Unsupported`].
    NoStore,
    /// The store takes fewer file descriptors than any hand-over needs.
    ///
    /// [`Store::from_env`](crate::Store::from_env) fails with it. The service's
    /// `FileDescriptorStoreMax=` raises the number. Converted into an [`io::Error`], it has the
    /// kind [`io::ErrorKind::Unsupported`].
    StoreTooSmall {
        /// The number of file descriptors that the store takes.
        max: usize,
    },
    /// systemd passed the service no socket of socket activation.
    ///
    /// [`Store::from_env`](crate::Store::from_env) fails with it if systemd passed nothing but
    /// what is stored under the key of the store, if that. A service tells systemd that it has
    /// started as it takes over, before it has restored what it took over, which only holds where
    /// its clients can wait for it meanwhile, in a socket that systemd keeps. So the service needs
    /// a socket unit, and `Requires=` on it in its own unit: systemd otherwise starts the service
    /// without the socket where the socket unit isn't active. Converted into an [`io::Error`], it
    /// has the kind [`io::ErrorKind::Unsupported`].
    NoSocket,
    /// The store has no room for the file descriptors of this hand-over.
    ///
    /// [`Store::hand_over`](crate::Store::hand_over) fails with it. Raising the service's
    /// `FileDescriptorStoreMax=` makes room for later hand-overs. Converted into an [`io::Error`],
    /// it has the kind [`io::ErrorKind::StorageFull`].
    StoreFull,
    /// The hand-over state or an item could not be saved. `item` identifies the item, or is `None`
    /// for the state. `source` contains the error returned by `Save::save`, if it was called.
    ///
    /// [`Store::hand_over`](crate::Store::hand_over) fails with it and stores nothing. Converted
    /// into an [`io::Error`], it has the kind [`io::ErrorKind::InvalidInput`].
    Unsaved {
        /// The id of the item, or `None` for the state.
        item: Option<u64>,
        /// The error returned by [`Save::save`](crate::Save::save), if it was called and failed.
        /// It is `None` when [`Save::saved_len`](crate::Save::saved_len) returned `None`.
        source: Option<Box<Error>>,
    },
    /// A hand-over that this version of zoter cannot make sense of.
    ///
    /// [`Store::take_over`](crate::Store::take_over) fails with it, also when file descriptors of
    /// the state or of an item are missing. The [`Restore`](crate::Restore) implementations of
    /// zoter's own types fail with it for a record or file descriptors that don't match the type.
    /// The service then starts afresh, without what was handed over. Converted into an
    /// [`io::Error`], it has the kind [`io::ErrorKind::InvalidData`].
    InvalidHandover(String),
    /// A hand-over that a later version of zoter laid out.
    ///
    /// [`Store::take_over`](crate::Store::take_over) fails with it for a hand-over in a later
    /// version of the format, or one that needs a format feature that this version doesn't know.
    /// This happens after a downgrade, for example. The service then starts afresh, without what
    /// was handed over. Converted into an [`io::Error`], it has the kind
    /// [`io::ErrorKind::Unsupported`].
    UnsupportedHandover(String),
    /// The service manager did not process the messages sent to it in time.
    ///
    /// [`Store::hand_over`](crate::Store::hand_over) waits for systemd to process what it stored,
    /// and fails with it. Converted into an [`io::Error`], it has the kind
    /// [`io::ErrorKind::TimedOut`].
    TimedOut,
}

/// The result type of zoter's API: a result whose error is an [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The error of a system call that failed with `errno`.
    pub(crate) fn os(errno: Errno) -> Self {
        Self::Io(errno.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Environment(message)
            | Self::InvalidName(message)
            | Self::InvalidHandover(message)
            | Self::UnsupportedHandover(message) => f.write_str(message),
            Self::NoManager => f.write_str(
                "no service manager takes notifications from the process, as `$NOTIFY_SOCKET` is \
                 unset or empty: systemd doesn't run it as a service, or its unit has neither \
                 `Type=notify` nor `FileDescriptorStoreMax=`",
            ),
            Self::NoStore => write!(
                f,
                "systemd keeps no file descriptor store for the service, as `$FDSTORE` is unset \
                 or not a number: set `FileDescriptorStoreMax=` of the service to {MIN_STORE} at \
                 least, under systemd 254 or later"
            ),
            Self::StoreTooSmall { max } => write!(
                f,
                "the file descriptor store takes {max}, which is too few: raise \
                 `FileDescriptorStoreMax=` of the service to {MIN_STORE} at least"
            ),
            Self::NoSocket => f.write_str(
                "systemd passed the service no socket of socket activation: add a socket unit for \
                 the service, and `Requires=` on it to the unit of the service, so that systemd \
                 never starts the service without it",
            ),
            Self::StoreFull => f.write_str(
                "the file descriptor store takes too few file descriptors for those of the \
                 hand-over: raise `FileDescriptorStoreMax=` of the service",
            ),
            Self::Unsaved { item, source } => match (item, source) {
                (None, None) => f.write_str("the state of the hand-over can't be saved"),
                (Some(id), None) => write!(f, "item {id} of the hand-over can't be saved"),
                (None, Some(e)) => {
                    write!(f, "the state of the hand-over failed to be saved: {e}")
                }
                (Some(id), Some(e)) => write!(
                    f,
                    "the record of item {id} of the hand-over failed to be saved: {e}"
                ),
            },
            Self::TimedOut => {
                f.write_str("the service manager didn't process the notifications in time")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Unsaved {
                source: Some(e), ..
            } => Some(&**e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Convert into an [`io::Error`] of the [`ErrorKind`](io::ErrorKind) that the variant's doc names.
/// The result keeps the message and the source of the error. [`Error::Io`] becomes the wrapped
/// error itself.
impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match e {
            Error::Io(e) => return e,
            Error::Environment(_) | Error::InvalidHandover(_) => io::ErrorKind::InvalidData,
            Error::InvalidName(_) | Error::Unsaved { .. } => io::ErrorKind::InvalidInput,
            Error::NoManager
            | Error::NoStore
            | Error::StoreTooSmall { .. }
            | Error::NoSocket
            | Error::UnsupportedHandover(_) => io::ErrorKind::Unsupported,
            Error::StoreFull => io::ErrorKind::StorageFull,
            Error::TimedOut => io::ErrorKind::TimedOut,
        };

        io::Error::new(kind, e)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    /// Each error converts into an `io::Error` of its kind with the same message. `Error::Io`
    /// converts into the wrapped error itself.
    #[test]
    fn into_io_errors() {
        let unsaved = Error::Unsaved {
            item: Some(2),
            source: Some(Box::new(Error::TimedOut)),
        };
        let errors = [
            (
                Error::Environment("env".to_owned()),
                io::ErrorKind::InvalidData,
            ),
            (
                Error::InvalidName("name".to_owned()),
                io::ErrorKind::InvalidInput,
            ),
            (Error::NoManager, io::ErrorKind::Unsupported),
            (Error::NoStore, io::ErrorKind::Unsupported),
            (Error::StoreTooSmall { max: 1 }, io::ErrorKind::Unsupported),
            (Error::NoSocket, io::ErrorKind::Unsupported),
            (Error::StoreFull, io::ErrorKind::StorageFull),
            (unsaved, io::ErrorKind::InvalidInput),
            (
                Error::InvalidHandover("bad".to_owned()),
                io::ErrorKind::InvalidData,
            ),
            (
                Error::UnsupportedHandover("later".to_owned()),
                io::ErrorKind::Unsupported,
            ),
            (Error::TimedOut, io::ErrorKind::TimedOut),
        ];
        for (error, kind) in errors {
            let message = error.to_string();
            let io_error = io::Error::from(error);
            assert_eq!(io_error.kind(), kind, "{message}");
            assert_eq!(io_error.to_string(), message);
        }

        let io_error = io::Error::from(Error::os(Errno::MFILE));
        assert_eq!(io_error.raw_os_error(), Some(Errno::MFILE.raw_os_error()));
        assert!(io_error.get_ref().is_none());
    }

    /// The source of a state that failed to be saved is the error from its `save`. An item that
    /// can't be saved has no source. The source of an I/O error is the wrapped error.
    #[test]
    fn sources() {
        let error = Error::Unsaved {
            item: None,
            source: Some(Box::new(Error::StoreFull)),
        };
        assert_eq!(
            error.to_string(),
            format!(
                "the state of the hand-over failed to be saved: {}",
                Error::StoreFull
            )
        );
        let source = error.source().unwrap().downcast_ref::<Error>();
        assert!(matches!(source, Some(Error::StoreFull)), "{source:?}");
        let error = Error::Unsaved {
            item: Some(3),
            source: None,
        };
        assert!(error.source().is_none());
        assert_eq!(error.to_string(), "item 3 of the hand-over can't be saved");

        let error = Error::from(io::Error::other("other"));
        assert_eq!(error.source().unwrap().to_string(), "other");
        assert_eq!(error.to_string(), "other");
    }
}
