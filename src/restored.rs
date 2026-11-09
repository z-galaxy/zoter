//! What the previous instance of a service handed over: [`Restored`].

use std::{fmt, mem, ops::Range, os::fd::OwnedFd};

use crate::{Restore, memfd};

/// What the previous instance of a service handed over: its state, and its items, each with its
/// record and its file descriptors.
///
/// [`Store::take_over`](crate::Store::take_over) gives it. The records are read in place, in a
/// mapping of the memfd that goes with this, so restore what they hold before this goes.
pub struct Restored {
    pub(crate) view: memfd::View,
    pub(crate) state: Range<usize>,
    pub(crate) state_fds: Vec<OwnedFd>,
    pub(crate) items: Vec<Slot>,
    pub(crate) left_out: Vec<u64>,
}

impl Restored {
    /// The record of the state.
    pub fn state(&self) -> &[u8] {
        &self.view[self.state.clone()]
    }

    /// Take the file descriptors of the state, in their order: none the second time.
    pub fn take_state_fds(&mut self) -> Vec<OwnedFd> {
        mem::take(&mut self.state_fds)
    }

    /// Take the items whose file descriptors are all there, in the order that they were handed
    /// over: none the second time. Those that the iterator doesn't get to close as it goes.
    pub fn take_items(&mut self) -> impl Iterator<Item = RestoredItem<'_>> {
        let view = &self.view;

        self.items.drain(..).map(move |slot| RestoredItem {
            id: slot.id,
            record: &view[slot.record],
            fds: slot.fds,
        })
    }

    /// The ids of the items that were left out, as their file descriptors are not all there, in
    /// the order that they were handed over.
    ///
    /// The kernel closes the file descriptors that the process can't have, as when it has as many
    /// open as its limit of open files takes, which [`raise_fd_limit`](crate::raise_fd_limit)
    /// raises. Those of the item that are there close.
    pub fn left_out(&self) -> &[u64] {
        &self.left_out
    }
}

/// Shows how long the state is, and how many items there are, rather than what they hold.
impl fmt::Debug for Restored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Restored")
            .field("state_len", &self.state.len())
            .field("state_fds", &self.state_fds.len())
            .field("items", &self.items.len())
            .field("left_out", &self.left_out)
            .finish()
    }
}

/// An item that the previous instance of a service handed over, as [`Restored::take_items`]
/// gives it.
#[derive(Debug)]
pub struct RestoredItem<'r> {
    /// The id that the item was handed over under.
    pub id: u64,
    /// Its record, in the mapping of the memfd.
    pub record: &'r [u8],
    /// Its file descriptors, in their order.
    pub fds: Vec<OwnedFd>,
}

impl RestoredItem<'_> {
    /// Restore `T` from the record and the file descriptors of the item, as [`Restore::restore`]
    /// does.
    pub fn restore<T>(self) -> Result<T, T::Error>
    where
        T: Restore,
    {
        T::restore(self.record, self.fds)
    }
}

/// An item, where its record is in the memfd, and its file descriptors.
#[derive(Debug)]
pub(crate) struct Slot {
    pub(crate) id: u64,
    pub(crate) record: Range<usize>,
    pub(crate) fds: Vec<OwnedFd>,
}
