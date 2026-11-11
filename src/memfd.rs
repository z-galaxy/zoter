//! Memfds, which hold the record of a hand-over: written in place, sealed, and read in place.
//!
//! A memfd is a file in memory that has no name in any directory, which suits what a service
//! passes on to its next instance through the store. The seals keep anyone from resizing it or
//! writing to it, through any of its file descriptors, so the reader gets exactly what was written.

use std::{
    io,
    ops::Deref,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    ptr, slice, thread,
    time::{Duration, Instant},
};

use rustix::{
    buffer::spare_capacity,
    fs::{MemfdFlags, SealFlags, fcntl_add_seals, fcntl_get_seals, fstat, ftruncate, memfd_create},
    io::{Errno, pread, retry_on_intr},
    mm::{Advice, MapFlags, ProtFlags, madvise, mmap, munmap},
};

/// A sealed memfd of `len` bytes, that `fill` fills in place: it is given the bytes of the memfd,
/// zeros at first, mapped into memory.
///
/// The memory of the memfd is allocated before `fill` writes to it, so that a lack of memory fails
/// this, rather than kill the process as it writes, from Linux 5.14 on. It fails with what `fill`
/// fails with, and the memfd goes then. `name` is for debugging only: it shows as `memfd:<name>`
/// among the files of the process, and can be 249 bytes at most.
///
/// Children that other threads fork meanwhile, such as to spawn processes, don't inherit the
/// mapping. A child forked between the creation of the mapping and its `MADV_DONTFORK` still
/// inherits it. An inherited writable mapping prevents write sealing until the child unmaps it,
/// execs, or exits, so sealing waits for that, for 1 s at most.
pub(crate) fn filled<F, E>(name: &str, len: usize, fill: F) -> Result<OwnedFd, E>
where
    F: FnOnce(&mut [u8]) -> Result<(), E>,
    E: From<io::Error>,
{
    let fd = new(name)?;
    ftruncate(&fd, len as u64).map_err(io::Error::from)?;
    // No one can shrink the memfd under the mapping, which a write there would fault on, nor grow
    // it.
    let seals = SealFlags::SHRINK | SealFlags::GROW;
    fcntl_add_seals(&fd, seals).map_err(io::Error::from)?;
    {
        let mut mapping = Mapping::shared(fd.as_fd(), len)?;
        mapping.populate()?;
        fill(mapping.bytes_mut())?;
    }
    // The kernel refuses to seal a memfd against writes while a shared mapping of it is left, a
    // read-only one included, so the mapping went first.
    seal_writes(fd.as_fd())?;

    Ok(fd)
}

/// A sealed memfd that holds `bytes`, as [`filled`] makes one.
#[cfg(test)]
pub(crate) fn sealed(name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    filled(name, bytes.len(), |memfd| {
        memfd.copy_from_slice(bytes);

        Ok(())
    })
}

/// All of what the memfd `fd` holds, from its start, to read in place: mapped into memory, if the
/// memfd is sealed against shrinking and writing, as [`filled`] seals it, or else read, as
/// [`read`] reads it, from any file.
///
/// The seals keep anyone from changing what the mapping shows, which reading the memfd in place
/// takes. The mapping is private and read-only, which works however the memfd is sealed: a shared
/// one fails, on some kernels, through a file descriptor that can write to a memfd that is sealed
/// against writes, as those that the store of systemd gives back can.
pub(crate) fn view(fd: BorrowedFd<'_>) -> io::Result<View> {
    let kind = match Mapping::sealed(fd)? {
        Some(mapping) => Kind::Mapped(mapping),
        None => Kind::Read(read(fd)?),
    };

    Ok(View(kind))
}

/// All of what the file `fd` holds, from its start.
///
/// It reads at offsets of its own, and leaves the file offset of `fd` where it is, as that is
/// shared with every duplicate of `fd`.
pub(crate) fn read(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        bytes.reserve(READ_CHUNK);
        let offset = bytes.len() as u64;
        if retry_on_intr(|| pread(fd, spare_capacity(&mut bytes), offset))? == 0 {
            return Ok(bytes);
        }
    }
}

/// All of what a memfd holds, as [`view`] gives it.
#[derive(Debug)]
pub(crate) struct View(Kind);

impl Deref for View {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Kind::Mapped(mapping) => mapping.bytes(),
            Kind::Read(bytes) => bytes,
        }
    }
}

/// How a memfd is viewed: see [`view`].
#[derive(Debug)]
enum Kind {
    /// The memfd, mapped into memory.
    Mapped(Mapping),
    /// A copy of what the memfd holds.
    Read(Vec<u8>),
}

/// A mapping of the start of a memfd into the memory of the process, which goes as this is dropped.
///
/// That takes `unsafe` code, all of which is here: Rust can't tell that the memory stays mapped,
/// nor that nothing changes it under the slices that the mapping gives, which the memfd and its
/// seals see to.
#[derive(Debug)]
struct Mapping {
    /// Where the mapping starts.
    start: *mut u8,
    /// How long the mapping is, which a slice of it can be.
    len: SliceLen,
    /// Whether the mapping is shared and writable, rather than private and read-only.
    writable: bool,
}

// SAFETY: The mapping is the only way to its memory, as a `Box<[u8]>` is to its own, and gives out
// its bytes as a `&[u8]` from a `&self` alone, and as a `&mut [u8]` from a `&mut self` alone, so it
// can go to, and be shared with, another thread, as that can. Unmapping it on another thread is as
// good as on this one.
unsafe impl Send for Mapping {}
// SAFETY: As above.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// A shared mapping of the first `len` bytes of the memfd `fd`, to write to the memfd, which
    /// no other process has yet, nor anything else in this one, and which is sealed against
    /// shrinking, so that no one else changes what the mapping shows, and it stays as long.
    ///
    /// `MADV_DONTFORK` prevents children from inheriting this mapping. An inherited writable
    /// mapping would prevent write sealing until the child unmaps it, execs, or exits.
    fn shared(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        let mapping = Self::new(fd, len, true)?;
        let len = mapping.len.get();
        if len > 0 {
            // SAFETY: The advice covers only the memory of the mapping, and changes none of it in
            // this process. Children forked from now on don't have this memory. Only `fill` of
            // `filled` could reach it in such a child: it would have to fork through `unsafe` code
            // of its own, and then use the slice that it was lent.
            unsafe { madvise(mapping.start.cast(), len, Advice::LinuxDontFork) }?;
        }

        Ok(mapping)
    }

    /// A private mapping of all of the memfd `fd`, to read, if it is sealed against shrinking and
    /// writing, so that no one changes what the mapping shows, and it stays as long.
    fn sealed(fd: BorrowedFd<'_>) -> io::Result<Option<Self>> {
        let seals = SealFlags::SHRINK | SealFlags::WRITE;
        if !fcntl_get_seals(fd).is_ok_and(|sealed| sealed.contains(seals)) {
            return Ok(None);
        }
        Self::new(fd, fstat(fd)?.st_size, false).map(Some)
    }

    /// A mapping of the first `len` bytes of the memfd `fd`, shared and writable if `writable`, or
    /// else private and read-only. It fails with `EOVERFLOW` for more bytes than a slice can have.
    fn new<L>(fd: BorrowedFd<'_>, len: L, writable: bool) -> io::Result<Self>
    where
        L: TryInto<SliceLen, Error = Errno>,
    {
        let len = len.try_into()?;
        // `mmap(2)` maps no empty range, and there is nothing to map then.
        if len.get() == 0 {
            return Ok(Self {
                start: ptr::NonNull::dangling().as_ptr(),
                len,
                writable,
            });
        }
        let (prot, flags) = match writable {
            true => (ProtFlags::READ | ProtFlags::WRITE, MapFlags::SHARED),
            false => (ProtFlags::READ, MapFlags::PRIVATE),
        };
        // SAFETY: The kernel puts the mapping where nothing is mapped, as it is given no address.
        let start = unsafe { mmap(ptr::null_mut(), len.get(), prot, flags, fd, 0) }?;

        Ok(Self {
            start: start.cast(),
            len,
            writable,
        })
    }

    /// Have the kernel allocate the memory of the mapping now (`MADV_POPULATE_WRITE`), so that a
    /// lack of memory fails this, rather than a write to the mapping, which `SIGBUS` would kill
    /// the process on. A kernel before 5.14 lacks that, and allocates the memory as it is written,
    /// as without this.
    fn populate(&self) -> io::Result<()> {
        let len = self.len.get();
        if len == 0 {
            return Ok(());
        }
        // SAFETY: The advice is for the memory of the mapping, and changes none of it: the kernel
        // allocates what it maps, as a write would.
        match unsafe { madvise(self.start.cast(), len, Advice::LinuxPopulateWrite) } {
            Ok(()) | Err(Errno::INVAL) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The bytes that the mapping shows.
    fn bytes(&self) -> &[u8] {
        // SAFETY: The mapping is of `len` readable bytes from `start`, which a slice can have, as
        // `SliceLen` sees to, for as long as `self` lives, which the slice can't outlive, and
        // nothing changes them meanwhile: a shared mapping is of a memfd that no one else writes
        // to, and a private one of a memfd sealed against writes. Neither memfd can shrink, which
        // would leave part of the mapping without memory.
        unsafe { slice::from_raw_parts(self.start, self.len.get()) }
    }

    /// The bytes that a shared mapping shows, to write to the memfd.
    fn bytes_mut(&mut self) -> &mut [u8] {
        assert!(self.writable, "a read-only mapping to write to");
        // SAFETY: As for `bytes`, and the mapping is writable, and borrowed for as long as the
        // slice is.
        unsafe { slice::from_raw_parts_mut(self.start, self.len.get()) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let len = self.len.get();
        if len == 0 {
            return;
        }
        // SAFETY: The mapping is this one's, and no slice of it is left, as each borrows `self`.
        let unmapped = unsafe { munmap(self.start.cast(), len) };
        debug_assert!(unmapped.is_ok(), "failed to unmap a memfd: {unmapped:?}");
    }
}

/// A number of bytes that a slice can have: `isize::MAX` at most, which a mapping can be longer
/// than on a 32-bit system.
///
/// It converts from a length, and from the size of a file, as `fstat(2)` gives it, and fails with
/// `EOVERFLOW` for one that no slice can have.
#[derive(Debug, Clone, Copy)]
struct SliceLen(usize);

impl SliceLen {
    fn get(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for SliceLen {
    type Error = Errno;

    fn try_from(len: usize) -> Result<Self, Errno> {
        if isize::try_from(len).is_err() {
            return Err(Errno::OVERFLOW);
        }

        Ok(Self(len))
    }
}

impl TryFrom<i64> for SliceLen {
    type Error = Errno;

    fn try_from(size: i64) -> Result<Self, Errno> {
        let len = usize::try_from(size).map_err(|_| Errno::OVERFLOW)?;

        Self::try_from(len)
    }
}

/// A memfd named `name`, empty, that can be sealed, and closes on exec.
fn new(name: &str) -> io::Result<OwnedFd> {
    let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING;
    // With `vm.memfd_noexec=2`, the kernel refuses a memfd that can be made executable, and a
    // kernel before 6.3 doesn't know the flag that prevents that. This is what systemd does about
    // both.
    match memfd_create(name, flags | MemfdFlags::NOEXEC_SEAL) {
        Err(Errno::INVAL) => Ok(memfd_create(name, flags)?),
        fd => Ok(fd?),
    }
}

/// Seal the memfd `fd` against writes, and against more seals.
///
/// The kernel refuses with `EBUSY` while any writable shared mapping of the memfd exists. A child
/// that inherited one keeps it until it unmaps it, execs, or exits. On `EBUSY`, this tries again
/// until [`SEAL_TIMEOUT`] has passed.
fn seal_writes(fd: BorrowedFd<'_>) -> io::Result<()> {
    let seals = SealFlags::WRITE | SealFlags::SEAL;
    let deadline = Instant::now() + SEAL_TIMEOUT;
    loop {
        match fcntl_add_seals(fd, seals) {
            Err(Errno::BUSY) if Instant::now() < deadline => thread::sleep(SEAL_RETRY),
            sealed => return Ok(sealed?),
        }
    }
}

/// How much room [`read`] makes for the next read, at least.
const READ_CHUNK: usize = 8192;
/// How long [`seal_writes`] keeps trying, at most.
const SEAL_TIMEOUT: Duration = Duration::from_secs(1);
/// How long [`seal_writes`] waits between tries.
const SEAL_RETRY: Duration = Duration::from_millis(1);

#[cfg(test)]
mod tests {
    use std::{fs::File, io::ErrorKind, os::fd::AsRawFd};

    use rustix::{
        io::{FdFlags, fcntl_getfd, pwrite, write},
        process::{Pid, WaitOptions, waitpid},
    };

    use super::*;
    use crate::test_utils::{in_own_process, pipe, read_byte};

    #[test]
    fn round_trip() {
        for len in [0, 5, 3 * READ_CHUNK + 1] {
            let bytes: Vec<_> = (0..=u8::MAX).cycle().take(len).collect();
            let filling = |memfd: &mut [u8]| {
                assert!(memfd.iter().all(|&byte| byte == 0), "the memfd isn't empty");
                memfd.copy_from_slice(&bytes);

                Ok::<_, io::Error>(())
            };
            let fd = filled("zoter-test", len, filling).unwrap();

            assert_eq!(read(fd.as_fd()).unwrap(), bytes);
            assert!(fcntl_getfd(&fd).unwrap().contains(FdFlags::CLOEXEC));
            let seals = SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL;
            assert!(fcntl_get_seals(&fd).unwrap().contains(seals));
        }
    }

    #[test]
    fn sealed_against_changes() {
        let fd = sealed("zoter-test", b"state").unwrap();

        assert_eq!(write(&fd, b"x").unwrap_err(), Errno::PERM);
        assert_eq!(pwrite(&fd, b"x", 0).unwrap_err(), Errno::PERM);
        assert_eq!(ftruncate(&fd, 0).unwrap_err(), Errno::PERM);
        assert_eq!(ftruncate(&fd, 1 << 20).unwrap_err(), Errno::PERM);
        // The seals themselves are sealed.
        assert_eq!(
            fcntl_add_seals(&fd, SealFlags::FUTURE_WRITE).unwrap_err(),
            Errno::PERM
        );
        assert_eq!(read(fd.as_fd()).unwrap(), b"state");
    }

    #[test]
    fn filled_fails_as_it_is_filled() {
        let failing = |_: &mut [u8]| Err(io::Error::from(ErrorKind::InvalidData));

        let error = filled("zoter-test", 8, failing).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn view_maps_sealed_memfds() {
        for bytes in [&b""[..], b"state"] {
            let fd = sealed("zoter-test", bytes).unwrap();

            let view = view(fd.as_fd()).unwrap();
            assert!(matches!(view.0, Kind::Mapped(_)), "{view:?}");
            assert_eq!(&*view, bytes);
        }
    }

    /// A mapping longer than a slice can be is refused before anything is mapped, as that of a
    /// sealed memfd of 2 GiB or more would be on a 32-bit system, and so is one of a size that no
    /// file has.
    #[test]
    fn mapping_longer_than_a_slice_is_refused() {
        let fd = sealed("zoter-test", b"state").unwrap();
        for writable in [false, true] {
            let e = Mapping::new(fd.as_fd(), isize::MAX.unsigned_abs() + 1, writable).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(Errno::OVERFLOW.raw_os_error()));
        }
        let e = Mapping::new(fd.as_fd(), -1_i64, false).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(Errno::OVERFLOW.raw_os_error()));
    }

    #[test]
    fn view_reads_other_files() {
        // A memfd that isn't sealed, as one of another process may not be.
        let fd = memfd_create("zoter-test", MemfdFlags::CLOEXEC).unwrap();
        assert_eq!(pwrite(&fd, b"state", 0).unwrap(), 5);
        let view = view(fd.as_fd()).unwrap();
        assert!(matches!(view.0, Kind::Read(_)), "{view:?}");
        assert_eq!(&*view, b"state");

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/memfd.rs");
        let file = File::open(path).unwrap();
        let view = super::view(file.as_fd()).unwrap();
        assert!(matches!(view.0, Kind::Read(_)), "{view:?}");
        assert_eq!(&*view, std::fs::read(path).unwrap());
    }

    #[test]
    fn name_too_long() {
        assert!(sealed(&"a".repeat(249), b"").is_ok());
        assert_eq!(
            sealed(&"a".repeat(250), b"").unwrap_err().raw_os_error(),
            Some(Errno::INVAL.raw_os_error())
        );
    }

    #[test]
    fn read_from_the_start() {
        let fd = sealed("zoter-test", b"state").unwrap();

        // The offset of the file descriptor is somewhere else than the start, and stays there.
        read_byte(&fd);
        assert_eq!(read(fd.as_fd()).unwrap(), b"state");
        assert_eq!(read_byte(&fd), b't');
    }

    #[test]
    fn read_any_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/memfd.rs");
        let file = File::open(path).unwrap();

        let bytes = read(file.as_fd()).unwrap();
        assert!(bytes.len() > READ_CHUNK);
        assert_eq!(bytes, std::fs::read(path).unwrap());
    }

    /// A child forked while the memfd is filled doesn't inherit the mapping, so sealing succeeds
    /// while the child is still alive. The child waits before it exits, as a child that spawns a
    /// process may wait before it execs.
    #[test]
    fn sealed_with_a_child_forked_as_it_was_filled() {
        let test = "memfd::tests::sealed_with_a_child_forked_as_it_was_filled";
        if !in_own_process(test, "ZOTER_TEST_FORK=1") {
            return;
        }

        let (wait, release) = pipe();
        let mut child = None;
        let filled = filled("zoter-test", 8, |memfd: &mut [u8]| {
            memfd.fill(1);
            child = Some(fork_waiting(&wait, &release));

            Ok::<_, io::Error>(())
        });
        drop(release);
        reap(child.unwrap());

        let fd = filled.unwrap();
        assert!(fcntl_get_seals(&fd).unwrap().contains(SealFlags::WRITE));
        assert_eq!(read(fd.as_fd()).unwrap(), [1; 8]);
    }

    /// A child that inherited a writable mapping of the memfd prevents write sealing until it
    /// exits, and sealing waits for that. A child forked right before the mapping got its
    /// `MADV_DONTFORK` is such a child.
    #[test]
    fn sealing_waits_for_a_child_with_a_copy() {
        let test = "memfd::tests::sealing_waits_for_a_child_with_a_copy";
        if !in_own_process(test, "ZOTER_TEST_FORK=1") {
            return;
        }

        let fd = new("zoter-test").unwrap();
        ftruncate(&fd, 8).unwrap();
        let (wait, release) = pipe();
        // Unlike the mapping of `Mapping::shared`, this one has no `MADV_DONTFORK`, so the child
        // inherits it.
        let mapping = Mapping::new(fd.as_fd(), 8_usize, true).unwrap();
        let child = fork_waiting(&wait, &release);
        drop(mapping);
        // The child is released after a while, so that sealing first finds it still alive.
        let releasing = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(release);
        });
        let sealed = seal_writes(fd.as_fd());
        releasing.join().unwrap();
        reap(child);

        sealed.unwrap();
        assert!(fcntl_get_seals(&fd).unwrap().contains(SealFlags::WRITE));
    }

    /// Fork a child that waits until `release` is closed, and then exits. The child reads from
    /// `wait`, the other end of the pipe. It closes its own copy of `release` first, so it exits as
    /// soon as the test closes its copy, even if the test fails.
    fn fork_waiting(wait: &OwnedFd, release: &OwnedFd) -> Pid {
        let (wait, release) = (wait.as_raw_fd(), release.as_raw_fd());
        // SAFETY: The child calls only `close`, `read` and `_exit`. These are async-signal-safe,
        // which is all that the child of a process with several threads may call.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            let mut byte = 0_u8;
            // SAFETY: As above. `read` reads at most 1 byte, into `byte`.
            unsafe {
                libc::close(release);
                libc::read(wait, (&raw mut byte).cast(), 1);
                libc::_exit(0);
            }
        }

        assert!(pid > 0, "{}", io::Error::last_os_error());

        Pid::from_raw(pid).unwrap()
    }

    /// Wait for the child `pid` to exit, and check that it exited with 0.
    fn reap(pid: Pid) {
        let (_, status) = waitpid(Some(pid), WaitOptions::empty()).unwrap().unwrap();
        assert_eq!(status.exit_status(), Some(0), "{status:?}");
    }
}
