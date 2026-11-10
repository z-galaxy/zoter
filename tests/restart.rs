//! A service that restarts the way that systemd restarts it, through its file descriptor store,
//! with its client connected throughout.
//!
//! The test is the service manager, through `zoter::testing::Manager`, and the test binary is the
//! service too: the test runs itself as the service, with `ROLE` set.
#![cfg(all(target_os = "linux", feature = "testing"))]

use std::{
    env,
    io::{self, BufRead, BufReader, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Child, Stdio},
    time::Duration,
};

use zoter::{Passed, Store, testing::Manager};

/// A client connects to a service, which hands it over to its next instance as it stops, with the
/// count of the requests that it answered. While no instance runs, the client sends its next
/// request, which waits in its socket, and the next instance answers it on the same connection,
/// with the count that it took over, and removes what it took from the store.
#[test]
fn restart() {
    if env::var_os(ROLE).is_some() {
        return serve();
    }

    let mut manager = Manager::new().unwrap();
    let path = env::temp_dir().join(format!("zoter-restart-{}", std::process::id()));
    let _removed = Removed(path.clone());
    let listener = UnixListener::bind(&path).unwrap();
    manager.add_socket(SOCKET, OwnedFd::from(listener));

    let mut first = spawn(&manager);
    manager.wait_for("READY=1", WAIT).unwrap();
    let mut client = UnixStream::connect(&path).unwrap();
    client.set_read_timeout(Some(WAIT)).unwrap();
    let mut replies = BufReader::new(client.try_clone().unwrap());
    client.write_all(b"a\n").unwrap();
    assert_eq!(read_line(&mut replies), "1:a");
    stop(&mut first);
    assert_eq!(manager.stored_names(), ["echo/state", "echo/fds"]);

    client.write_all(b"b\n").unwrap();
    let mut second = spawn(&manager);
    manager.wait_for("READY=1", WAIT).unwrap();
    assert_eq!(read_line(&mut replies), "2:b");
    assert!(manager.stored_names().is_empty());
    assert_eq!(manager.take_removed(), ["echo/fds", "echo/state"]);
    stop(&mut second);
}

/// Serve as the service: take over from the previous instance, if any, or else accept a client,
/// and answer its request with the count of those answered. Then hand the client over to the next
/// instance once the standard input ends, as this one stops.
fn serve() {
    // SAFETY: The test runs in a process of its own, whose other threads don't read or write the
    // environment, and where nothing took the file descriptors that the manager passed.
    let mut passed = unsafe { Passed::from_env() }.unwrap();
    let mut store = Store::from_env(KEY).unwrap().unwrap();
    let restored = store.take_over(&mut passed).unwrap();
    let listener = UnixListener::from(passed.take(SOCKET).unwrap());
    let (mut count, client) = match restored {
        Some(mut restored) => {
            let count = u64::from_le_bytes(restored.state().try_into().unwrap());
            let item = restored.take_items().next().unwrap();
            assert_eq!(item.id, CLIENT);

            (count, UnixStream::from(item.restore::<OwnedFd>().unwrap()))
        }
        None => {
            store.notifier().notify("READY=1").unwrap();

            (0, listener.accept().unwrap().0)
        }
    };
    if count > 0 {
        store.notifier().notify("READY=1").unwrap();
    }

    let mut request = String::new();
    BufReader::new(&client).read_line(&mut request).unwrap();
    count += 1;
    writeln!(&client, "{count}:{}", request.trim_end()).unwrap();

    // The standard input ends as the test stops this instance.
    io::stdin().read_to_end(&mut Vec::new()).unwrap();
    let handover = store.handover().unwrap();
    let state = count.to_le_bytes();
    let stored = handover
        .store(&state[..], [(CLIENT, client.as_fd())])
        .unwrap();
    assert_eq!((stored.items(), stored.fds()), (1, 1));
}

/// Start the service, as the manager does.
fn spawn(manager: &Manager) -> Child {
    manager
        .command(env::current_exe().unwrap())
        .unwrap()
        .args(["--exact", "restart", "--nocapture"])
        .env(ROLE, "service")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

/// Stop the service, which hands over, and check that it went well.
fn stop(service: &mut Child) {
    drop(service.stdin.take());
    let status = service.wait().unwrap();
    assert!(status.success(), "{status}");
}

/// The next line that `replies` gives, without its end.
fn read_line(replies: &mut BufReader<UnixStream>) -> String {
    let mut line = String::new();
    replies.read_line(&mut line).unwrap();

    line.trim_end().to_owned()
}

/// A file that is removed when dropped.
struct Removed(PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// What tells the test binary to serve as the service.
const ROLE: &str = "ZOTER_TEST_ROLE";
/// The name of the socket of socket activation.
const SOCKET: &str = "echo.socket";
/// The key that the service hands over under.
const KEY: &str = "echo";
/// The id of the client among the items of the hand-over.
const CLIENT: u64 = 7;
/// How long the test waits for what should come soon, rather than hang if it never does.
const WAIT: Duration = Duration::from_secs(10);
