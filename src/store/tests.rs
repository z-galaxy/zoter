use std::{
    cell::RefCell,
    iter,
    os::fd::{AsFd, OwnedFd},
};

use rustix::io::{read, write};

use super::*;
use crate::{
    test_utils::{in_own_process, pipe, read_byte},
    testing::Manager,
};

/// What one instance hands over, the next takes over, through the store: the state, and the items
/// in order, with their ids, records and file descriptors, more than a datagram of a queue takes,
/// one of an open file description that another one has, and none, included. What the store held
/// of it is removed from it, and the sockets of socket activation are left to the service.
#[test]
fn round_trip() {
    let mut manager = Manager::new().unwrap();
    manager.add_socket("test.socket", pipe().1);
    let (read_a, write_a) = pipe();
    let (read_b, write_b) = pipe();
    let many: Vec<_> = iter::repeat_with(|| pipe().1).take(MAX_FDS + 10).collect();
    let bytes = b"bytes only".to_vec();
    let many = Item::new(b"many", many);
    let twice = vec![write_a.try_clone().unwrap(), write_a.try_clone().unwrap()];
    let twice = Item::new(b"", twice);
    let items: [(u64, &dyn Save); 4] = [(1, &bytes), (2, &write_a), (3, &many), (u64::MAX, &twice)];
    let state = Item::new(b"state", vec![write_b]);
    let stored = store(&manager)
        .handover()
        .unwrap()
        .store(state, items)
        .unwrap();
    assert_eq!(stored.items(), 4);
    assert_eq!(stored.fds(), 1 + 1 + MAX_FDS + 10 + 2);
    assert!(stored.left_out().is_empty(), "{stored:?}");
    assert_eq!(manager.stored_names(), ["test/state", "test/fds"]);

    let mut passed = manager.passed().unwrap();
    let mut second = store(&manager);
    let mut restored = second.take_over(&mut passed).unwrap().unwrap();
    second.notifier().barrier(WAIT).unwrap();
    assert_eq!(manager.take_removed(), ["test/fds", "test/state"]);
    let names: Vec<_> = passed.iter().map(|(name, _)| name).collect();
    assert_eq!(names, ["test.socket"]);

    assert_eq!(restored.state(), b"state");
    let [fd] = <[OwnedFd; 1]>::try_from(restored.take_state_fds()).unwrap();
    write(&fd, b"b").unwrap();
    assert_eq!(read_byte(&read_b), b'b');
    assert!(restored.take_state_fds().is_empty());
    assert!(restored.left_out().is_empty());
    let mut items = restored.take_items();
    let item = items.next().unwrap();
    assert_eq!(item.id, 1);
    assert_eq!(item.restore::<Vec<u8>>().unwrap(), b"bytes only");
    let item = items.next().unwrap();
    assert_eq!(item.id, 2);
    let fd = item.restore::<OwnedFd>().unwrap();
    write(&fd, b"a").unwrap();
    assert_eq!(read_byte(&read_a), b'a');
    let item = items.next().unwrap();
    assert_eq!(
        (item.id, item.record, item.fds.len()),
        (3, &b"many"[..], MAX_FDS + 10)
    );
    let item = items.next().unwrap();
    assert_eq!(item.id, u64::MAX);
    // Each of the two goes as it is, rather than once, as the store would keep it.
    assert_eq!(item.fds.len(), 2);
    for fd in &item.fds {
        write(fd, b"t").unwrap();
        assert_eq!(read_byte(&read_a), b't');
    }
    assert!(items.next().is_none());
    drop(items);
    assert_eq!(restored.take_items().count(), 0);
}

/// Nothing was handed over without the memfd of the hand-over, whatever else there is, and the
/// queue sockets that came without it close.
#[test]
fn nothing_to_take_over() {
    let mut manager = Manager::new().unwrap();
    manager.add_socket("test.socket", pipe().1);
    let mut passed = manager.passed().unwrap();
    assert!(store(&manager).take_over(&mut passed).unwrap().is_none());
    assert_eq!(passed.len(), 1);

    let queue = Queue::new().unwrap().into_socket();
    manager.replace_store(vec![("test/fds".to_owned(), queue)]);
    let mut passed = manager.passed().unwrap();
    let mut store = store(&manager);
    assert!(store.take_over(&mut passed).unwrap().is_none());
    store.notifier().barrier(WAIT).unwrap();
    assert_eq!(manager.take_removed(), ["test/fds"]);
    assert_eq!(passed.len(), 1);
}

/// A memfd that isn't a hand-over fails the take-over, after all of what was stored under the key
/// was removed from the store, names that zoter doesn't know included, while what was stored under
/// other keys stays.
#[test]
fn no_take_over_of_garbage() {
    let manager = Manager::new().unwrap();
    let garbage = memfd::sealed("test", b"not a hand-over").unwrap();
    manager.replace_store(vec![
        ("test/state".to_owned(), garbage),
        ("test/other".to_owned(), pipe().1),
        ("other/state".to_owned(), pipe().1),
    ]);

    let mut passed = manager.passed().unwrap();
    let mut store = store(&manager);
    let error = store.take_over(&mut passed).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    store.notifier().barrier(WAIT).unwrap();
    assert_eq!(manager.take_removed(), ["test/other", "test/state"]);
    let names: Vec<_> = passed.iter().map(|(name, _)| name).collect();
    assert_eq!(names, ["other/state"]);
}

/// The items go in their order, as far as they fit under the limit of open file descriptors of the
/// next instance, counting the extra ones of each, and one that doesn't fit is left out whole,
/// while those after it may still fit.
#[test]
fn items_fit_under_the_fd_limit() {
    let manager = Manager::new().unwrap();
    let mut first = store(&manager);
    let reserved = START as usize + manager.store_max();
    first.set_fd_limit(Some(reserved as u64 + 4));

    let mut handover = first.handover().unwrap();
    handover.set_spare_fds(0);
    handover.set_extra_fds_per_item(1);
    // Each takes one file descriptor more than it has: 2, then 3, which don't fit with the 2 left,
    // then 1, which does.
    let items = [
        (1, Item::new(b"one", vec![pipe().1])),
        (2, Item::new(b"two", vec![pipe().1, pipe().1])),
        (3, Item::new(b"three", Vec::new())),
    ];
    let stored = handover.store(&[][..], items).unwrap();
    assert_eq!(stored.items(), 2);
    let [(2, LeftOut::FdLimit)] = stored.left_out() else {
        panic!("{stored:?}");
    };

    let restored = take_over(&manager);
    assert_eq!(ids(restored), [1, 3]);
}

/// An item that can't be saved, and one whose record fails to be saved, are left out alone, with
/// their file descriptors, and the others go.
#[test]
fn unsaved_items_are_left_out() {
    let manager = Manager::new().unwrap();
    let items: [(u64, Box<dyn Save>); 3] = [
        (1, Box::new(Unsaved)),
        (2, Box::new(Failing(pipe().1))),
        (3, Box::new(Item::new(b"three", vec![pipe().1]))),
    ];
    let stored = store(&manager)
        .handover()
        .unwrap()
        .store(&[][..], items)
        .unwrap();
    assert_eq!((stored.items(), stored.fds()), (1, 1));
    let [(1, LeftOut::Unsaved), (2, LeftOut::Save(e))] = stored.left_out() else {
        panic!("{stored:?}");
    };
    assert_eq!(e.kind(), ErrorKind::Other);

    let mut restored = take_over(&manager);
    let item = restored.take_items().next().unwrap();
    assert_eq!(
        (item.id, item.record, item.fds.len()),
        (3, &b"three"[..], 1)
    );
}

/// The items whose file descriptors aren't all there are left out as the next instance takes over,
/// and the others aren't.
#[test]
fn items_with_missing_fds_are_left_out() {
    let manager = Manager::new().unwrap();
    let items = [
        (1, Item::new(b"one", vec![pipe().1])),
        (2, Item::new(b"two", vec![pipe().1, pipe().1])),
        (3, Item::new(b"three", vec![pipe().1])),
    ];
    store(&manager)
        .handover()
        .unwrap()
        .store(&[][..], items)
        .unwrap();

    // The second file descriptor, the first of the second item, goes missing.
    let mut stored = manager.stored().unwrap();
    let queue = Queue::new().unwrap();
    queue::take(stored[1].1.as_fd(), |index, fds, _| {
        for (index, fd) in (index..).zip(fds) {
            if index != 1 {
                assert!(queue.push(index, &[fd.as_fd()]).unwrap());
            }
        }
    })
    .unwrap();
    stored[1].1 = queue.into_socket();
    manager.replace_store(stored);

    let restored = take_over(&manager);
    assert_eq!(restored.left_out(), [2]);
    assert_eq!(ids(restored), [1, 3]);
}

/// A state that can't be saved fails the hand-over, as one that fails to be saved does.
#[test]
fn unsaved_state() {
    for state in [
        Box::new(Unsaved) as Box<dyn Save>,
        Box::new(Failing(pipe().1)),
    ] {
        let manager = Manager::new().unwrap();
        let handover = store(&manager).handover().unwrap();
        let error = handover.store(state, NO_ITEMS).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }
}

/// A store that takes no hand-over, as one without `$FDSTORE` or of fewer than 2 file descriptors,
/// starts none.
#[test]
fn no_hand_over_without_room() {
    let manager = Manager::new().unwrap();
    for max in [None, Some(0), Some(1)] {
        let store = Store::new(manager.notifier().unwrap(), KEY, max).unwrap();
        let error = store.handover().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported, "{max:?}");
    }
}

/// A hand-over starts before the service has what it hands over, which it hands over borrowed, as
/// an async service has it once it stopped taking requests.
#[test]
fn items_made_once_started() {
    let manager = Manager::new().unwrap();
    let handover = store(&manager).handover().unwrap();
    let clients = [(4, pipe().1), (2, pipe().1)];
    let state = b"state".to_vec();
    let items = clients.iter().map(|(id, client)| (*id, client));
    let stored = handover.store(&state, items).unwrap();
    assert_eq!((stored.items(), stored.fds()), (2, 2));

    assert_eq!(ids(take_over(&manager)), [4, 2]);
}

/// A hand-over is `Send`, so that an async service can hold one across the awaits of stopping, on
/// a runtime of several threads.
#[test]
fn handover_is_send() {
    fn send<T: Send>() {}

    send::<Handover>();
}

#[test]
fn keys() {
    let manager = Manager::new().unwrap();
    let longest = "a".repeat(MAX_KEY_LEN);
    for key in ["busd", "with spaces", "a.b-c_d", &longest] {
        Store::new(manager.notifier().unwrap(), key, Some(2)).unwrap();
    }
    let too_long = "a".repeat(MAX_KEY_LEN + 1);
    for key in ["", "a/b", "a:b", "a\nb", "é", &too_long] {
        let error = Store::new(manager.notifier().unwrap(), key, Some(2)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{key:?}");
    }
}

/// The file descriptors go on in the next queue socket once one is full, and come back by the
/// index that each was queued as, whatever the order of the queue sockets, each as it went, one of
/// the same open file description as another one included.
#[test]
fn queued_fds_come_back_by_their_index() {
    let pipes: Vec<_> = (0..3).map(|_| pipe()).collect();
    // The last is of the same open file description as the first, which the store would keep
    // once.
    let mut fds: Vec<_> = pipes.iter().map(|(_, write)| write.as_fd()).collect();
    fds.push(fds[0]);
    let stored = RefCell::new(Vec::new());

    // Datagrams without file descriptors fill the first queue socket, between two that have some.
    let (first, last) = fds.split_at(2);
    let (mut first, mut last) = (Some(first), Some(last));
    let chunks = iter::from_fn(|| match stored.borrow().is_empty() {
        true => Some(first.take().unwrap_or_default()),
        false => last.take(),
    });
    let (queued, failure) = queue_fds(chunks, 3, |socket| {
        stored.borrow_mut().push(socket);

        Ok(())
    })
    .unwrap();
    assert!(failure.is_none(), "{failure:?}");
    assert_eq!(queued, fds.len());
    let mut stored = stored.into_inner();
    assert_eq!(
        stored.len(),
        2,
        "the file descriptors went in other than two sockets"
    );

    stored.reverse();
    let (mut taken, complete) = take_queued(stored);
    assert!(complete);
    // What goes into each file descriptor comes out of the pipe that it was queued of.
    for (index, pipe) in [(0, 0), (1, 1), (2, 2), (3, 0)] {
        let fd = taken.remove(&index).unwrap();
        let byte = [index as u8];
        assert_eq!(write(&fd, &byte).unwrap(), 1);
        let mut read_byte = [0];
        assert_eq!(read(&pipes[pipe].0, &mut read_byte).unwrap(), 1);
        assert_eq!(read_byte, byte);
    }
    assert!(taken.is_empty(), "{taken:?}");
}

/// The file descriptors go in no more queue sockets than the store has room for: those of the
/// datagrams that fit in them are queued, and the others aren't.
#[test]
fn queued_fds_fill_the_room_of_the_store() {
    let fd = pipe().1;
    let fds = [fd.as_fd()];
    let mut stored = Vec::new();

    // One datagram with a file descriptor, then ones without, more than any queue socket takes.
    let chunks = iter::once(&fds[..]).chain(iter::repeat(&fds[..0]));
    let (queued, failure) = queue_fds(chunks, 1, |socket| {
        stored.push(socket);

        Ok(())
    })
    .unwrap();
    assert!(failure.is_none(), "{failure:?}");
    assert_eq!(queued, 1);
    assert_eq!(stored.len(), 1);
    let (taken, _) = take_queued(stored);
    assert_eq!(taken.into_keys().collect::<Vec<_>>(), [0]);

    // A store without room takes none.
    let (queued, failure) = queue_fds([&fds[..]], 0, |_| unreachable!()).unwrap();
    assert_eq!(queued, 0);
    assert!(failure.is_none());
}

/// An item gets the file descriptors at its place, if they are all there, and none otherwise,
/// while those that are there go all the same.
#[test]
fn items_take_their_fds() {
    let mut fds: HashMap<u64, OwnedFd> = [0, 1, 2, 4].map(|index| (index, pipe().1)).into();

    assert_eq!(take_fds(&mut fds, 0..2).unwrap().len(), 2);
    assert!(take_fds(&mut fds, 2..4).is_none(), "{fds:?}");
    assert!(!fds.contains_key(&2), "{fds:?}");
    assert!(take_fds(&mut fds, 4..u64::MAX).is_none());
    assert!(fds.is_empty(), "{fds:?}");
    assert_eq!(take_fds(&mut fds, 5..5).unwrap().len(), 0);
}

#[test]
fn store_max() {
    assert_eq!(parse_store_max(Some("16")), Some(16));
    assert_eq!(parse_store_max(Some("1")), Some(1));
    assert_eq!(parse_store_max(None), None);
    for value in ["", "0", "-1", "x", "1.5", " 1", "99999999999999999999"] {
        assert_eq!(parse_store_max(Some(value)), None, "$FDSTORE={value:?}");
    }
}

#[test]
fn from_the_environment() {
    let exports = "NOTIFY_SOCKET=/run/systemd/notify FDSTORE=16";
    if !in_own_process("store::tests::from_the_environment", exports) {
        return;
    }

    let store = Store::from_env(KEY).unwrap().unwrap();
    assert_eq!(store.max(), Some(16));
    // SAFETY: The test runs in a process of its own, which has no other thread that reads or
    // writes the environment. So do the ones below.
    unsafe { env::remove_var("FDSTORE") };
    let store = Store::from_env(KEY).unwrap().unwrap();
    assert_eq!(store.max(), None);
    // SAFETY: As above.
    unsafe { env::remove_var("NOTIFY_SOCKET") };
    assert!(Store::from_env(KEY).unwrap().is_none());
}

/// A store of the manager, under `KEY`, that takes as many file descriptors as the store of the
/// manager does.
fn store(manager: &Manager) -> Store {
    Store::new(manager.notifier().unwrap(), KEY, Some(manager.store_max())).unwrap()
}

/// What the manager passes the next instance, taken over.
fn take_over(manager: &Manager) -> Restored {
    let mut passed = manager.passed().unwrap();

    store(manager).take_over(&mut passed).unwrap().unwrap()
}

/// The ids of the items of `restored`.
fn ids(mut restored: Restored) -> Vec<u64> {
    restored.take_items().map(|item| item.id).collect()
}

/// An item of bytes and file descriptors of the test's choosing.
struct Item {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
}

impl Item {
    fn new(bytes: &[u8], fds: Vec<OwnedFd>) -> Self {
        Self {
            bytes: bytes.to_vec(),
            fds,
        }
    }
}

impl Save for Item {
    fn saved_len(&self) -> Option<usize> {
        Some(self.bytes.len())
    }

    fn save(&self, dest: &mut [u8]) -> io::Result<()> {
        dest.copy_from_slice(&self.bytes);

        Ok(())
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        self.fds.iter().map(AsFd::as_fd).collect()
    }
}

/// An item that can't be saved.
struct Unsaved;

impl Save for Unsaved {
    fn saved_len(&self) -> Option<usize> {
        None
    }

    fn save(&self, _dest: &mut [u8]) -> io::Result<()> {
        unreachable!("an item that can't be saved was saved")
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        Vec::new()
    }
}

/// An item whose record fails to be saved, after writing some of it.
struct Failing(OwnedFd);

impl Save for Failing {
    fn saved_len(&self) -> Option<usize> {
        Some(8)
    }

    fn save(&self, dest: &mut [u8]) -> io::Result<()> {
        dest[0] = 1;

        Err(io::Error::other("the record failed"))
    }

    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        vec![self.0.as_fd()]
    }
}

const KEY: &str = "test";
/// No items, for a hand-over of the state alone.
const NO_ITEMS: [(u64, &[u8]); 0] = [];
const WAIT: Duration = Duration::from_secs(10);
