//! A fake service manager, for the tests of the restarts of services: [`Manager`].
//!
//! This is behind the `testing` cargo feature.

use std::{
    collections::VecDeque,
    env,
    ffi::OsStr,
    fmt,
    fs::remove_file,
    io::{self, ErrorKind, IoSliceMut},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd, RawFd},
        unix::{net::UnixDatagram, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rustix::{
    cmsg_space,
    io::fcntl_dupfd_cloexec,
    net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg},
};

use crate::{Notifier, Passed, notifier::MAX_FDS, passed::START};

/// A service manager for tests, which runs a service as systemd does, with the sockets of socket
/// activation and a file descriptor store, without systemd.
///
/// It binds a notification socket of its own, in the temporary directory, and serves it on a
/// thread of its own, as systemd serves `$NOTIFY_SOCKET`. It keeps what a service stores, as
/// systemd does: one file descriptor of each open file description, under the name that the first
/// came with, and as many as its store takes ([`set_store_max`](Self::set_store_max)), closing the
/// others. It removes from the store what a service removes, and closes the pipe of a barrier,
/// which lets the service go on. It keeps the text of every notification, for the test to wait for
/// ([`wait_for`](Self::wait_for)).
///
/// [`command`](Self::command) starts a service as systemd starts one, with the sockets of socket
/// activation that the test gives the manager ([`add_socket`](Self::add_socket)) and what is in
/// the store, so that a test restarts a service by stopping it and starting it again. For a
/// service that runs in the test process, [`notifier`](Self::notifier) and
/// [`passed`](Self::passed) give what [`Notifier::from_env`] and [`Passed::from_env`] would.
pub struct Manager {
    /// The path of the notification socket.
    path: PathBuf,
    /// What the thread that serves the socket shares with the manager.
    shared: Arc<Shared>,
    serving: Option<JoinHandle<()>>,
    /// The sockets of socket activation, with their names.
    activation: Vec<(String, OwnedFd)>,
}

impl Manager {
    /// A manager whose store takes 16 file descriptors, and that has no socket of socket
    /// activation yet.
    pub fn new() -> io::Result<Self> {
        let path = env::temp_dir().join(unique_name());
        let socket = UnixDatagram::bind(&path)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                store_max: DEFAULT_STORE_MAX,
                ..State::default()
            }),
            changed: Condvar::new(),
        });
        let serving = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("zoter manager".to_owned())
                .spawn(move || serve(&socket, &shared))
        };
        let serving = match serving {
            Ok(serving) => serving,
            Err(e) => {
                let _ = remove_file(&path);

                return Err(e);
            }
        };

        Ok(Self {
            path,
            shared,
            serving: Some(serving),
            activation: Vec::new(),
        })
    }

    /// The path of the notification socket, as `$NOTIFY_SOCKET` names it.
    pub fn notify_socket(&self) -> &Path {
        &self.path
    }

    /// A notifier of this manager, as [`Notifier::from_env`] gives one to the services that it
    /// starts.
    pub fn notifier(&self) -> io::Result<Notifier> {
        Notifier::new(&self.path)
    }

    /// Have the store take `max` file descriptors, as `FileDescriptorStoreMax=` has systemd do.
    ///
    /// The services that the manager starts from then on get it as `$FDSTORE`. What the store
    /// keeps already stays.
    pub fn set_store_max(&mut self, max: usize) {
        self.shared.state().store_max = max;
    }

    /// How many file descriptors the store takes.
    pub fn store_max(&self) -> usize {
        self.shared.state().store_max
    }

    /// Pass `socket` to the services that the manager starts, under `name`, as a socket of socket
    /// activation, which the manager keeps open while they come and go.
    pub fn add_socket(&mut self, name: &str, socket: OwnedFd) {
        self.activation.push((name.to_owned(), socket));
    }

    /// Take the sockets of socket activation back, as when their units go.
    pub fn take_sockets(&mut self) -> Vec<(String, OwnedFd)> {
        std::mem::take(&mut self.activation)
    }

    /// What systemd would pass a service that it starts now: duplicates of the sockets of socket
    /// activation, then of what is in the store, with their names, as [`Passed::from_env`] gives
    /// them.
    pub fn passed(&self) -> io::Result<Passed> {
        let state = self.shared.state();
        let fds = self
            .activation
            .iter()
            .chain(&state.store)
            .map(|(name, fd)| Ok((name.clone(), fd.try_clone()?)))
            .collect::<io::Result<_>>()?;

        Ok(Passed::new(fds))
    }

    /// A command that runs `program` as systemd runs a service.
    ///
    /// Arguments added to the command go to `program`. It runs with what [`passed`](Self::passed)
    /// gives as its file descriptors from 3 on, with `$LISTEN_FDS`, `$LISTEN_FDNAMES` and
    /// `$LISTEN_PID` telling of them, and with `$NOTIFY_SOCKET` naming the socket of the manager,
    /// and `$FDSTORE` telling how many file descriptors the store takes. A shell runs `program` as
    /// itself, so that `$LISTEN_PID` is the PID of `program`, which can't be known before it
    /// starts. The command keeps duplicates of the file descriptors to pass until it is dropped.
    pub fn command(&self, program: impl AsRef<OsStr>) -> io::Result<Command> {
        let passed = self.passed()?.into_vec();
        let count = passed.len();
        // The file descriptors may be among those they are to be passed as, so that passing one
        // would close another before it's passed. Copies out of that way, which close on exec, are
        // what is passed instead.
        let first_free = RawFd::try_from(count)
            .ok()
            .and_then(|count| START.checked_add(count))
            .ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
        let mut names = Vec::with_capacity(count);
        let mut copies = Vec::with_capacity(count);
        for (name, fd) in passed {
            names.push(name);
            copies.push(fcntl_dupfd_cloexec(&fd, first_free)?);
        }
        let sources: Vec<RawFd> = copies.iter().map(AsRawFd::as_raw_fd).collect();

        let mut command = Command::new("sh");
        command
            .args(["-c", r#"LISTEN_PID=$$ exec "$0" "$@""#])
            .arg(program);
        // What the tests run with must not tell the service of other file descriptors, or of
        // another manager.
        for var in [
            "LISTEN_PID",
            "LISTEN_PIDFDID",
            "LISTEN_FDS",
            "LISTEN_FDNAMES",
        ] {
            command.env_remove(var);
        }
        command
            .env("NOTIFY_SOCKET", &self.path)
            .env("FDSTORE", self.store_max().to_string());
        if count > 0 {
            command
                .env("LISTEN_FDS", count.to_string())
                .env("LISTEN_FDNAMES", names.join(":"));
        }
        // SAFETY: The closure runs in the child, between `fork` and `exec`, where only
        // async-signal-safe functions may be called: it calls `dup2`, and `last_os_error` if that
        // fails, which reads `errno` and neither allocates nor takes a lock. It owns the copies,
        // which are open in the child, as the command holds them, and so the closure, for as long
        // as it may spawn. They are all `first_free` or above, so none is among the targets, 3 up
        // to that, and no `dup2` closes a source that another has yet to copy. A target that is
        // open is closed, but only in the child, whose copy nothing uses but for `std`'s pipe for
        // reporting a failed `exec`. `dup2` clears the close-on-exec flag of the target, which
        // keeps it open across `exec`. The copies stay close-on-exec, so they close then.
        unsafe {
            command.pre_exec(move || {
                let _copies = &copies;
                for (target, source) in (START..).zip(&sources) {
                    if libc::dup2(*source, target) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }

                Ok(())
            });
        }

        Ok(command)
    }

    /// Wait, `timeout` at most, for a notification that has `state` as one of its lines, such as
    /// `READY=1`, and let go of those that came before it.
    ///
    /// Each notification is waited for once: a second wait for `READY=1` waits for another one. It
    /// fails with [`ErrorKind::TimedOut`] if none comes in time.
    pub fn wait_for(&self, state: &str, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.shared.state();
        loop {
            while let Some(notification) = guard.notifications.pop_front() {
                if notification.lines().any(|line| line == state) {
                    return Ok(());
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("no `{state}` came in time"),
                ));
            }
            guard = self
                .shared
                .changed
                .wait_timeout(guard, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The names of what is in the store, in the order that it was stored.
    pub fn stored_names(&self) -> Vec<String> {
        let state = self.shared.state();

        state.store.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Duplicates of what is in the store, with their names, in the order that it was stored.
    pub fn stored(&self) -> io::Result<Vec<(String, OwnedFd)>> {
        let state = self.shared.state();

        state
            .store
            .iter()
            .map(|(name, fd)| Ok((name.clone(), fd.try_clone()?)))
            .collect()
    }

    /// Put `fds` in the store in place of what it holds, as for a test to hand a service over what
    /// no service stored.
    pub fn replace_store(&self, fds: Vec<(String, OwnedFd)>) {
        self.shared.state().store = fds;
    }

    /// The names that services removed from the store since this was last called, in the order
    /// that they were removed.
    pub fn take_removed(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.state().removed)
    }

    /// Close what is in the store, as systemd does once a service stops for good.
    pub fn clear_store(&self) {
        self.shared.state().store.clear();
    }
}

impl fmt::Debug for Manager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manager")
            .field("path", &self.path)
            .field("activation", &self.activation)
            .field("stored", &self.stored_names())
            .finish_non_exhaustive()
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.shared.state().stopping = true;
        // Wake the thread up, as it may be waiting for a notification.
        if let Ok(waking) = UnixDatagram::unbound() {
            let _ = waking.send_to(&[], &self.path);
        }
        if let Some(serving) = self.serving.take() {
            let _ = serving.join();
        }
        let _ = remove_file(&self.path);
    }
}

/// What the thread that serves the notification socket shares with the manager.
struct Shared {
    state: Mutex<State>,
    /// Notified when a notification comes.
    changed: Condvar,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
struct State {
    /// The file descriptor store, with the names that each file descriptor was stored under.
    store: Vec<(String, OwnedFd)>,
    /// How many file descriptors the store takes.
    store_max: usize,
    /// The names removed from the store, that the test didn't take yet.
    removed: Vec<String>,
    /// The text of each notification that the test didn't wait for yet, oldest first.
    notifications: VecDeque<String>,
    /// Whether the manager is going, and the thread is to stop.
    stopping: bool,
}

/// Serve `socket`, the notification socket, until the manager goes.
fn serve(socket: &UnixDatagram, shared: &Shared) {
    loop {
        let received = receive(socket);
        let mut state = shared.state();
        if state.stopping {
            return;
        }
        let Ok((text, fds)) = received else {
            continue;
        };
        process(&mut state, &text, fds);
        state.notifications.push_back(text);
        drop(state);
        shared.changed.notify_all();
    }
}

/// Deal with the notification `text`, and the file descriptors that came with it, as systemd does.
///
/// It keeps those that it stores, and closes the others: the pipe of a barrier among them, after
/// the notifications that came before it, which lets the service go on.
fn process(state: &mut State, text: &str, fds: Vec<OwnedFd>) {
    let assignments: Vec<_> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let value = |key: &str| {
        assignments
            .iter()
            .find(|(assigned, _)| *assigned == key)
            .map(|(_, value)| *value)
    };
    let name = value("FDNAME");
    if value("FDSTORE") == Some("1") {
        let name = name.unwrap_or("stored");
        for fd in fds {
            let kept = state.store.iter().any(|(_, stored)| same_file(stored, &fd));
            if !kept && state.store.len() < state.store_max {
                state.store.push((name.to_owned(), fd));
            }
        }
    } else if value("FDSTOREREMOVE") == Some("1")
        && let Some(name) = name
    {
        state.store.retain(|(stored, _)| stored != name);
        state.removed.push(name.to_owned());
    }
}

/// The next notification that `socket` gets, with the file descriptors that come with it.
fn receive(socket: &UnixDatagram) -> io::Result<(String, Vec<OwnedFd>)> {
    let mut text = [0; MAX_NOTIFICATION];
    let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut text)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    // Every file descriptor is taken, so that none stays open in the kernel.
    let fds = control
        .drain()
        .filter_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten()
        .collect();
    let text = String::from_utf8_lossy(&text[..received.bytes]).into_owned();

    Ok((text, fds))
}

/// Whether `a` and `b` are of the same open file description, as systemd tells through `kcmp(2)`,
/// if the kernel lets the process call that.
fn same_file(a: &OwnedFd, b: &OwnedFd) -> bool {
    // SAFETY: `getpid` has no preconditions.
    let pid = libc::c_long::from(unsafe { libc::getpid() });
    // SAFETY: The call reads and writes no memory, and takes each of its arguments as a long.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid,
            pid,
            KCMP_FILE,
            libc::c_long::from(a.as_fd().as_raw_fd()),
            libc::c_long::from(b.as_fd().as_raw_fd()),
        )
    };

    order == 0
}

/// A name that no other manager uses, of this process or another.
fn unique_name() -> String {
    // Managers can be made on several threads at once.
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    format!(
        "zoter-manager-{}-{}",
        process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// How many file descriptors the store takes unless set otherwise.
const DEFAULT_STORE_MAX: usize = 16;
/// The longest notification that systemd takes.
const MAX_NOTIFICATION: usize = 4096;
/// What `kcmp(2)` compares of two file descriptors: their open file descriptions.
const KCMP_FILE: libc::c_long = 0;

#[cfg(test)]
mod tests {
    use std::{os::fd::AsFd, process::Stdio};

    use rustix::io::write;

    use super::*;
    use crate::test_utils::{pipe, read_byte};

    #[test]
    fn store_and_remove() {
        let manager = Manager::new().unwrap();
        let notifier = manager.notifier().unwrap();
        let (read, write_end) = pipe();

        notifier.store("a", &[write_end.as_fd()]).unwrap();
        // Once of each open file description, under the name that it first came with.
        notifier.store("b", &[write_end.as_fd()]).unwrap();
        notifier.store("b", &[pipe().1.as_fd()]).unwrap();
        notifier.barrier(WAIT).unwrap();
        assert_eq!(manager.stored_names(), ["a", "b"]);
        let (_, stored) = manager.stored().unwrap().remove(0);
        write(&stored, b"x").unwrap();
        assert_eq!(read_byte(&read), b'x');

        notifier.remove("b").unwrap();
        notifier.barrier(WAIT).unwrap();
        assert_eq!(manager.stored_names(), ["a"]);
        assert_eq!(manager.take_removed(), ["b"]);
        assert!(manager.take_removed().is_empty());

        manager.clear_store();
        assert!(manager.stored_names().is_empty());
    }

    #[test]
    fn store_takes_its_max() {
        let mut manager = Manager::new().unwrap();
        manager.set_store_max(2);
        let notifier = manager.notifier().unwrap();
        let fds: Vec<_> = (0..3).map(|_| pipe().1).collect();
        let borrowed: Vec<_> = fds.iter().map(AsFd::as_fd).collect();

        notifier.store("a", &borrowed).unwrap();
        notifier.barrier(WAIT).unwrap();
        assert_eq!(manager.stored_names(), ["a", "a"]);
    }

    #[test]
    fn wait_for_notifications() {
        let manager = Manager::new().unwrap();
        let notifier = manager.notifier().unwrap();

        notifier.notify("STATUS=Starting").unwrap();
        notifier.notify("READY=1\nSTATUS=Up").unwrap();
        manager.wait_for("READY=1", WAIT).unwrap();
        // Each notification is waited for once.
        let error = manager
            .wait_for("READY=1", Duration::from_millis(100))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
    }

    #[test]
    fn passed_as_systemd_passes_them() {
        let mut manager = Manager::new().unwrap();
        let notifier = manager.notifier().unwrap();
        manager.add_socket("example.socket", pipe().1);
        notifier.store("stored", &[pipe().1.as_fd()]).unwrap();
        notifier.barrier(WAIT).unwrap();

        let passed = manager.passed().unwrap();
        let names: Vec<_> = passed.iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["example.socket", "stored"]);
        assert_eq!(manager.take_sockets().len(), 1);
        assert_eq!(manager.passed().unwrap().len(), 1);
    }

    /// The service runs with its file descriptors from 3 on, and the variables that tell of them
    /// and of the manager.
    #[test]
    fn command() {
        let mut manager = Manager::new().unwrap();
        manager.set_store_max(4);
        manager.add_socket("first", pipe().1);
        manager.add_socket("second", pipe().1);
        let script = r#"
            test "$LISTEN_PID" = "$$" && test -e /proc/$$/fd/3 && test -e /proc/$$/fd/4 &&
            echo "$LISTEN_FDS $LISTEN_FDNAMES $FDSTORE $NOTIFY_SOCKET $1"
        "#;

        let output = manager
            .command("sh")
            .unwrap()
            .args(["-c", script, "sh", "arg"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let expected = format!(
            "2 first:second 4 {} arg\n",
            manager.notify_socket().display()
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }

    const WAIT: Duration = Duration::from_secs(10);
}
