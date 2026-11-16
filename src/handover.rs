//! A hand-over of a service to its next instance: [`Handover`], and what it stored: [`Stored`].

use crate::{Notifier, Result, Save, Store};

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
    /// Either the whole hand-over is stored, or this fails: no part of it is left out. It fails
    /// with [`Error::Unsaved`](crate::Error::Unsaved) if the state or an item can't be saved, or if
    /// saving its record fails. The error names the id of the item, and nothing is stored. It fails
    /// with [`Error::StoreFull`](crate::Error::StoreFull) if the file descriptors don't fit in the
    /// store. The service's `FileDescriptorStoreMax=` makes the store hold more. It fails with
    /// [`Error::Io`](crate::Error::Io) for any I/O error while storing the memfd or the file
    /// descriptors, or while queueing them for the store. It fails with
    /// [`Error::TimedOut`](crate::Error::TimedOut) if systemd takes longer than 5 s to process what
    /// was stored. A hand-over that fails after the memfd was stored may leave part of itself in
    /// the store. The next instance fails to take that part over, and removes it from the store
    /// like any other hand-over.
    pub fn store<S, I, T>(self, state: S, items: I) -> Result<Stored>
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
    /// How many items were stored, whole.
    pub fn items(&self) -> usize {
        self.items
    }

    /// How many file descriptors were stored, those of the state included.
    pub fn fds(&self) -> usize {
        self.fds
    }
}
