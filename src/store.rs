//! The file descriptor store of a service, through which it hands over to its next instance:
//! [`Store`].

use std::{
    collections::{BTreeSet, HashMap},
    env,
    io::{self, ErrorKind},
    ops::Range,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    time::Duration,
};

use rustix::{
    io::fcntl_dupfd_cloexec,
    pipe::{PipeFlags, pipe_with},
};

use crate::{
    Handover, Notifier, Passed, Restored, Save, Stored,
    layout::{self, Entry, Layout},
    memfd,
    notifier::MAX_FDS,
    queue::{self, Queue},
    restored::Slot,
};

/// The file descriptor store of a service, through which the service hands over to its next
/// instance under a key of its own.
///
/// systemd keeps the store for the service across its restarts: what the service stores in it,
/// systemd passes the next instance as it starts it, along with the sockets of socket activation.
/// [`handover`](Self::handover) starts a [`Handover`], which stores what the service hands over as
/// it stops, and [`take_over`](Self::take_over) gives it back in the next instance, under the same
/// key, as a [`Restored`]. Several keys keep several hand-overs apart, in one store.
///
/// The unit of the service needs `FileDescriptorStoreMax=` of 2 at least, for systemd to keep a
/// store, and systemd 254 or later, which tells the service how many file descriptors the store
/// takes, in `$FDSTORE`. A socket unit has systemd keep the socket that the service listens on,
/// and the clients that connect meanwhile, while no instance runs.
///
/// # What the store holds
///
/// A hand-over takes a memfd, stored under `<key>/state`, that holds the record of the state and
/// those of the items, each of which the next instance reads in place, and one or more sockets,
/// stored under `<key>/fds`, whose queues hold the file descriptors of the state and of the items:
/// some 70000 in each, with the default size of its send buffer. The store takes each as one, so
/// the hand-over takes few of its file descriptors, and few names in `$LISTEN_FDNAMES`, which Linux
/// takes 128 KiB of at most, however many file descriptors it holds. The store keeps one file
/// descriptor of each open file description, while a queue keeps each as it is, so two of one file,
/// such as one that a client sent twice, go to the next instance as two. The memfd is stored last,
/// once the file descriptors are, so that the store has no hand-over in it, but for queue sockets
/// that the next instance closes, until all of it is there.
///
/// # Limits
///
/// A hand-over is stored whole, or [`Handover::store`] fails: nothing predicts what fits in the
/// next instance. The next instance takes the file descriptors out of the queues under its soft
/// limit of open files, which [`raise_fd_limit`](crate::raise_fd_limit) raises to the hard one,
/// and [`take_over`](Self::take_over) leaves out whole the items whose file descriptors don't all
/// come, as [`Restored::left_out`] tells, rather than any part of one. The store keeps a few file
/// descriptors open from its creation on, which it gives up for those that storing a hand-over
/// opens, so that storing can't fail for want of them, however many the service has open by then.
#[derive(Debug)]
pub struct Store {
    notifier: Notifier,
    key: String,
    /// How many file descriptors the store takes, `$FDSTORE`.
    max: Option<usize>,
    /// The file descriptors kept to give up for those that storing opens.
    reserve: Option<Reserve>,
}

impl Store {
    /// The store of the service that `$NOTIFY_SOCKET` names, taking as many file descriptors as
    /// `$FDSTORE` tells, under `key`, if `$NOTIFY_SOCKET` is set.
    ///
    /// `None` means that systemd didn't start the service, or that the service takes no
    /// notifications. It fails as [`Notifier::from_env`] and [`Store::new`] do.
    pub fn from_env(key: &str) -> io::Result<Option<Self>> {
        let Some(notifier) = Notifier::from_env()? else {
            return Ok(None);
        };

        Self::new(notifier, key, fd_store_max()).map(Some)
    }

    /// The store of the service that `notifier` reaches, taking `max` file descriptors, as
    /// `FileDescriptorStoreMax=` of the unit sets it, under `key`.
    ///
    /// The key is 1 to 200 printable ASCII characters, other than `:` and `/`: it fails with
    /// [`ErrorKind::InvalidInput`] otherwise.
    pub fn new(notifier: Notifier, key: &str, max: Option<usize>) -> io::Result<Self> {
        check_key(key)?;
        let reserve = match max {
            Some(max) if max >= MIN_STORE => Reserve::new()
                .inspect_err(|e| warn!("Failed to keep file descriptors to hand over with: {e}"))
                .ok(),
            _ => None,
        };

        Ok(Self {
            notifier,
            key: key.to_owned(),
            max,
            reserve,
        })
    }

    /// The notification socket of the service manager, for the service to tell it that it's
    /// ready, among other things.
    pub fn notifier(&self) -> &Notifier {
        &self.notifier
    }

    /// How many file descriptors the store takes, if systemd keeps one.
    pub fn max(&self) -> Option<usize> {
        self.max
    }

    /// Take over from the previous instance: take the file descriptors stored under the key out
    /// of `passed`, remove them from the store, and read the hand-over that they hold, if any.
    ///
    /// They are removed from the store first, whatever comes of it, so that a hand-over that this
    /// instance fails on, or crashes on, isn't found by the one after it. The file descriptors
    /// under the key that this version of zoter doesn't know of, as those that another version
    /// stored, close. What is left in `passed` is the sockets of socket activation, and what the
    /// service stored itself, if anything.
    ///
    /// It gives `None` if nothing was handed over, and fails with [`ErrorKind::InvalidData`] for a
    /// hand-over that this version of zoter can't make sense of, file descriptors of the state
    /// that aren't all there included, and with [`ErrorKind::Unsupported`] for one that a later
    /// version of zoter laid out otherwise. The file descriptors of the hand-over close then,
    /// which disconnects the peers of the items.
    ///
    /// An item whose file descriptors aren't all there is left out whole, as
    /// [`Restored::left_out`] tells, and those of its file descriptors that are there close, while
    /// the take-over goes on with the next item. The kernel closes the file descriptors that the
    /// process can't have, as when it has as many open as its soft limit of open files takes,
    /// which [`raise_fd_limit`](crate::raise_fd_limit) raises to the hard one that `LimitNOFILE=`
    /// of the service sets. The file descriptors are queued in the order of the items, so in a
    /// hand-over of one queue socket a limit too low costs the last items; with several, the
    /// queue sockets come in no fixed order, and so do the items that it costs.
    pub fn take_over(&mut self, passed: &mut Passed) -> io::Result<Option<Restored>> {
        let prefix = format!("{}/", self.key);
        let stored = passed.take_prefixed(&prefix);
        // More than one file descriptor may come under a name, as the queue sockets do.
        let names: BTreeSet<_> = stored.iter().map(|(name, _)| name.as_str()).collect();
        for name in names {
            if let Err(e) = self.notifier.remove(name) {
                warn!("Failed to remove `{name}` from the file descriptor store: {e}");
            }
        }

        let mut state = None;
        let mut queues = Vec::new();
        for (name, fd) in stored {
            match &name[prefix.len()..] {
                STATE if state.is_none() => state = Some(fd),
                FDS => queues.push(fd),
                _ => {
                    warn!("Closing `{name}` of the file descriptor store, which zoter doesn't know")
                }
            }
        }
        let Some(state) = state else {
            if !queues.is_empty() {
                warn!("Closing the file descriptors that were stored without their hand-over");
            }

            return Ok(None);
        };
        let view = memfd::view(state.as_fd())?;
        let contents = layout::read(&view)?;

        let (mut fds, complete) = take_queued(queues);
        if !complete {
            warn!(
                "Failed to take file descriptors of the hand-over out of the store, as when the \
                 process has too many open: raise the hard limit of `LimitNOFILE=` of the service."
            );
        }
        let state_fds = take_fds(&mut fds, 0..contents.state_fds).ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                "file descriptors of the state of the hand-over are missing",
            )
        })?;
        let mut items = Vec::with_capacity(contents.entries.len());
        let mut left_out = Vec::new();
        for Entry {
            id,
            record,
            fds: indexes,
        } in contents.entries
        {
            match take_fds(&mut fds, indexes) {
                Some(fds) => items.push(Slot { id, record, fds }),
                None => left_out.push(id),
            }
        }
        if !left_out.is_empty() {
            warn!(
                "Leaving {} items of the hand-over out, as file descriptors of theirs are missing \
                 from the store",
                left_out.len()
            );
        }
        if !fds.is_empty() {
            warn!(
                "Closing {} file descriptors of the store that no item of the hand-over has",
                fds.len()
            );
        }

        Ok(Some(Restored {
            view,
            state: contents.state,
            state_fds,
            items,
            left_out,
        }))
    }

    /// Start a hand-over to the next instance through the store, which
    /// [stores](Handover::store) the state of the service and its items once the service stopped
    /// taking requests.
    ///
    /// It fails with [`ErrorKind::Unsupported`] if the store can't take a hand-over, saying why,
    /// and which setting of the unit to change: systemd keeps no store for the service, or one that
    /// takes fewer than the 2 file descriptors that a hand-over takes at least, the memfd and a
    /// queue socket. It logs nothing, so that a service that can't hand over shuts down as it does
    /// without systemd, rather than stop taking requests to hand over. The store goes either way,
    /// and its [notifier](Self::notifier) with it, unless the service cloned it.
    pub fn handover(self) -> io::Result<Handover> {
        match self.max {
            Some(max) if max >= MIN_STORE => Ok(Handover::new(self)),
            Some(max) => Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "the file descriptor store takes {max}, which is too few: raise \
                     `FileDescriptorStoreMax=` of the service to {MIN_STORE} at least"
                ),
            )),
            None => Err(io::Error::new(
                ErrorKind::Unsupported,
                format!(
                    "systemd keeps no file descriptor store for the service, as `$FDSTORE` isn't \
                     set: set `FileDescriptorStoreMax=` of the service to {MIN_STORE} at least, \
                     under systemd 254 or later"
                ),
            )),
        }
    }

    /// Store `state` and `items`, as [`Handover::store`] tells.
    pub(crate) fn hand_over(
        mut self,
        state: &dyn Save,
        items: &[(u64, &dyn Save)],
    ) -> io::Result<Stored> {
        // The store takes the memfd, and as many queue sockets as it has room for besides: one at
        // least, as `handover` checked.
        let room = self.max.unwrap_or(MIN_STORE) - 1;
        // The file descriptors kept from the start are given up for those that storing opens, 3 at
        // most at a time: the memfd, with the two ends of each pair whose one end is a queue
        // socket, and then the two ends of the pipe of the barrier.
        self.reserve = None;

        let state_len = state.saved_len().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "the state of the hand-over can't be saved",
            )
        })?;
        let lens = items
            .iter()
            .map(|(id, item)| {
                item.saved_len().ok_or_else(|| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("item {id} of the hand-over can't be saved"),
                    )
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let state_fds = state.fds();
        let item_fds: Vec<_> = items.iter().map(|(_, item)| item.fds()).collect();

        let layout = Layout::new(state_len, lens.into_iter())?;
        let fds: Vec<_> = state_fds
            .iter()
            .chain(item_fds.iter().flatten())
            .copied()
            .collect();
        {
            let state_name = self.name(STATE);
            let memfd = memfd::filled(&state_name, layout.len(), |memfd| -> io::Result<()> {
                state.save(&mut memfd[layout.state()]).map_err(|e| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("the state of the hand-over failed to be saved: {e}"),
                    )
                })?;
                let mut entries = Vec::with_capacity(items.len());
                let mut next = state_fds.len() as u64;
                for (place, ((id, item), fds)) in items.iter().zip(&item_fds).enumerate() {
                    item.save(&mut memfd[layout.item(place)]).map_err(|e| {
                        io::Error::new(
                            ErrorKind::InvalidInput,
                            format!(
                                "the record of item {id} of the hand-over failed to be saved: {e}"
                            ),
                        )
                    })?;
                    let first = next;
                    next += fds.len() as u64;
                    entries.push(Entry {
                        id: *id,
                        record: layout.item(place),
                        fds: first..next,
                    });
                }
                layout.write(memfd, state_fds.len(), &entries);

                Ok(())
            })?;
            let name = self.name(FDS);
            queue_fds(fds.chunks(MAX_FDS), room, |socket| {
                self.notifier.store(&name, &[socket.as_fd()])
            })?;
            // Stored last, so that a hand-over that fails before then leaves only queue sockets in
            // the store, which the next instance takes nothing over from.
            self.notifier.store(&state_name, &[memfd.as_fd()])?;
        }

        // The service manager ties what it gets to the service by the process that sent it, which
        // goes next.
        self.notifier.barrier(BARRIER_TIMEOUT)?;
        debug!(
            "Stored {} items and {} file descriptors under `{}` for the next instance",
            items.len(),
            fds.len(),
            self.key
        );

        Ok(Stored {
            items: items.len(),
            fds: fds.len(),
        })
    }

    /// The name that the store keeps the file descriptors of the hand-over that `kind` tells
    /// under.
    fn name(&self, kind: &str) -> String {
        format!("{}/{kind}", self.key)
    }
}

/// File descriptors that a store keeps from its creation on, to give up for those that storing a
/// hand-over opens: however many file descriptors the service has open by then, it has those to
/// spare.
#[derive(Debug)]
struct Reserve {
    /// For what storing opens, 3 at most at a time: the memfd, which stays open while the two ends
    /// of each pair whose one end is a queue socket open and close in turn, and then the two ends
    /// of the pipe of the barrier, once the memfd closed.
    _storing: [OwnedFd; 3],
}

impl Reserve {
    /// Three file descriptors to spare: the ends of a pipe, and a duplicate of one of them.
    fn new() -> io::Result<Self> {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
        let duplicate = fcntl_dupfd_cloexec(&read, 0)?;

        Ok(Self {
            _storing: [read, write, duplicate],
        })
    }
}

/// Queue the file descriptors of `chunks`, in order, each chunk as one datagram, in queue sockets,
/// as many as that takes, `room` at most, and hand each one to `store`, once it took all that it
/// takes.
///
/// It fails with [`ErrorKind::StorageFull`] if `room` queue sockets don't take all of them, and
/// with the error of queueing them if that fails. The queue sockets that `store` got by then stay
/// stored.
fn queue_fds<'c, 'f, C, S>(chunks: C, room: usize, mut store: S) -> io::Result<()>
where
    'f: 'c,
    C: IntoIterator<Item = &'c [BorrowedFd<'f>]>,
    S: FnMut(OwnedFd) -> io::Result<()>,
{
    let mut chunks = chunks.into_iter().peekable();
    let mut index = 0;
    for _ in 0..room {
        if chunks.peek().is_none() {
            return Ok(());
        }
        let queue = Queue::new()?;
        let mut took = false;
        while let Some(chunk) = chunks.peek() {
            let pushed = queue.push(index, chunk).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("the file descriptors of the hand-over failed to be queued: {e}"),
                )
            })?;
            if !pushed {
                // The next queue takes the rest.
                break;
            }
            index += chunk.len() as u64;
            took = true;
            chunks.next();
        }
        if !took {
            // Any other queue would take none either.
            return Err(io::Error::other(
                "a queue socket took none of the file descriptors of the hand-over",
            ));
        }
        store(queue.into_socket())?;
    }
    if chunks.peek().is_none() {
        return Ok(());
    }

    Err(io::Error::new(
        ErrorKind::StorageFull,
        format!(
            "the file descriptor store takes too few file descriptors for those of the hand-over, \
             {room} queue sockets besides the memfd: raise `FileDescriptorStoreMax=` of the service"
        ),
    ))
}

/// The file descriptors out of the queue sockets that hold them, `queues`, as [`queue::take`]
/// takes them, by the index that each was queued as, whatever the order of the queue sockets, and
/// whether all of them came. Those that the process can't have open close, as do those of a queue
/// socket that fails to give them, with a warning.
fn take_queued(queues: Vec<OwnedFd>) -> (HashMap<u64, OwnedFd>, bool) {
    let mut fds = HashMap::new();
    let mut complete = true;
    for queue in queues {
        let taken = queue::take(queue.as_fd(), |index, taken, all| {
            for (n, fd) in (0..).zip(taken) {
                fds.insert(index.wrapping_add(n), fd);
            }
            complete &= all;
        });
        if let Err(e) = taken {
            warn!("Failed to take file descriptors of the hand-over out of the store: {e}");
        }
    }

    (fds, complete)
}

/// Take the file descriptors at `indexes` out of `fds`, if they are all there.
///
/// Those that are there go either way, so that those of an item that lacks one close with it.
fn take_fds(fds: &mut HashMap<u64, OwnedFd>, indexes: Range<u64>) -> Option<Vec<OwnedFd>> {
    // More than there are can't all be there, and aren't looked for one by one.
    if indexes.end.saturating_sub(indexes.start) > fds.len() as u64 {
        // Those that are there go all the same.
        fds.retain(|index, _| !indexes.contains(index));

        return None;
    }
    let taken: Vec<_> = indexes.map(|index| fds.remove(&index)).collect();

    taken.into_iter().collect()
}

/// How many file descriptors the store of the service takes, `$FDSTORE`, if it has one.
///
/// systemd sets it to the `FileDescriptorStoreMax=` of the unit when that isn't 0, from version 254
/// on. It is `None` if the variable is unset, 0, or not a number.
fn fd_store_max() -> Option<usize> {
    parse_store_max(env::var("FDSTORE").ok().as_deref())
}

/// The value of `$FDSTORE`, if it's a number that isn't 0.
fn parse_store_max(value: Option<&str>) -> Option<usize> {
    value?.parse().ok().filter(|max| *max > 0)
}

/// Check that `key` is one that the names of the store can start with: 1 to [`MAX_KEY_LEN`]
/// printable ASCII characters, other than `:`, which no name of the store has, and `/`, which
/// comes after the key in a name.
fn check_key(key: &str) -> io::Result<()> {
    let valid = |b: u8| (b' '..=b'~').contains(&b) && b != b':' && b != b'/';
    if key.is_empty() || key.len() > MAX_KEY_LEN || !key.bytes().all(valid) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "a key is 1 to {MAX_KEY_LEN} printable ASCII characters, other than `:` and `/`"
            ),
        ));
    }

    Ok(())
}

/// The fewest file descriptors that the store has to take for a hand-over: the memfd and a queue
/// socket.
const MIN_STORE: usize = 2;
/// How long a hand-over waits for the service manager to deal with what it stored.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest key.
const MAX_KEY_LEN: usize = 200;
/// What the name of the memfd of a hand-over ends with, after the key and a `/`.
const STATE: &str = "state";
/// What the name of each queue socket of a hand-over ends with, after the key and a `/`.
const FDS: &str = "fds";

#[cfg(test)]
mod tests;
