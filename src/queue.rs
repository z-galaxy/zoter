//! Sockets whose queues hold file descriptors: [`Queue`], which takes any number of them through
//! the store as one.

use std::{
    io::{self, ErrorKind, IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{BorrowedFd, OwnedFd},
};

use rustix::{
    cmsg_space,
    io::{Errno, retry_on_intr},
    net::{
        AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType, recvmsg,
        sendmsg, socketpair,
    },
};

use crate::notifier::MAX_FDS;

/// A unix socket that holds file descriptors in its queue, and so takes any number of them through
/// the store as one: [`push`](Self::push) them into it, store the socket that
/// [`into_socket`](Self::into_socket) gives, and [`take`] takes them out of it in the next
/// instance of the service.
///
/// systemd passes the next instance the name of each file descriptor that it passes in one
/// environment variable, `$LISTEN_FDNAMES`, of which Linux takes 128 KiB at most
/// (`MAX_ARG_STRLEN`): it fails to start the next instance with more, as it does with more file
/// descriptors than the soft limit of open files of the service takes, and it keeps no more than
/// `FileDescriptorStoreMax=` of them. A file descriptor that comes with a datagram over a unix
/// socket stays in the queue of the socket until someone receives the datagram, whoever holds the
/// socket meanwhile, so the store keeps those of the queue with the socket, as one. Unlike the
/// store, which keeps one file descriptor of each open file description, the queue keeps each as it
/// is.
///
/// The queue takes as many datagrams as the send buffer of the socket has room for: a few hundred
/// of [`MAX_FDS`] file descriptors each, some 70000 of them, with the size that Linux gives it by
/// default.
#[derive(Debug)]
pub(crate) struct Queue {
    /// The end that the file descriptors are pushed into.
    sending: OwnedFd,
    /// The end whose queue holds them.
    queue: OwnedFd,
}

impl Queue {
    /// An empty queue.
    pub(crate) fn new() -> io::Result<Self> {
        // A socket of sequenced packets keeps the datagrams apart, and in order, and tells that the
        // queue ended once the end that they were pushed into is closed.
        let (sending, queue) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;

        Ok(Self { sending, queue })
    }

    /// Push `fds`, [`MAX_FDS`] of them at most, as those from `index` on, if the queue has room for
    /// them, which this tells: [`take`] gives them back with `index`.
    ///
    /// The queue takes duplicates of them, each as it is. Those that nobody took out by then close
    /// with the last file descriptor of the socket that [`into_socket`](Self::into_socket) gives,
    /// as when the manager removes it from the store. This fails with `E2BIG` for more than
    /// [`MAX_FDS`] file descriptors, and pushes none of them then.
    pub(crate) fn push(&self, index: u64, fds: &[BorrowedFd<'_>]) -> io::Result<bool> {
        if fds.len() > MAX_FDS {
            return Err(Errno::TOOBIG.into());
        }

        let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if !fds.is_empty() {
            let fits = control.push(SendAncillaryMessage::ScmRights(fds));
            debug_assert!(
                fits,
                "the space is for as many file descriptors as a datagram takes"
            );
        }
        let index = index.to_le_bytes();
        let pushed = retry_on_intr(|| {
            sendmsg(
                &self.sending,
                &[IoSlice::new(&index)],
                &mut control,
                SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
            )
        });
        match pushed {
            Ok(_) => Ok(true),
            Err(Errno::AGAIN) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The socket whose queue holds what was pushed, for the store to keep, and to pass on to the
    /// next instance.
    ///
    /// The end that the file descriptors were pushed into closes, so that nothing more comes, which
    /// tells [`take`] where the queue ends.
    pub(crate) fn into_socket(self) -> OwnedFd {
        self.queue
    }
}

/// Take the file descriptors out of the queue of `socket`, which [`Queue::into_socket`] gave, as
/// the next instance of the service does with the socket that it was passed.
///
/// `each` gets those of each datagram, in the order that they were pushed in, with the index that
/// they were pushed as, and whether all of them came: the kernel closes those that the process
/// can't have, as when it has as many open as its limit of open files takes, or a security module
/// denies it one, and gives those before them alone. They are close-on-exec.
///
/// It ends where the queue does, and fails on a datagram that no [`Queue`] pushed: its file
/// descriptors close then, and those after it stay in the queue, to close with the socket.
pub(crate) fn take<F>(socket: BorrowedFd<'_>, mut each: F) -> io::Result<()>
where
    F: FnMut(u64, Vec<OwnedFd>, bool),
{
    loop {
        let mut index = [0; INDEX_LEN];
        let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let received = retry_on_intr(|| {
            recvmsg(
                socket,
                &mut [IoSliceMut::new(&mut index)],
                &mut control,
                RecvFlags::DONTWAIT | RecvFlags::CMSG_CLOEXEC,
            )
        });
        let received = match received {
            Ok(received) => received,
            // The end that the file descriptors were pushed into is open still, somewhere, and
            // nothing more is queued.
            Err(Errno::AGAIN) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // Past the last datagram, as the end that they were pushed into is closed: every datagram
        // that a queue pushed has an index.
        if received.bytes == 0 {
            return Ok(());
        }
        if received.bytes != INDEX_LEN || received.flags.contains(ReturnFlags::TRUNC) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "a datagram that no queue of file descriptors pushed",
            ));
        }
        let fds = control
            .drain()
            .filter_map(|message| match message {
                RecvAncillaryMessage::ScmRights(fds) => Some(fds),
                _ => None,
            })
            .flatten()
            .collect();
        let complete = !received.flags.contains(ReturnFlags::CTRUNC);

        each(u64::from_le_bytes(index), fds, complete);
    }
}

/// How long the index is that a [`Queue`] pushes with each datagram, ahead of its file
/// descriptors: a `u64`, little-endian.
const INDEX_LEN: usize = size_of::<u64>();

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use rustix::{
        io::{FdFlags, fcntl_getfd, write},
        net::{send, sockopt::set_socket_send_buffer_size},
        process::{Resource, getrlimit, setrlimit},
    };

    use super::*;
    use crate::test_utils::{in_own_process, pipe, read_byte};

    /// What goes into a queue comes out of it as it went, in order, with the index that it went
    /// as, the same file descriptor twice included, and close-on-exec.
    #[test]
    fn round_trip() {
        let (read_a, write_a) = pipe();
        let (read_b, write_b) = pipe();
        let queue = Queue::new().unwrap();

        assert!(queue.push(7, &[write_a.as_fd(), write_b.as_fd()]).unwrap());
        assert!(queue.push(9, &[]).unwrap());
        // Which the store would keep once.
        assert!(queue.push(10, &[write_a.as_fd(), write_a.as_fd()]).unwrap());
        let socket = queue.into_socket();

        let mut taken = Vec::new();
        take(socket.as_fd(), |index, fds, complete| {
            assert!(complete, "not all of those at {index} came");
            taken.push((index, fds));
        })
        .unwrap();
        let [(7, first), (9, none), (10, second)] = &taken[..] else {
            panic!("{taken:?} came");
        };
        assert!(none.is_empty());
        // These are the pipes that went, in the order that they went.
        let pairs = [
            (&first[..], [(&read_a, b'a'), (&read_b, b'b')]),
            (&second[..], [(&read_a, b'c'), (&read_a, b'd')]),
        ];
        for (fds, reads) in pairs {
            assert_eq!(fds.len(), reads.len());
            for (fd, (read, byte)) in fds.iter().zip(reads) {
                write(fd, &[byte]).unwrap();
                assert_eq!(read_byte(read), byte);
                assert!(fcntl_getfd(fd).unwrap().contains(FdFlags::CLOEXEC));
            }
        }
        // Nothing more comes.
        take(socket.as_fd(), |index, _, _| panic!("{index} came twice")).unwrap();
    }

    /// A queue takes as many datagrams as the send buffer of its socket has room for, and then
    /// tells that it is full, rather than take more, while those that it took come out all the
    /// same.
    #[test]
    fn fills_up() {
        let queue = Queue::new().unwrap();
        // The kernel gives it the smallest buffer that it gives any, of a few kilobytes.
        set_socket_send_buffer_size(&queue.sending, 1).unwrap();
        let (_read, write) = pipe();

        let mut pushed = 0;
        while queue.push(pushed, &[write.as_fd()]).unwrap() {
            pushed += 1;
            assert!(
                pushed < 1000,
                "the queue took more than its buffer has room for"
            );
        }
        assert!(pushed > 0);
        assert!(!queue.push(pushed, &[write.as_fd(); MAX_FDS]).unwrap());
        let socket = queue.into_socket();

        let mut indexes = Vec::new();
        take(socket.as_fd(), |index, fds, complete| {
            assert!(complete && fds.len() == 1, "{index}: {fds:?}");
            indexes.push(index);
        })
        .unwrap();
        assert!(indexes.into_iter().eq(0..pushed));
    }

    /// A queue takes no more file descriptors with a datagram than fit in a message, and none of
    /// them then.
    #[test]
    fn takes_max_fds_at_most() {
        let queue = Queue::new().unwrap();
        let (_read, write) = pipe();

        let error = queue.push(0, &[write.as_fd(); MAX_FDS + 1]).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::TOOBIG.raw_os_error()));
        assert!(queue.push(1, &[write.as_fd(); MAX_FDS]).unwrap());
        let socket = queue.into_socket();

        let mut taken = Vec::new();
        take(socket.as_fd(), |index, fds, _| {
            taken.push((index, fds.len()))
        })
        .unwrap();
        assert_eq!(taken, [(1, MAX_FDS)]);
    }

    /// The queue ends where nothing more is queued, even while the end that the file descriptors
    /// were pushed into is open still.
    #[test]
    fn open_still() {
        let queue = Queue::new().unwrap();
        let (_read, write) = pipe();
        assert!(queue.push(3, &[write.as_fd()]).unwrap());

        let mut taken = Vec::new();
        take(queue.queue.as_fd(), |index, fds, _| {
            taken.push((index, fds.len()))
        })
        .unwrap();
        assert_eq!(taken, [(3, 1)]);
    }

    /// Taking file descriptors out of a socket that no queue filled fails on a datagram without an
    /// index, rather than give its file descriptors with one that means nothing.
    #[test]
    fn other_datagrams_are_refused() {
        for datagram in [&b"an index and more"[..], b"short"] {
            let (sending, socket) = socketpair(
                AddressFamily::UNIX,
                SocketType::SEQPACKET,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            assert_eq!(
                send(&sending, datagram, SendFlags::empty()).unwrap(),
                datagram.len()
            );

            let error = take(socket.as_fd(), |index, _, _| panic!("{index} came"));
            assert_eq!(
                error.unwrap_err().kind(),
                ErrorKind::InvalidData,
                "{datagram:?}"
            );
        }
    }

    /// The file descriptors of a datagram that the process can't have close, and the queue tells
    /// that not all of them came, while it gives those that did.
    #[test]
    fn without_room_for_all() {
        let test = "queue::tests::without_room_for_all";
        if !in_own_process(test, "ZOTER_TEST_LOW_NOFILE=1") {
            return;
        }

        let queue = Queue::new().unwrap();
        let (_read, write) = pipe();
        assert!(queue.push(0, &[write.as_fd(); 8]).unwrap());
        let socket = queue.into_socket();
        // Room for 2 file descriptors more than are open, or 3 once the directory closes.
        let open = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
        let mut limit = getrlimit(Resource::Nofile);
        limit.current = Some(open + 2);
        setrlimit(Resource::Nofile, limit).unwrap();

        let mut taken = Vec::new();
        take(socket.as_fd(), |index, fds, complete| {
            taken.push((index, fds.len(), complete))
        })
        .unwrap();
        let [(0, came, false)] = taken[..] else {
            panic!("{taken:?} came");
        };
        assert!(0 < came && came < 8, "{came} came");
    }
}
