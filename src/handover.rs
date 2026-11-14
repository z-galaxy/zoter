//! A hand-over of a service to its next instance: [`Handover`], and what it stored: [`Stored`].

use std::io;

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
}

impl Handover {
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
    /// The hand-over is stored whole, or this fails. It fails with
    /// [`ErrorKind::InvalidInput`](io::ErrorKind::InvalidInput) if the state or an item can't be
    /// saved, or if its record fails to be saved, naming the id of the item, and nothing is stored
    /// then. It fails with [`ErrorKind::StorageFull`](io::ErrorKind::StorageFull) if the file
    /// descriptors don't fit in the store, which is to take more with `FileDescriptorStoreMax=` of
    /// the service, and with any I/O error of storing the memfd or the file descriptors, or of
    /// queueing them for the store. If systemd takes longer than 5 s to deal with what was stored,
    /// it fails with [`ErrorKind::TimedOut`](io::ErrorKind::TimedOut).
    ///
    /// The memfd of the hand-over is stored last, after the file descriptors, so a hand-over that
    /// fails before then leaves only sockets of its file descriptors in the store, which the next
    /// instance takes nothing over from, and removes. One that times out once the memfd was stored
    /// can still be taken over, whole.
    pub fn store<S, I, T>(self, state: S, items: I) -> io::Result<Stored>
    where
        S: Save,
        I: IntoIterator<Item = (u64, T)>,
        T: Save,
    {
        let items: Vec<_> = items.into_iter().collect();
        // The rest takes no generics, so that it is built once, whatever the types.
        let items: Vec<_> = items
            .iter()
            .map(|(id, item)| (*id, item as &dyn Save))
            .collect();

        self.store.hand_over(&state, &items)
    }

    /// A hand-over through `store`, which takes one.
    pub(crate) fn new(store: Store) -> Self {
        Self { store }
    }
}

/// What [`Handover::store`] stored: all of the hand-over.
#[derive(Debug)]
pub struct Stored {
    pub(crate) items: usize,
    pub(crate) fds: usize,
}

impl Stored {
    /// How many items were stored.
    pub fn items(&self) -> usize {
        self.items
    }

    /// How many file descriptors were stored, those of the state included.
    pub fn fds(&self) -> usize {
        self.fds
    }
}
