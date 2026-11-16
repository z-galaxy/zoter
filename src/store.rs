//! The file descriptor store of a service, through which it hands over to its next instance:
//! [`Store`].

use std::{
    collections::{BTreeSet, HashMap},
    env, io,
    ops::Range,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    time::Duration,
};

use rustix::{
    io::{Errno, fcntl_dupfd_cloexec},
    pipe::{PipeFlags, pipe_with},
};

use crate::{
    Error, Handover, Notifier, Passed, Restored, Result, Save, Stored,
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
/// such as one that a client sent twice, go to the next instance as two.
///
/// # Limits
///
/// A hand-over is stored whole, or [`Handover::store`] fails, and it is taken over whole, or
/// [`take_over`](Self::take_over) fails: nothing of it is left out. The next instance takes the
/// file descriptors out of the queues under its soft limit of open files, which
/// [`raise_fd_limit`](crate::raise_fd_limit) raises to the hard one, and fails to take over if
/// they don't fit under it. The store keeps a few file descriptors open from its creation on, which
/// it gives up for those that storing a hand-over opens, so that storing can't fail for want of
/// them, however many the service has open by then.
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
    pub fn from_env(key: &str) -> Result<Option<Self>> {
        let Some(notifier) = Notifier::from_env()? else {
            return Ok(None);
        };

        Self::new(notifier, key, fd_store_max()).map(Some)
    }

    /// The store of the service that `notifier` reaches, taking `max` file descriptors, as
    /// `FileDescriptorStoreMax=` of the unit sets it, under `key`.
    ///
    /// The key is 1 to 200 printable ASCII characters other than `:` and `/`. For any other key,
    /// this fails with [`Error::InvalidName`].
    pub fn new(notifier: Notifier, key: &str, max: Option<usize>) -> Result<Self> {
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
    /// It returns `None` if nothing was handed over. Otherwise it returns the whole hand-over: the
    /// state and every item, with all of their file descriptors.
    ///
    /// It fails with [`Error::Io`] of the system's error for too many open files (`EMFILE`) if the
    /// file descriptors don't fit under the process's soft limit of open files.
    /// [`raise_fd_limit`](crate::raise_fd_limit) raises that limit, up to the hard limit that the
    /// service's `LimitNOFILE=` sets. It fails with [`Error::InvalidHandover`] for a hand-over that
    /// this version of zoter can't make sense of. That includes file descriptors of the state or
    /// of an item that are missing for any reason other than that limit. It fails with
    /// [`Error::UnsupportedHandover`] for a hand-over that a later version of zoter laid out. On
    /// failure, the file descriptors of the hand-over are closed, which disconnects the peers of
    /// the items.
    pub fn take_over(&mut self, passed: &mut Passed) -> Result<Option<Restored>> {
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

        let mut fds = take_queued(queues)?;
        let state_fds = take_fds(&mut fds, 0..contents.state_fds).ok_or_else(|| {
            Error::InvalidHandover(
                "file descriptors of the state of the hand-over are missing".to_owned(),
            )
        })?;
        let mut items = Vec::with_capacity(contents.entries.len());
        for Entry {
            id,
            record,
            fds: indexes,
        } in contents.entries
        {
            let Some(fds) = take_fds(&mut fds, indexes) else {
                return Err(Error::InvalidHandover(format!(
                    "file descriptors of item {id} of the hand-over are missing"
                )));
            };
            items.push(Slot { id, record, fds });
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
        }))
    }

    /// Start a hand-over to the next instance through the store, which
    /// [stores](Handover::store) the state of the service and its items once the service stopped
    /// taking requests.
    ///
    /// It fails if the store can't take a hand-over. The error says why, and which setting of the
    /// unit to change. It fails with [`Error::NoStore`] if systemd keeps no store for the service.
    /// It fails with [`Error::StoreTooSmall`] if the store takes fewer than 2 file descriptors, the
    /// minimum for a hand-over: the memfd and a queue socket. It logs nothing, and the service has
    /// not stopped taking requests yet. So a service that can't hand over can shut down as it
    /// would without systemd. Either way, this consumes the store and its
    /// [notifier](Self::notifier), unless the service cloned the notifier.
    pub fn handover(self) -> Result<Handover> {
        match self.max {
            Some(max) if max >= MIN_STORE => Ok(Handover::new(self)),
            Some(max) => Err(Error::StoreTooSmall { max }),
            None => Err(Error::NoStore),
        }
    }

    /// Store `state` and `items`, as [`Handover::store`] tells.
    pub(crate) fn hand_over(
        mut self,
        state: &dyn Save,
        items: &[(u64, &dyn Save)],
    ) -> Result<Stored> {
        // The store takes the memfd, and as many queue sockets as it has room for besides: one at
        // least, as `handover` checked.
        let room = self.max.unwrap_or(MIN_STORE) - 1;
        // The file descriptors kept from the start are given up for those that storing opens, each
        // closed before the next opens: the memfd, the two ends of each pair whose one end is a
        // queue socket, and the two ends of the pipe of the barrier.
        self.reserve = None;

        let state_len = state.saved_len().ok_or(Error::Unsaved {
            item: None,
            source: None,
        })?;
        let lens = items
            .iter()
            .map(|(id, item)| {
                item.saved_len().ok_or(Error::Unsaved {
                    item: Some(*id),
                    source: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let state_fds = state.fds();
        let item_fds: Vec<_> = items.iter().map(|(_, item)| item.fds()).collect();

        let layout = Layout::new(state_len, lens.into_iter())?;
        {
            let name = self.name(STATE);
            let memfd = memfd::filled(&name, layout.len(), |memfd| -> Result<()> {
                state
                    .save(&mut memfd[layout.state()])
                    .map_err(|e| Error::Unsaved {
                        item: None,
                        source: Some(Box::new(e)),
                    })?;
                let mut entries = Vec::with_capacity(items.len());
                let mut next = state_fds.len() as u64;
                for (place, ((id, item), fds)) in items.iter().zip(&item_fds).enumerate() {
                    item.save(&mut memfd[layout.item(place)])
                        .map_err(|e| Error::Unsaved {
                            item: Some(*id),
                            source: Some(Box::new(e)),
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
            self.notifier.store(&name, &[memfd.as_fd()])?;
        }

        let fds: Vec<_> = state_fds
            .iter()
            .chain(item_fds.iter().flatten())
            .copied()
            .collect();
        let name = self.name(FDS);
        queue_fds(fds.chunks(MAX_FDS), room, |socket| {
            self.notifier.store(&name, &[socket.as_fd()])
        })?;

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
    /// For what storing opens, in turn, each closed before the next opens: the memfd, the two ends
    /// of each pair whose one end is a queue socket, and the two ends of the pipe of the barrier.
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
/// It fails with [`Error::StoreFull`] if `room` queue sockets can't hold all of them. If queueing
/// them fails, it fails with that error. The queue sockets that `store` received up to then stay
/// stored.
fn queue_fds<'c, 'f, C, S>(chunks: C, room: usize, mut store: S) -> Result<()>
where
    'f: 'c,
    C: IntoIterator<Item = &'c [BorrowedFd<'f>]>,
    S: FnMut(OwnedFd) -> Result<()>,
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
            )
            .into());
        }
        store(queue.into_socket())?;
    }
    if chunks.peek().is_none() {
        return Ok(());
    }

    Err(Error::StoreFull)
}

/// The file descriptors out of the queue sockets that hold them, `queues`, as [`queue::take`]
/// takes them, by the index that each was queued as, whatever the order of the queue sockets.
///
/// It fails with the system's error of too many open files (`EMFILE`) if the kernel closed any of
/// them, as it does with those that the process can't have open. It warns of a queue socket that
/// fails to give them, whose file descriptors from there on close with it, and are missing then.
fn take_queued(queues: Vec<OwnedFd>) -> Result<HashMap<u64, OwnedFd>> {
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
    if !complete {
        warn!(
            "Failed to take file descriptors of the hand-over out of the store, as the process has \
             too many open: raise the hard limit of `LimitNOFILE=` of the service."
        );

        return Err(Error::os(Errno::MFILE));
    }

    Ok(fds)
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
fn check_key(key: &str) -> Result<()> {
    let valid = |b: u8| (b' '..=b'~').contains(&b) && b != b':' && b != b'/';
    if key.is_empty() || key.len() > MAX_KEY_LEN || !key.bytes().all(valid) {
        return Err(Error::InvalidName(format!(
            "a key is 1 to {MAX_KEY_LEN} printable ASCII characters, other than `:` and `/`"
        )));
    }

    Ok(())
}

/// The fewest file descriptors that the store has to take for a hand-over: the memfd and a queue
/// socket.
pub(crate) const MIN_STORE: usize = 2;
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
