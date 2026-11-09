//! What the unit tests share.

use std::{
    env,
    os::fd::{OwnedFd, RawFd},
    path::{Path, PathBuf},
    process::{self, Command},
    sync::atomic::{AtomicUsize, Ordering},
};

use rustix::{
    io::read,
    pipe::{PipeFlags, pipe_with},
};

/// The two ends of a pipe, read first.
pub(crate) fn pipe() -> (OwnedFd, OwnedFd) {
    pipe_with(PipeFlags::CLOEXEC).unwrap()
}

/// The next byte that `fd` gives.
pub(crate) fn read_byte(fd: &OwnedFd) -> u8 {
    let mut byte = [0];
    assert_eq!(read(fd, &mut byte).unwrap(), 1);

    byte[0]
}

/// A name that no other test, or run of the tests, uses.
pub(crate) fn unique_name() -> String {
    // The tests of a process run on threads of their own, at once.
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    format!(
        "zoter-{}-{}",
        process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A path in the temporary directory that no other test uses.
pub(crate) fn unique_path() -> PathBuf {
    env::temp_dir().join(unique_name())
}

/// Whether `fd` is open in this process.
pub(crate) fn is_open(fd: RawFd) -> bool {
    Path::new(&format!("/proc/self/fd/{fd}")).exists()
}

/// Whether this is the process that the test `test`, its path in the crate, runs itself in.
///
/// Some tests take file descriptors from 3 on, or change the environment, which no test can do
/// among the threads of the others, so each runs in a process of its own. A shell makes that
/// process the way that systemd makes a service: it opens the file descriptors 3 and 4, and
/// exports `exports`, in which `$$` is the PID of the process, which `exec` keeps. It closes 5,
/// which a test needs closed, as the process may inherit any descriptor that another test has open
/// at the time and that isn't close-on-exec.
///
/// In that process, this is true, and the test goes on. Anywhere else, it runs the test there,
/// and checks that it passed, and the test is done.
pub(crate) fn in_own_process(test: &str, exports: &str) -> bool {
    if env::var_os(OWN_PROCESS).is_some() {
        return true;
    }

    let script = format!(
        r#"export {exports}; exec "$0" --exact "$1" --nocapture 3</dev/null 4</dev/null 5<&-"#
    );
    let output = Command::new("sh")
        .args(["-c", &script])
        .arg(env::current_exe().unwrap())
        .arg(test)
        .env(OWN_PROCESS, "1")
        .output()
        .unwrap();
    // A process that finds no test of that name succeeds all the same.
    let passed = String::from_utf8_lossy(&output.stdout).contains(&format!("test {test} ... ok"));
    assert!(output.status.success() && passed, "{output:?}");

    false
}

const OWN_PROCESS: &str = "ZOTER_TEST_OWN_PROCESS";
