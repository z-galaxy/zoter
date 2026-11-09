//! The file descriptors that systemd passes to a service: [`Passed`].

use std::{
    env::{self, VarError},
    io::{self, ErrorKind},
    os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    process,
    str::FromStr,
};

use rustix::io::{FdFlags, fcntl_setfd};

/// The file descriptors that the service manager passed to this process, with their names.
///
/// systemd passes a service the sockets of the socket units that activate it, and the file
/// descriptors that were stored in its store before it was restarted, which come last. The name of
/// a socket is the `FileDescriptorName=` of its unit, by default the name of the unit (`connection`
/// if the unit has `Accept=yes`). That of a stored file descriptor is the name it was stored under,
/// or `stored` if it was stored with none, and `unknown` stands for a name that systemd didn't
/// send. The names aren't unique, and the file descriptors come in no set order but for the stored
/// ones coming last, so tell them apart by their names rather than their places.
///
/// The service takes them by name.
#[derive(Debug, Default)]
pub struct Passed {
    fds: Vec<(String, OwnedFd)>,
}

impl Passed {
    /// Take the file descriptors that systemd passed to this process, as `sd_listen_fds(3)` does.
    ///
    /// It takes none if systemd passed none to this process: if the environment doesn't tell of
    /// any, or tells of those of another process. It fails if what the environment tells of them
    /// makes no sense, and closes those that it took by then. The file descriptors it takes are
    /// close-on-exec.
    ///
    /// Whether it succeeds or not, it removes `$LISTEN_PID`, `$LISTEN_FDS` and `$LISTEN_FDNAMES`
    /// from the environment, as `sd_listen_fds(3)` does when asked to, and `$LISTEN_PIDFDID` as
    /// well, which systemd 259 and later set, and which it doesn't check. So no call but the first
    /// takes the file descriptors, and the processes that this one starts don't inherit what tells
    /// of them.
    ///
    /// # Safety
    ///
    /// No other thread may read or write the environment meanwhile, as for
    /// [`std::env::remove_var`]: call this before the process starts any other thread.
    ///
    /// Nothing else in the process may own the file descriptors that the environment tells of, as
    /// nothing does when systemd passes them: this takes them as its own, and closes them as
    /// they're dropped. So nothing may have taken them before, such as an implementation of
    /// `sd_listen_fds(3)` that left the environment as it was, as `sd_listen_fds(0)` does. File
    /// descriptors that the process took some other way go to [`Passed::new`] instead.
    pub unsafe fn from_env() -> io::Result<Self> {
        // Read all the variables before any of them can end this early, as all of them go either
        // way.
        let pid = var("LISTEN_PID");
        let fds = var("LISTEN_FDS");
        let names = var("LISTEN_FDNAMES");
        for name in [
            "LISTEN_PID",
            "LISTEN_PIDFDID",
            "LISTEN_FDS",
            "LISTEN_FDNAMES",
        ] {
            // SAFETY: No other thread reads or writes the environment meanwhile, as the caller
            // sees to.
            unsafe { env::remove_var(name) };
        }

        let Some(count) = parse_count(pid?.as_deref(), fds?.as_deref(), process::id())? else {
            return Ok(Self::default());
        };
        // Once taken, the file descriptors close again if the names turn out to make no sense.
        let fds = take_fds(count)?;
        let names = parse_names(names?.as_deref(), count)?;

        Ok(Self::new(names.into_iter().zip(fds).collect()))
    }

    /// File descriptors passed to this process some other way, with their names.
    pub fn new(fds: Vec<(String, OwnedFd)>) -> Self {
        Self { fds }
    }

    /// Take the first file descriptor named `name`, if any.
    pub fn take(&mut self, name: &str) -> Option<OwnedFd> {
        let index = self.fds.iter().position(|(passed, _)| passed == name)?;

        Some(self.fds.remove(index).1)
    }

    /// Take every file descriptor named `name`, in their order.
    pub fn take_all(&mut self, name: &str) -> Vec<OwnedFd> {
        self.fds
            .extract_if(.., |(passed, _)| passed == name)
            .map(|(_, fd)| fd)
            .collect()
    }

    /// The file descriptors that are left, with their names, in their order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, BorrowedFd<'_>)> {
        self.fds
            .iter()
            .map(|(name, fd)| (name.as_str(), fd.as_fd()))
    }

    /// How many file descriptors are left.
    pub fn len(&self) -> usize {
        self.fds.len()
    }

    /// Whether no file descriptor is left.
    pub fn is_empty(&self) -> bool {
        self.fds.is_empty()
    }

    /// The file descriptors that are left, with their names, in their order.
    pub fn into_vec(self) -> Vec<(String, OwnedFd)> {
        self.fds
    }
}

/// The value of the environment variable `name`, if it is set.
fn var(name: &str) -> io::Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("${name} isn't valid Unicode"),
        )),
    }
}

/// How many file descriptors systemd passed to the process with PID `own_pid`, going by
/// `$LISTEN_PID` and `$LISTEN_FDS`: `None` if it passed none, because they are unset, or
/// `$LISTEN_PID` is another PID.
///
/// As `sd_listen_fds(3)` does, it takes `$LISTEN_FDS` for garbage if it isn't a number from 1 to
/// [`MAX_LISTEN_FDS`], but only looks at it if the PID is right.
fn parse_count(pid: Option<&str>, fds: Option<&str>, own_pid: u32) -> io::Result<Option<usize>> {
    let Some(pid) = pid else {
        return Ok(None);
    };
    if parse_number::<u32>("$LISTEN_PID", pid)? != own_pid {
        return Ok(None);
    }
    let Some(fds) = fds else {
        return Ok(None);
    };
    match parse_number("$LISTEN_FDS", fds)? {
        count @ 1..=MAX_LISTEN_FDS => Ok(Some(count)),
        count => Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("$LISTEN_FDS is {count}, not a number from 1 to {MAX_LISTEN_FDS}"),
        )),
    }
}

/// Take the `count` file descriptors from [`START`] on, and make them close-on-exec.
///
/// Each is found to be open before it's taken: `$LISTEN_FDS` may tell of more than there are, and
/// an `OwnedFd` of one that isn't open aborts the process when it drops, in a debug build.
fn take_fds(count: usize) -> io::Result<Vec<OwnedFd>> {
    (START..)
        .take(count)
        .map(|fd| -> io::Result<OwnedFd> {
            // SAFETY: It's only borrowed for the call below, which fails with `EBADF` if it isn't
            // open, and does nothing else to it.
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            fcntl_setfd(borrowed, FdFlags::CLOEXEC)?;
            // SAFETY: It's open, as the call above shows, and systemd passed it to this process, as
            // `$LISTEN_PID` says. Nothing else owns it, as the caller of `Passed::from_env` sees
            // to, and nothing takes it after this does, as the variables that tell of it are gone.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        })
        .collect()
}

/// The names of `count` file descriptors, going by `$LISTEN_FDNAMES`.
///
/// There is one for each, or none at all, in which case they are all `unknown`.
fn parse_names(names: Option<&str>, count: usize) -> io::Result<Vec<String>> {
    let Some(names) = names else {
        return Ok(vec![UNKNOWN.to_owned(); count]);
    };
    let names: Vec<_> = names.split(':').map(str::to_owned).collect();
    if names.len() != count {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "$LISTEN_FDNAMES has {} names for {count} file descriptors",
                names.len()
            ),
        ));
    }

    Ok(names)
}

/// The number that `value` is, for the environment variable `name`.
fn parse_number<T>(name: &str, value: &str) -> io::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error,
{
    value.parse().map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("{name} is {value:?}, not a number: {e}"),
        )
    })
}

/// The largest count that `$LISTEN_FDS` can have: `sd_listen_fds(3)` takes a larger one for garbage
/// too, as the number of the last file descriptor would overflow.
const MAX_LISTEN_FDS: usize = (RawFd::MAX - START) as usize;

/// The first file descriptor that systemd passes, `SD_LISTEN_FDS_START`.
pub(crate) const START: RawFd = 3;

/// The name of a file descriptor that systemd sent no names for.
const UNKNOWN: &str = "unknown";

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use rustix::io::fcntl_getfd;

    use super::*;
    use crate::test_utils::{in_own_process, is_open, pipe};

    #[test]
    fn count() {
        assert_eq!(parse_count(Some("42"), Some("3"), 42).unwrap(), Some(3));
        assert_eq!(parse_count(Some("42"), Some("1"), 42).unwrap(), Some(1));
        assert_eq!(
            parse_count(Some("42"), Some(&MAX_LISTEN_FDS.to_string()), 42).unwrap(),
            Some(MAX_LISTEN_FDS)
        );
    }

    #[test]
    fn count_is_none_unless_for_this_process() {
        // Nothing is passed if either variable is unset, or the PID is another one. The count isn't
        // even looked at then, so it can't make anything fail.
        assert_eq!(parse_count(None, Some("3"), 42).unwrap(), None);
        assert_eq!(parse_count(Some("42"), None, 42).unwrap(), None);
        assert_eq!(parse_count(None, None, 42).unwrap(), None);
        assert_eq!(parse_count(Some("41"), Some("3"), 42).unwrap(), None);
        assert_eq!(parse_count(Some("43"), Some("3"), 42).unwrap(), None);
        assert_eq!(parse_count(Some("41"), Some("garbage"), 42).unwrap(), None);
    }

    #[test]
    fn count_garbage() {
        for pid in ["", "x", "-1", " 42", "42 ", "4.2", "4294967296"] {
            let error = parse_count(Some(pid), Some("3"), 42).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData, "$LISTEN_PID={pid:?}");
        }

        let too_many = (MAX_LISTEN_FDS + 1).to_string();
        for fds in [
            "",
            "x",
            "-1",
            "0",
            "3 ",
            "1.5",
            "99999999999999999999",
            &too_many,
        ] {
            let error = parse_count(Some("42"), Some(fds), 42).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData, "$LISTEN_FDS={fds:?}");
        }
    }

    #[test]
    fn names() {
        let names = parse_names(Some("web:ssh:stored"), 3).unwrap();
        assert_eq!(names, ["web", "ssh", "stored"]);

        // Names aren't unique, and can be anything but the separator, empty ones included.
        let names = parse_names(Some("a:a:unknown:"), 4).unwrap();
        assert_eq!(names, ["a", "a", "unknown", ""]);
        assert_eq!(parse_names(Some(""), 1).unwrap(), [""]);
        assert_eq!(parse_names(Some("one"), 1).unwrap(), ["one"]);
    }

    #[test]
    fn names_unknown() {
        assert_eq!(
            parse_names(None, 3).unwrap(),
            ["unknown", "unknown", "unknown"]
        );
        assert_eq!(parse_names(None, 1).unwrap(), ["unknown"]);
    }

    #[test]
    fn names_that_do_not_match_the_count() {
        for (names, count) in [("a:b", 3), ("a:b:c", 2), ("a", 2), ("", 2), ("a:b", 1)] {
            let error = parse_names(Some(names), count).unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::InvalidData,
                "{names:?} for {count}"
            );
        }
    }

    /// The file descriptors are taken by name, the first or all of them, in their order, and those
    /// with other names stay.
    #[test]
    fn take_by_name() {
        let fd = || pipe().1;
        let mut passed = Passed::new(vec![
            ("a".to_owned(), fd()),
            ("b".to_owned(), fd()),
            ("a".to_owned(), fd()),
        ]);
        let numbers =
            |passed: &Passed| -> Vec<_> { passed.iter().map(|(_, fd)| fd.as_raw_fd()).collect() };
        let all = numbers(&passed);

        assert!(passed.take("c").is_none());
        assert_eq!(passed.take("a").unwrap().as_raw_fd(), all[0]);
        let names: Vec<_> = passed.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["b", "a"]);
        let taken: Vec<_> = passed
            .take_all("a")
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect();
        assert_eq!(taken, [all[2]]);
        assert_eq!(passed.len(), 1);
        assert!(passed.take_all("a").is_empty());
        let left = passed.into_vec();
        assert_eq!(left[0].0, "b");
        assert_eq!(left[0].1.as_raw_fd(), all[1]);
    }

    #[test]
    fn from_the_environment() {
        let exports = "LISTEN_PID=$$ LISTEN_FDS=2 LISTEN_FDNAMES=first:second LISTEN_PIDFDID=1";
        if !in_own_process("passed::tests::from_the_environment", exports) {
            return;
        }

        // SAFETY: The test runs in a process of its own, which has no other thread that reads or
        // writes the environment, nor anything that owns the file descriptors that it tells of.
        let passed = unsafe { Passed::from_env() }.unwrap();
        let names: Vec<_> = passed.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["first", "second"]);
        let numbers: Vec<_> = passed.iter().map(|(_, fd)| fd.as_raw_fd()).collect();
        assert_eq!(numbers, [3, 4]);
        for (_, fd) in passed.iter() {
            assert!(fcntl_getfd(fd).unwrap().contains(FdFlags::CLOEXEC));
        }
        assert_listen_variables_removed();
        // There is nothing left to take.
        // SAFETY: As above.
        assert!(unsafe { Passed::from_env() }.unwrap().is_empty());
    }

    #[test]
    fn from_the_environment_of_another_process() {
        let exports = "LISTEN_PID=1 LISTEN_FDS=2 LISTEN_FDNAMES=a:b LISTEN_PIDFDID=1";
        if !in_own_process(
            "passed::tests::from_the_environment_of_another_process",
            exports,
        ) {
            return;
        }

        // SAFETY: The test runs in a process of its own, which has no other thread that reads or
        // writes the environment, nor anything that owns the file descriptors that it tells of.
        assert!(unsafe { Passed::from_env() }.unwrap().is_empty());
        assert_listen_variables_removed();
        // The file descriptors are not for this process, which leaves them as they are.
        assert!(is_open(3) && is_open(4));
    }

    #[test]
    fn from_the_environment_closes_all_on_error() {
        // There are two file descriptors, and one name.
        let exports = "LISTEN_PID=$$ LISTEN_FDS=2 LISTEN_FDNAMES=a";
        if !in_own_process(
            "passed::tests::from_the_environment_closes_all_on_error",
            exports,
        ) {
            return;
        }

        // SAFETY: The test runs in a process of its own, which has no other thread that reads or
        // writes the environment, nor anything that owns the file descriptors that it tells of.
        let error = unsafe { Passed::from_env() }.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(!is_open(3) && !is_open(4));
        assert_listen_variables_removed();
    }

    #[test]
    fn from_the_environment_with_fds_that_are_not_open() {
        // Only the file descriptors 3 and 4 are open.
        let exports = "LISTEN_PID=$$ LISTEN_FDS=3";
        let test = "passed::tests::from_the_environment_with_fds_that_are_not_open";
        if !in_own_process(test, exports) {
            return;
        }

        // SAFETY: The test runs in a process of its own, which has no other thread that reads or
        // writes the environment, nor anything that owns the file descriptors that it tells of.
        let error = unsafe { Passed::from_env() }.unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(rustix::io::Errno::BADF.raw_os_error())
        );
        assert!(!is_open(3) && !is_open(4));
        assert_listen_variables_removed();
    }

    fn assert_listen_variables_removed() {
        for var in [
            "LISTEN_PID",
            "LISTEN_PIDFDID",
            "LISTEN_FDS",
            "LISTEN_FDNAMES",
        ] {
            assert!(env::var_os(var).is_none(), "${var} is still set");
        }
    }
}
