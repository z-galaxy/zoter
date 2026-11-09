//! The notification socket of the service manager: [`Notifier`].

use std::{
    env,
    ffi::OsStr,
    io::{self, ErrorKind, IoSlice},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    time::{Duration, Instant},
};

use rustix::{
    cmsg_space,
    event::{PollFd, PollFlags, Timespec, poll},
    io::{Errno, retry_on_intr},
    net::{
        AddressFamily, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix,
        SocketFlags, SocketType, sendmsg_addr, socket_with,
    },
    pipe::{PipeFlags, pipe_with},
};

/// The notification socket of the service manager, `$NOTIFY_SOCKET`, as `sd_notify(3)` has it.
///
/// A service sends it its state, such as `READY=1`, and the file descriptors that it wants the
/// manager to keep for it, in its store. The manager takes the messages one at a time, in the order
/// they came, and ties each to a service by the process that sent it, so they have to come from a
/// process that the `NotifyAccess=` of the service lets: the main process, unless the unit says
/// otherwise.
///
/// A send blocks while the queue of the socket of the manager is full, which only happens if the
/// manager is far behind, but it doesn't wait for the manager to read the message. The manager
/// says nothing back, so [`barrier`](Self::barrier) is how to tell that it got to the messages.
#[derive(Debug)]
pub struct Notifier {
    socket: OwnedFd,
    address: SocketAddrUnix,
}

impl Notifier {
    /// The one that `$NOTIFY_SOCKET` names, if it's set and not empty.
    ///
    /// systemd sets it for the services that take notifications, so `None` means that this isn't
    /// one, or isn't a service of systemd at all. It leaves the variable in the environment.
    pub fn from_env() -> io::Result<Option<Self>> {
        match env::var_os("NOTIFY_SOCKET") {
            Some(address) if !address.is_empty() => Self::new(address).map(Some),
            _ => Ok(None),
        }
    }

    /// The one at `address`, in the form of `$NOTIFY_SOCKET`.
    ///
    /// That is the path of a socket if `address` starts with a `/`, and the name of an abstract
    /// socket after an `@` if it starts with that. A `/` or an `@` alone names nothing, which
    /// fails with [`ErrorKind::InvalidInput`]. systemd also has `vsock:` addresses, by which a
    /// virtual machine tells its host that it has booted: they fail with
    /// [`ErrorKind::Unsupported`], as does anything else that is neither of the two.
    pub fn new(address: impl AsRef<OsStr>) -> io::Result<Self> {
        let address = match address.as_ref().as_bytes() {
            [b'/' | b'@'] => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "the notification socket has no name",
                ));
            }
            path @ [b'/', ..] => SocketAddrUnix::new(path)?,
            [b'@', name @ ..] => SocketAddrUnix::new_abstract_name(name)?,
            other => {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "the notification socket `{}` is neither a path nor an abstract name",
                        String::from_utf8_lossy(other),
                    ),
                ));
            }
        };
        let socket = socket_with(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            None,
        )?;

        Ok(Self { socket, address })
    }

    /// Another notifier of the same manager, with a socket of its own.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            socket: self.socket.try_clone()?,
            address: self.address.clone(),
        })
    }

    /// Store `fds` in the store under `name`.
    ///
    /// The manager keeps a duplicate of each of them for the service, and hands it to the main
    /// process of the next instance of the service, along with `name`
    /// ([`Passed::from_env`](crate::Passed::from_env)). It keeps them while the service restarts,
    /// and closes them when the service is stopped for good (`FileDescriptorStorePreserve=`
    /// changes that), or when they are removed ([`remove`](Self::remove)). It doesn't watch them
    /// for hang-ups (`FDPOLL=0`, from systemd 246 on): by default, the manager closes a stored file
    /// descriptor once it sees a hang-up or an error on it, which, for a socket whose peer hangs
    /// up, would drop what the peer sent before that, for the next instance to read. It takes no
    /// more of them than the `FileDescriptorStoreMax=` of the unit at a time, and closes those that
    /// don't fit, as well as those that it has already: it stores an open file description once,
    /// under the name that the first of its file descriptors came with. This only fails if the
    /// message can't be sent, as the manager says nothing of what it did with it.
    /// [`barrier`](Self::barrier) tells that it has dealt with it.
    ///
    /// A name is no longer than 255 characters, all of them printable ASCII ones but `:`, as
    /// systemd ignores a name that isn't, and stores the file descriptors with none instead, as
    /// `stored`. This refuses an empty name as well, which systemd takes, but which is likelier a
    /// mistake than a name. A message has one name for all of its file descriptors, so those that
    /// need names of their own take a call each.
    pub fn store(&self, name: &str, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        check_name(name)?;

        self.notify_with_fds(&format!("FDSTORE=1\nFDPOLL=0\nFDNAME={name}"), fds)
    }

    /// Remove the file descriptors stored under `name`.
    ///
    /// That is all of them, as names aren't unique. The manager closes them, which is also how the
    /// process lets go of a socket that it stored: while the manager has a copy of it, the peer of
    /// a connection doesn't see the connection end when the process closes its own. The name has to
    /// be a valid one, as for [`store`](Self::store).
    pub fn remove(&self, name: &str) -> io::Result<()> {
        check_name(name)?;

        self.notify(&format!("FDSTOREREMOVE=1\nFDNAME={name}"))
    }

    /// Send `state`.
    ///
    /// `state` is a list of assignments such as `READY=1`, each on a line of its own. The manager
    /// takes a message of 4096 bytes at most, and ignores a longer one, as well as one that it
    /// can't make sense of, so this only fails if the message can't be sent.
    pub fn notify(&self, state: &str) -> io::Result<()> {
        self.notify_with_fds(state, &[])
    }

    /// Wait, `timeout` at most, for the manager to process the notifications sent so far.
    ///
    /// This sends a message that asks the manager to close a file descriptor once it gets to the
    /// message, and waits for that. The manager takes its messages in order, so it has dealt with
    /// the earlier ones by then. That doesn't say that it took them: it ignores those that the
    /// `NotifyAccess=` of the service doesn't allow.
    ///
    /// A process needs this before it exits, as the manager may otherwise fail to tie its messages
    /// to the service, once the process is gone.
    ///
    /// It fails with [`ErrorKind::TimedOut`] if the manager takes longer than `timeout`. It blocks
    /// the thread meanwhile.
    pub fn barrier(&self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now().checked_add(timeout);
        // The manager has the only other copy of the write end once this sends it, and closes it
        // once it gets to the message, which hangs up the read end.
        let read = {
            let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
            self.notify_with_fds("BARRIER=1", &[write.as_fd()])?;

            read
        };

        // A hang-up is reported whether `poll` is asked for it or not.
        let mut fds = [PollFd::new(&read, PollFlags::empty())];
        match retry_on_intr(|| poll(&mut fds, remaining(deadline).as_ref()))? {
            0 => Err(io::Error::new(
                ErrorKind::TimedOut,
                "the service manager didn't process the notifications in time",
            )),
            _ => Ok(()),
        }
    }

    /// Send `state`, as [`notify`](Self::notify) does, and `fds` along with it.
    ///
    /// The manager closes the file descriptors that come with a message unless the message stores
    /// them, as [`store`](Self::store) does. No more than 253 file descriptors fit in a message,
    /// which is as many as Linux passes at once: more fail with `E2BIG`, without a message being
    /// sent.
    pub fn notify_with_fds(&self, state: &str, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        if fds.len() > MAX_FDS {
            return Err(Errno::TOOBIG.into());
        }

        let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        // A message with no file descriptors has no control data at all.
        if !fds.is_empty() {
            let fits = control.push(SendAncillaryMessage::ScmRights(fds));
            debug_assert!(
                fits,
                "the space is for as many file descriptors as a message takes"
            );
        }
        let state = [IoSlice::new(state.as_bytes())];
        sendmsg_addr(
            &self.socket,
            &self.address,
            &state,
            &mut control,
            SendFlags::NOSIGNAL,
        )?;

        Ok(())
    }
}

/// Check that `name` is one that systemd takes for a stored file descriptor, and not empty.
///
/// A newline isn't printable, so a name can't add assignments of its own to the message.
pub(crate) fn check_name(name: &str) -> io::Result<()> {
    let printable = |b: u8| (b' '..=b'~').contains(&b) && b != b':';
    if name.is_empty() || name.len() > MAX_NAME_LEN || !name.bytes().all(printable) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "a file descriptor name is 1 to {MAX_NAME_LEN} printable ASCII characters, \
                 other than `:`"
            ),
        ));
    }

    Ok(())
}

/// How long `poll` is to wait until `deadline`: not at all once it has passed, and for ever if
/// there is none, or if what is left is too long for a `Timespec`.
fn remaining(deadline: Option<Instant>) -> Option<Timespec> {
    let left = deadline?.saturating_duration_since(Instant::now());

    Timespec::try_from(left).ok()
}

/// How many file descriptors fit in a message: `SCM_MAX_FD` of Linux, as unix(7) tells.
pub(crate) const MAX_FDS: usize = 253;

/// The longest name that systemd takes for a stored file descriptor.
pub(crate) const MAX_NAME_LEN: usize = 255;

#[cfg(test)]
mod tests {
    use std::{fs::remove_file, io::IoSliceMut, path::PathBuf, sync::mpsc::channel, thread};

    use rustix::{
        io::write,
        net::{
            RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, bind, recvmsg,
            sockopt::{Timeout, set_socket_timeout},
        },
    };

    use super::*;
    use crate::test_utils::{in_own_process, pipe, read_byte, unique_name, unique_path};

    #[test]
    fn valid_names() {
        let longest = "a".repeat(MAX_NAME_LEN);
        let printable = ('!'..='~').filter(|c| *c != ':').collect::<String>();
        for name in ["x", "busd/fds", "with spaces", &printable, &longest] {
            check_name(name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        }
    }

    #[test]
    fn invalid_names() {
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        let invalid = [
            "",
            "a:b",
            ":",
            "a\nb",
            "a\rb",
            "a\tb",
            "a\0b",
            "a\x1fb",
            "a\x7fb",
            "é",
            "日本",
            // Valid names, with the assignment of another message after them.
            "x\nFDSTOREREMOVE=1",
            &too_long,
        ];
        for name in invalid {
            assert_eq!(
                check_name(name).unwrap_err().kind(),
                ErrorKind::InvalidInput,
                "{name:?}"
            );
        }
    }

    #[test]
    fn notify() {
        for (manager, notifier) in managers() {
            notifier.notify("READY=1\nSTATUS=Up").unwrap();

            let (state, fds) = manager.receive();
            assert_eq!(state, "READY=1\nSTATUS=Up");
            assert!(fds.is_empty());
        }
    }

    #[test]
    fn store() {
        for (manager, notifier) in managers() {
            let (read_a, write_a) = pipe();
            let (read_b, write_b) = pipe();

            notifier
                .store("x", &[write_a.as_fd(), write_b.as_fd()])
                .unwrap();

            let (state, fds) = manager.receive();
            assert_eq!(state, "FDSTORE=1\nFDPOLL=0\nFDNAME=x");
            // These are the pipes that were sent, in the order they were sent.
            assert_eq!(fds.len(), 2);
            write(&fds[0], b"a").unwrap();
            write(&fds[1], b"b").unwrap();
            assert_eq!(read_byte(&read_a), b'a');
            assert_eq!(read_byte(&read_b), b'b');
        }
    }

    #[test]
    fn remove() {
        for (manager, notifier) in managers() {
            notifier.remove("x y").unwrap();

            let (state, fds) = manager.receive();
            assert_eq!(state, "FDSTOREREMOVE=1\nFDNAME=x y");
            assert!(fds.is_empty());
        }
    }

    #[test]
    fn invalid_names_are_not_sent() {
        for (manager, notifier) in managers() {
            let (_read, write) = pipe();

            let error = notifier.store("a:b", &[write.as_fd()]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            let error = notifier.remove("").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);

            // If either had been sent, the manager would get it before this.
            notifier.notify("READY=1").unwrap();
            let (state, fds) = manager.receive();
            assert_eq!(state, "READY=1");
            assert!(fds.is_empty());
        }
    }

    #[test]
    fn notify_the_most_file_descriptors() {
        for (manager, notifier) in managers() {
            let (_read, write) = pipe();
            let fd = write.as_fd();

            notifier.notify_with_fds("READY=1", &[fd; MAX_FDS]).unwrap();
            assert_eq!(manager.receive().1.len(), MAX_FDS);

            let error = notifier
                .notify_with_fds("READY=1", &[fd; MAX_FDS + 1])
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(Errno::TOOBIG.raw_os_error()));
            // The manager gets nothing of it: the next message is this one.
            notifier.notify("STATUS=Next").unwrap();
            assert_eq!(manager.receive().0, "STATUS=Next");
        }
    }

    #[test]
    fn notify_without_a_manager() {
        let notifier = Notifier::new(unique_path()).unwrap();

        let error = notifier.notify("READY=1").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        let error = notifier.barrier(Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn clones_reach_the_same_manager() {
        for (manager, notifier) in managers() {
            let clone = notifier.try_clone().unwrap();
            drop(notifier);

            clone.notify("READY=1").unwrap();
            assert_eq!(manager.receive().0, "READY=1");
        }
    }

    #[test]
    fn addresses() {
        // The socket of the notifier is not connected, so these don't need a manager.
        Notifier::new("/run/systemd/notify").unwrap();
        Notifier::new("@/org/freedesktop/systemd1/notify").unwrap();
        // The longest names that fit a socket address: 108 bytes for a path, 107 for an abstract
        // name.
        Notifier::new(format!("/{}", "a".repeat(107))).unwrap();
        Notifier::new(format!("@{}", "a".repeat(107))).unwrap();

        for address in ["/", "@"] {
            let error = Notifier::new(address).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{address:?}");
        }
        for address in [
            "",
            "x",
            "relative/path",
            "unix:/run/notify",
            "vsock:2:1234",
            "vsock-stream:2:1234",
        ] {
            let error = Notifier::new(address).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{address:?}");
        }
        for address in [
            format!("/{}", "a".repeat(108)),
            format!("@{}", "a".repeat(108)),
        ] {
            let error = Notifier::new(&address).unwrap_err();
            assert_eq!(
                error.raw_os_error(),
                Some(Errno::NAMETOOLONG.raw_os_error()),
                "{address:?}"
            );
        }
    }

    #[test]
    fn from_the_environment() {
        let exports = "NOTIFY_SOCKET=/run/systemd/notify";
        if !in_own_process("notifier::tests::from_the_environment", exports) {
            return;
        }

        assert!(Notifier::from_env().unwrap().is_some());
        // The variable stays, for the processes that this one starts to inherit.
        assert!(env::var_os("NOTIFY_SOCKET").is_some());

        // SAFETY: The test runs in a process of its own, which has no other thread that reads or
        // writes the environment. So do the ones below.
        unsafe { env::set_var("NOTIFY_SOCKET", "") };
        assert!(Notifier::from_env().unwrap().is_none());
        // SAFETY: As above.
        unsafe { env::remove_var("NOTIFY_SOCKET") };
        assert!(Notifier::from_env().unwrap().is_none());

        // SAFETY: As above.
        unsafe { env::set_var("NOTIFY_SOCKET", "vsock:2:1234") };
        let error = Notifier::from_env().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn barrier_waits_for_the_manager() {
        for (manager, notifier) in managers() {
            let delay = Duration::from_millis(200);
            let manager = thread::spawn(move || {
                let (state, fds) = manager.receive();
                // Take the time to deal with the message, then close its file descriptor.
                thread::sleep(delay);
                (state, fds.len())
            });

            let start = Instant::now();
            notifier.barrier(Duration::from_secs(30)).unwrap();
            assert!(start.elapsed() >= delay, "{:?}", start.elapsed());

            assert_eq!(manager.join().unwrap(), ("BARRIER=1".to_owned(), 1));
        }
    }

    #[test]
    fn barrier_times_out() {
        for (manager, notifier) in managers() {
            let (release, released) = channel::<()>();
            let manager = thread::spawn(move || {
                let (state, fds) = manager.receive();
                // Keep the file descriptor until the barrier gave up on it.
                let _ = released.recv();
                (state, fds.len())
            });

            let timeout = Duration::from_millis(200);
            let start = Instant::now();
            let error = notifier.barrier(timeout).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::TimedOut);
            assert!(start.elapsed() >= timeout, "{:?}", start.elapsed());

            release.send(()).unwrap();
            assert_eq!(manager.join().unwrap(), ("BARRIER=1".to_owned(), 1));
        }
    }

    /// A manager at a socket file, and another at an abstract socket, with a notifier for each.
    fn managers() -> [(Manager, Notifier); 2] {
        [Manager::at_path(), Manager::at_abstract_name()]
    }

    /// A socket that a notifier sends to: the notification socket of the manager.
    struct Manager {
        socket: OwnedFd,
        /// The file of the socket, if it's not an abstract one.
        path: Option<PathBuf>,
    }

    impl Manager {
        fn at_path() -> (Self, Notifier) {
            let path = unique_path();
            let address = SocketAddrUnix::new(&path).unwrap();
            let manager = Self::bind(&address, Some(path.clone()));

            (manager, Notifier::new(&path).unwrap())
        }

        fn at_abstract_name() -> (Self, Notifier) {
            let name = unique_name();
            let address = SocketAddrUnix::new_abstract_name(name.as_bytes()).unwrap();
            let manager = Self::bind(&address, None);

            (manager, Notifier::new(format!("@{name}")).unwrap())
        }

        fn bind(address: &SocketAddrUnix, path: Option<PathBuf>) -> Self {
            let socket = socket_with(
                AddressFamily::UNIX,
                SocketType::DGRAM,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            bind(&socket, address).unwrap();
            // Fail, rather than wait for ever, for a message that never comes.
            set_socket_timeout(&socket, Timeout::Recv, Some(Duration::from_secs(10))).unwrap();

            Self { socket, path }
        }

        /// The next message: its text, and the file descriptors that come with it.
        fn receive(&self) -> (String, Vec<OwnedFd>) {
            let mut text = [0; 8192];
            let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let message = recvmsg(
                &self.socket,
                &mut [IoSliceMut::new(&mut text)],
                &mut control,
                RecvFlags::CMSG_CLOEXEC,
            )
            .unwrap();
            assert!(
                !message
                    .flags
                    .intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC)
            );

            let mut fds = Vec::new();
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(received) = message {
                    fds.extend(received);
                }
            }

            (
                String::from_utf8(text[..message.bytes].to_vec()).unwrap(),
                fds,
            )
        }
    }

    impl Drop for Manager {
        fn drop(&mut self) {
            if let Some(path) = &self.path {
                let _ = remove_file(path);
            }
        }
    }
}
