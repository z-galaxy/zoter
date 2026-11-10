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
    process::{Resource, getrlimit},
};

use crate::{
    Handover, LeftOut, Notifier, Passed, Restored, Save, Stored,
    layout::{self, Entry, Layout},
    memfd,
    notifier::MAX_FDS,
    passed::START,
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
/// The next instance takes the file descriptors out of the queues under its soft limit of open
/// files, which [`raise_fd_limit`](crate::raise_fd_limit) raises to the hard one, so
/// [`Handover::store`] leaves out the items that don't fit under the limit that this process had
/// as the store was created: those of a client that don't fit get disconnected, rather than all of
/// them. The store keeps a few file descriptors open from its creation on, which it gives up for
/// those that storing a hand-over opens, so that storing can't fail for want of them, however many
/// the service has open by then.
#[derive(Debug)]
pub struct Store {
    notifier: Notifier,
    key: String,
    /// How many file descriptors the store takes, `$FDSTORE`.
    max: Option<usize>,
    /// The soft limit of open file descriptors that the next instance is taken to have.
    fd_limit: Option<u64>,
    /// How many file descriptors systemd passed besides those of the hand-over, as `take_over`
    /// found them: the next instance is passed them too.
    others: usize,
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
            fd_limit: getrlimit(Resource::Nofile).current,
            others: 0,
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

    /// Set the soft limit of open file descriptors that the next instance is taken to have,
    /// `None` for none: the one that this process had as the store was created, unless set
    /// otherwise.
    pub fn set_fd_limit(&mut self, limit: Option<u64>) {
        self.fd_limit = limit;
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
    /// hand-over that this version of zoter can't make sense of, and with
    /// [`ErrorKind::Unsupported`] for one that a later version of zoter laid out otherwise. The
    /// file descriptors of the hand-over close then, which disconnects the peers of the items.
    /// The items whose file descriptors aren't all there are left out, as
    /// [`Restored::left_out`] tells.
    pub fn take_over(&mut self, passed: &mut Passed) -> io::Result<Option<Restored>> {
        let prefix = format!("{}/", self.key);
        let stored = passed.take_prefixed(&prefix);
        self.others = passed.len();
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

    /// Store `state` and `items`, as [`Handover::store`] tells, with as many file descriptors
    /// extra per item, and as many spare ones, in the next instance.
    pub(crate) fn hand_over(
        mut self,
        state: &dyn Save,
        items: &[(u64, &dyn Save)],
        extra_fds_per_item: usize,
        spare_fds: usize,
    ) -> io::Result<Stored> {
        // The store takes the memfd, and as many queue sockets as it has room for besides: one at
        // least, as `handover` checked.
        let room = self.max.unwrap_or(MIN_STORE) - 1;
        // The file descriptors kept from the start are given up for those that storing opens, each
        // closed before the next opens: the memfd, the two ends of each pair whose one end is a
        // queue socket, and the two ends of the pipe of the barrier.
        self.reserve = None;

        let state_len = state.saved_len().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "the state of the hand-over can't be saved",
            )
        })?;
        let state_fds = state.fds();
        let all = items.len();
        let mut left_out = Vec::new();
        let mut kept = Vec::with_capacity(all);
        for (index, (id, item)) in items.iter().enumerate() {
            match item.saved_len() {
                Some(len) => kept.push(Kept {
                    index,
                    id: *id,
                    item: *item,
                    len,
                    fds: item.fds(),
                }),
                None => left_out.push((index, *id, LeftOut::Unsaved)),
            }
        }
        self.fit(
            &mut kept,
            &mut left_out,
            state_fds.len() + spare_fds,
            extra_fds_per_item,
        );

        let layout = Layout::new(state_len, kept.iter().map(|item| item.len))?;
        let mut failed = Vec::new();
        {
            let name = self.name(STATE);
            let memfd = memfd::filled(&name, layout.len(), |memfd| -> io::Result<()> {
                state.save(&mut memfd[layout.state()]).map_err(|e| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("the state of the hand-over failed to be saved: {e}"),
                    )
                })?;
                let mut entries = Vec::with_capacity(kept.len());
                let mut next = state_fds.len() as u64;
                for (place, item) in kept.iter().enumerate() {
                    let record = &mut memfd[layout.item(place)];
                    if let Err(e) = item.item.save(record) {
                        // What it wrote of its record goes, as the table doesn't have it.
                        record.fill(0);
                        failed.push((place, e));

                        continue;
                    }
                    let first = next;
                    next += item.fds.len() as u64;
                    entries.push(Entry {
                        id: item.id,
                        record: layout.item(place),
                        fds: first..next,
                    });
                }
                layout.write(memfd, state_fds.len(), &entries);

                Ok(())
            })?;
            // Stored first, so that a store too small for all of the hand-over keeps what matters
            // most.
            self.notifier.store(&name, &[memfd.as_fd()])?;
        }
        for (place, e) in failed.into_iter().rev() {
            let item = kept.remove(place);
            warn!(
                "Leaving item {} out of the hand-over, as its record failed to be saved: {e}",
                item.id
            );
            left_out.push((item.index, item.id, LeftOut::Save(e)));
        }

        let fds: Vec<_> = state_fds
            .iter()
            .copied()
            .chain(kept.iter().flat_map(|item| item.fds.iter().copied()))
            .collect();
        let name = self.name(FDS);
        let (queued, failure) = queue_fds(fds.chunks(MAX_FDS), room, |socket| {
            self.notifier.store(&name, &[socket.as_fd()])
        })?;
        if queued < state_fds.len() {
            return Err(failure.unwrap_or_else(|| {
                io::Error::other("the store took none of the file descriptors of the state")
            }));
        }
        let mut end = state_fds.len();
        let before = kept.len();
        kept.retain(|item| {
            end += item.fds.len();
            if end <= queued {
                return true;
            }
            let why = match &failure {
                Some(e) => LeftOut::Queue(e.kind()),
                None => LeftOut::Store,
            };
            left_out.push((item.index, item.id, why));

            false
        });
        if kept.len() < before {
            let why = match &failure {
                Some(e) => format!("queueing their file descriptors failed: {e}"),
                None => "the file descriptor store takes no more sockets of their file \
                         descriptors: raise `FileDescriptorStoreMax=` of the service"
                    .to_owned(),
            };
            warn!("Handing over {} of the {all} items, as {why}.", kept.len());
        }

        // The service manager ties what it gets to the service by the process that sent it, which
        // goes next.
        self.notifier.barrier(BARRIER_TIMEOUT)?;
        debug!(
            "Stored {} items and {queued} file descriptors under `{}` for the next instance",
            kept.len(),
            self.key
        );
        left_out.sort_by_key(|(index, _, _)| *index);

        Ok(Stored {
            items: kept.len(),
            fds: queued,
            left_out: left_out.into_iter().map(|(_, id, why)| (id, why)).collect(),
        })
    }

    /// Keep the items of `kept` that fit, in order, under the soft limit of open file descriptors
    /// that the next instance is taken to have, if any, with `reserved` of them reserved besides,
    /// and leave the others out, to `left_out`.
    ///
    /// Each item takes its file descriptors, and `extra` more in the next instance. One that
    /// doesn't fit is left out whole, as the next instance couldn't make anything of it without all
    /// of its file descriptors, while those after it, of fewer, may still fit.
    fn fit(
        &self,
        kept: &mut Vec<Kept<'_>>,
        left_out: &mut Vec<(usize, u64, LeftOut)>,
        reserved: usize,
        extra: usize,
    ) {
        let Some(limit) = self.fd_limit.and_then(|limit| usize::try_from(limit).ok()) else {
            return;
        };
        let all = kept.len();
        // The next instance has the standard input, output and error open, the other file
        // descriptors that systemd passes it, the memfd and the queue sockets, as many as the store
        // takes at most, and `reserved`.
        let taken = START as usize + self.others + self.max.unwrap_or(0) + reserved;
        let mut room = limit.saturating_sub(taken);
        kept.retain(|item| {
            let left = item
                .fds
                .len()
                .checked_add(extra)
                .and_then(|needed| room.checked_sub(needed));
            let Some(left) = left else {
                left_out.push((item.index, item.id, LeftOut::FdLimit));

                return false;
            };
            room = left;

            true
        });
        if kept.len() < all {
            warn!(
                "Handing over {} of the {all} items that can be, as the next instance takes their \
                 file descriptors under its limit of open file descriptors, {limit} as this one \
                 has it: raise the hard limit of `LimitNOFILE=` of the service.",
                kept.len()
            );
        }
    }

    /// The name that the store keeps the file descriptors of the hand-over that `kind` tells
    /// under.
    fn name(&self, kind: &str) -> String {
        format!("{}/{kind}", self.key)
    }
}

/// An item of a hand-over that is to be stored, as far as it fits.
struct Kept<'h> {
    /// Its place among the items of the hand-over.
    index: usize,
    id: u64,
    item: &'h dyn Save,
    /// How long its record is.
    len: usize,
    fds: Vec<BorrowedFd<'h>>,
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
/// as many as that takes, `room` at most, and hand each one that took any datagram to `store`,
/// once it took all that it takes. Tell how many file descriptors that queued, and why not all of
/// them, if not: queueing failed, or else, `room` queue sockets took no more.
fn queue_fds<'c, 'f, C, S>(
    chunks: C,
    room: usize,
    mut store: S,
) -> io::Result<(usize, Option<io::Error>)>
where
    'f: 'c,
    C: IntoIterator<Item = &'c [BorrowedFd<'f>]>,
    S: FnMut(OwnedFd) -> io::Result<()>,
{
    let mut chunks = chunks.into_iter().peekable();
    let mut queued = 0;
    for _ in 0..room {
        if chunks.peek().is_none() {
            break;
        }
        let queue = Queue::new()?;
        let mut took = false;
        let mut failure = None;
        while let Some(chunk) = chunks.peek() {
            match queue.push(queued as u64, chunk) {
                Ok(true) => queued += chunk.len(),
                // The next queue takes the rest.
                Ok(false) => break,
                Err(e) => {
                    failure = Some(e);

                    break;
                }
            }
            took = true;
            chunks.next();
        }
        if took {
            store(queue.into_socket())?;
        } else if failure.is_none() {
            // Any other queue would take none either.
            failure = Some(io::Error::other("a socket took none of them"));
        }
        if failure.is_some() {
            return Ok((queued, failure));
        }
    }

    Ok((queued, None))
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
