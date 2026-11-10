//! A hand-over of a service to its next instance: [`Handover`], and what of it was stored:
//! [`Stored`].

use std::{fmt, io};

use crate::{Notifier, Save, Store};

/// A hand-over of a service to its next instance, through its [`Store`], which
/// [`Store::handover`] starts.
///
/// Starting it fails if the store can't take a hand-over, so that the service finds out before it
/// stops taking requests to hand over. [`store`](Self::store) then stores what the service hands
/// over: its state, and items, such as the connections of its clients, under ids of its own, each
/// what [`Save`] saves, a record of bytes and file descriptors. [`Store::take_over`] gives it back
/// in the next instance, as a [`Restored`](crate::Restored).
#[derive(Debug)]
pub struct Handover {
    store: Store,
    extra_fds_per_item: usize,
    spare_fds: usize,
}

impl Handover {
    /// How many file descriptors each item takes in the next instance, besides those that it hands
    /// over, such as the pidfd of the peer of a connection: none, unless set.
    ///
    /// [`store`](Self::store) leaves out the items that don't fit under the limit of open file
    /// descriptors of the next instance, counting these too.
    pub fn set_extra_fds_per_item(&mut self, count: usize) {
        self.extra_fds_per_item = count;
    }

    /// How many file descriptors the next instance keeps free, besides those of the hand-over, for
    /// those that it opens of its own as it starts and takes over: 32, unless set.
    pub fn set_spare_fds(&mut self, count: usize) {
        self.spare_fds = count;
    }

    /// The notification socket of the service manager, as [`Store::notifier`] gives it, for the
    /// service to tell it how stopping goes, among other things, while it hands over.
    pub fn notifier(&self) -> &Notifier {
        self.store.notifier()
    }

    /// Store `state`, the state of the service, and `items`, in their order, each under its id,
    /// for the next instance, and wait for systemd to have dealt with them.
    ///
    /// The state of a service that has none is `&[][..]`. zoter doesn't look at the ids, nor
    /// checks that they are unique: it gives each back with its item, for the state to refer to
    /// the item by. The items are of any type, borrowed or owned, those of different types as
    /// `&dyn Save` or `Box<dyn Save>`.
    ///
    /// It fails with [`ErrorKind::InvalidInput`](io::ErrorKind::InvalidInput) if the state can't
    /// be saved, or if its record fails to be saved, and with any I/O error of storing it, the
    /// memfd or its file descriptors. If systemd takes longer than 5 s to deal with what was
    /// stored, it fails with [`ErrorKind::TimedOut`](io::ErrorKind::TimedOut). A hand-over that
    /// fails once the memfd was stored may leave part of it in the store, which the next instance
    /// fails to take over, and removes from the store as it does any other.
    ///
    /// The items that can't go, as [`Stored::left_out`] tells, are left out whole, while those
    /// after them may still go: those that can't be saved, those whose records fail to be saved,
    /// those that don't fit under the soft limit of open files of the next instance, counting what
    /// [`set_extra_fds_per_item`](Self::set_extra_fds_per_item) and
    /// [`set_spare_fds`](Self::set_spare_fds) add, and those whose file descriptors don't fit in
    /// the store, or fail to be queued for it.
    pub fn store<S, I, T>(self, state: S, items: I) -> io::Result<Stored>
    where
        S: Save,
        I: IntoIterator<Item = (u64, T)>,
        T: Save,
    {
        let Self {
            store,
            extra_fds_per_item,
            spare_fds,
        } = self;
        let items: Vec<_> = items.into_iter().collect();
        // The rest takes no generics, so that it is built once, whatever the types.
        let items: Vec<_> = items
            .iter()
            .map(|(id, item)| (*id, item as &dyn Save))
            .collect();

        store.hand_over(&state, &items, extra_fds_per_item, spare_fds)
    }

    /// A hand-over through `store`, which takes one.
    pub(crate) fn new(store: Store) -> Self {
        Self {
            store,
            extra_fds_per_item: 0,
            spare_fds: DEFAULT_SPARE_FDS,
        }
    }
}

/// What [`Handover::store`] stored.
#[derive(Debug)]
pub struct Stored {
    pub(crate) items: usize,
    pub(crate) fds: usize,
    pub(crate) left_out: Vec<(u64, LeftOut)>,
}

impl Stored {
    /// How many items were stored, whole.
    pub fn items(&self) -> usize {
        self.items
    }

    /// How many file descriptors were stored, those of the state included.
    pub fn fds(&self) -> usize {
        self.fds
    }

    /// The ids of the items that were left out, and why, in the order of the items.
    ///
    /// Nothing is stored of an item that is left out, so its peer is disconnected once its file
    /// descriptors close: as [`Handover::store`] returns, for an item that it was given by value,
    /// and as the caller drops it, for one that it was given by reference.
    pub fn left_out(&self) -> &[(u64, LeftOut)] {
        &self.left_out
    }
}

/// Why an item of a [`Handover`] was left out.
#[derive(Debug)]
#[non_exhaustive]
pub enum LeftOut {
    /// It can't be saved: its [`Save::saved_len`] is `None`.
    Unsaved,
    /// Its record failed to be saved.
    Save(io::Error),
    /// Its file descriptors don't fit under the limit of open file descriptors of the next
    /// instance, which is to be raised with the hard limit of `LimitNOFILE=` of the service.
    FdLimit,
    /// Its file descriptors don't fit in the store, which is to take more with
    /// `FileDescriptorStoreMax=` of the service.
    Store,
    /// Its file descriptors failed to be queued for the store, with an error of this kind.
    Queue(io::ErrorKind),
}

impl fmt::Display for LeftOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsaved => f.write_str("it can't be saved"),
            Self::Save(e) => write!(f, "its record failed to be saved: {e}"),
            Self::FdLimit => f.write_str(
                "its file descriptors don't fit under the limit of open file descriptors of the \
                 next instance: raise the hard limit of `LimitNOFILE=` of the service",
            ),
            Self::Store => f.write_str(
                "its file descriptors don't fit in the file descriptor store: raise \
                 `FileDescriptorStoreMax=` of the service",
            ),
            Self::Queue(kind) => write!(f, "its file descriptors failed to be queued: {kind}"),
        }
    }
}

/// How many file descriptors the next instance keeps free, unless set otherwise.
const DEFAULT_SPARE_FDS: usize = 32;
