# zoter — design

A crate for services that systemd runs to restart without dropping their clients: the instance
that stops hands its state and the connections of its clients over to the next one, through the
file descriptor store (the store) that systemd keeps for the service across its restarts. It is
what busd does today, taken out of busd so that any service can do it, zlink-based ones first,
and the interface through which zbus and zlink hand their connections over.

## The problem

busd restarts without its clients noticing anything but a pause. Most of the code that does that
is not about D-Bus at all:

- `src/systemd.rs` (busd): the protocols of systemd, which its module docs say have "nothing of
  the bus in them": the file descriptors (FDs) that systemd passes (`take_listen_fds`), the
  notification socket (`Notifier`, with `notify`, `store`, `remove` and `barrier`),
  `$FDSTORE` (`fd_store_max`), sealed memfds that are read in place (`memfd`, `memfd_filled`,
  `view_memfd`), and the sockets whose queues hold any number of FDs (`FdQueue`,
  `take_queued`).
- `src/bus/systemd.rs` (busd): how a hand-over is laid out in the store, and the rules that keep
  it within the limits of systemd and of the next instance: one memfd that holds the state and
  the record of each client, each record where the state says, read in place; the FDs of the
  clients spread over as many queue sockets as the store has room for; the clients fitted under
  the limit of open FDs of the next instance (`fit`); FDs reserved from the start, so that
  storing can't fail for want of them (`Reserve`); the stored FDs told apart from the sockets of
  socket activation by their names, and removed from the store as the next instance takes them;
  and a barrier before the process exits.
- `tests/fdstore.rs` (busd): a fake service manager, which owns the notification socket, keeps
  the store, and starts busd as systemd does, with `$LISTEN_FDS` and the passed FDs.

What is about D-Bus is the state of the bus (`src/bus/handover.rs`), how the bus pauses and
settles (`src/peers/handover.rs`), and the hand-over of each connection, which zbus does
(`zbus::connection::Handover`): it takes a connection apart into a record of bytes and FDs
(`saved_len`, `save`, `fds`), and puts it back together from them (`restore`). That pair is the
interface between a protocol and the store: zlink needs the same for Varlink, and the store
needs nothing else of either.

Nothing of it has shipped yet (busd's commit 53d15a5 says no release hands over), so the names it
stores under, the layout of the memfd, busd's state and zbus's API can all change.

## Goals

- Give a service everything of a restart through the store that is not about its own protocol:
  the passed FDs, the notifications, the layout of a hand-over in the store, and its limits.
- Define the interface between a protocol and the store once, as two traits, `Save` and
  `Restore`, which zbus and zlink implement for their hand-overs of connections, so that a
  service hands those over as they are, and so that each protocol has one API for its records.
- Keep what busd learnt: the FDs of a hand-over go in queue sockets (so the store takes any
  number of them, each FD as it is, and `$LISTEN_FDNAMES` stays short, z-galaxy/busd#438); the
  state is in one sealed memfd that is written and read in place; the items that don't fit under
  the limit of open FDs of the next instance are left out whole; storing can't fail for want of
  FDs; the next instance removes what it takes from the store before anything else.
- One API for every runtime: it is synchronous, and blocks only at exit, as the barrier waits for
  the service manager. Pausing a server, and timing it out, is the async work of its caller,
  which zbus and zlink do for their services.
- Let services test their restarts without systemd: the fake service manager of busd's tests,
  made generic.

## Non-goals

- The hand-over of a connection itself, which is about the protocol: zbus has it for D-Bus, and
  zlink gets one for Varlink (see "The other crates").
- Signals, timeouts, and pausing a server in general: those take a runtime.
- Restarts that don't go through systemd, such as a service that executes itself anew, or hands
  over to a process that it starts. The layout of a hand-over doesn't depend on the store, so a
  later version could add those.

## The life of a service

```rust
fn main() -> io::Result<()> {
    // The next instance takes the FDs of the hand-over out of the queue sockets under this limit.
    zoter::raise_fd_limit()?;
    // SAFETY: No other thread runs yet, so none reads or writes the environment meanwhile.
    let passed = unsafe { zoter::Passed::from_env() }?;

    runtime.block_on(serve(passed))
}

async fn serve(mut passed: zoter::Passed) -> io::Result<()> {
    // `None` without `$NOTIFY_SOCKET`, as when systemd didn't start the service.
    let mut store = zoter::Store::from_env("example")?;
    // Takes the FDs stored under `example/` out of `passed`, removes them from the store, and
    // reads the hand-over that they hold, if any.
    let restored = match &mut store {
        Some(store) => store.take_over(&mut passed)?,
        None => None,
    };
    // What is left in `passed` is the sockets of socket activation, by the names of their units.
    let listener = passed.take("example.socket");
    let mut server = match restored {
        Some(mut restored) => {
            let state = State::decode(restored.state())?;
            // Each connection comes back as the protocol restores it, from its record and FDs.
            let connections = restored
                .take_items()
                .map(|item| Ok((item.id, item.restore::<Connection>()?)))
                .collect::<io::Result<Vec<_>>>()?;

            Server::restore(state, connections, listener)
        }
        None => Server::new(listener),
    };
    if let Some(store) = &store {
        store.notifier().notify("READY=1")?;
    }

    sigterm.await;
    // Without a store to hand over through, the service shuts down as it does without systemd.
    let Some(store) = store else {
        return server.shut_down().await;
    };
    if let Err(e) = store.check_hand_over() {
        warn!("Not handing over to the next instance: {e}");

        return server.shut_down().await;
    }
    // The service's own async work: stop taking requests, finish those it took, and take each
    // connection apart. Bounded by a timeout of the service's choosing.
    let (state, connections) = server.pause().await?;
    let mut handover = zoter::Handover::with_state(state.encode());
    for (id, connection) in &connections {
        handover.push(*id, connection);
    }
    // Lays the hand-over out in the store, waits for systemd to have dealt with it, and returns.
    let stored = store.hand_over(handover)?;

    Ok(())
}
```

systemd starts the next instance with the sockets of socket activation and what was stored, and
`Store::take_over` gives back what `Store::hand_over` stored. The unit needs
`FileDescriptorStoreMax=` of 2 at least (the state memfd and one queue socket), systemd 254 or
later (for `$FDSTORE`), and a socket unit, so that systemd keeps the listening socket, and the
clients that connect meanwhile, while no instance runs.

## API

All of it is in the crate root. The crate builds nothing on platforms other than Linux, where
systemd is.

### `Save` and `Restore`: what can be handed over

```rust
/// What can be handed over: a record of bytes, and the FDs that go with it.
pub trait Save {
    /// The exact length of the record, or `None` for what can't be saved, as the hand-over of a
    /// connection over a socket that only goes on in this process: a hand-over leaves it out.
    fn saved_len(&self) -> Option<usize>;
    /// Copy the record into `dest`, which is `saved_len` bytes long, at any address, such as its
    /// place in a memfd mapped into memory.
    fn save(&self, dest: &mut [u8]) -> io::Result<()>;
    /// The FDs, in the order that `Restore::restore` takes them.
    fn fds(&self) -> Vec<BorrowedFd<'_>>;
}

/// What can be restored from the record that `Save::save` copied, and the FDs of `Save::fds`.
pub trait Restore: Sized {
    type Error;

    fn restore(record: &[u8], fds: Vec<OwnedFd>) -> Result<Self, Self::Error>;
}
```

The names are those of zbus's methods today, which the traits take the place of. `Save` takes no
generics and returns no `impl Trait`, so that a hand-over can hold items of different types, as
`dyn Save`. It is implemented for `[u8]` and `Vec<u8>` (a record with no FDs), for `OwnedFd` and
`BorrowedFd` (one FD with an empty record), and for `&T` and `Box<T>`; `Restore` for `Vec<u8>`
and `OwnedFd`. Records are copied once, from the item into the memfd, and read back in place:
zbus's records are a multiple of 8 bytes long, and each record starts at a multiple of 8.

### `Passed`: what systemd passed to this process

```rust
/// The FDs that the service manager passed to this process, with their names: the sockets of
/// socket activation, with the `FileDescriptorName=` of their units, and the FDs of the store.
#[derive(Debug, Default)]
pub struct Passed { /* Vec<(String, OwnedFd)> */ }

impl Passed {
    /// Take them as `sd_listen_fds(3)` does, and remove `$LISTEN_PID`, `$LISTEN_FDS`,
    /// `$LISTEN_FDNAMES` and `$LISTEN_PIDFDID` from the environment, so that no later call takes
    /// them again, and no process that this one starts inherits them. They are close-on-exec.
    ///
    /// # Safety
    ///
    /// No other thread may read or write the environment meanwhile, as for
    /// `std::env::remove_var`: call it before the process starts any other thread.
    pub unsafe fn from_env() -> io::Result<Self>;
    /// FDs passed some other way, with their names.
    pub fn new(fds: Vec<(String, OwnedFd)>) -> Self;
    /// Take the first FD named `name`.
    pub fn take(&mut self, name: &str) -> Option<OwnedFd>;
    /// Take every FD named `name`, in their order.
    pub fn take_all(&mut self, name: &str) -> Vec<OwnedFd>;
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, BorrowedFd<'_>)>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn into_vec(self) -> Vec<(String, OwnedFd)>;
}
```

`from_env` is the one `unsafe` function of the API. Edition 2024 makes `std::env::remove_var`
unsafe, as it is unsound while another thread reads the environment, and the alternative of
leaving the variables there lets a later `sd_listen_fds` in the same process, or the process
executing itself anew, take FDs that are long closed, or other ones under their numbers.

### `Notifier`: the notification socket

busd's `Notifier`, as it is: `from_env() -> io::Result<Option<Self>>`, `new(address)`,
`notify(&str)`, `notify_with_fds(&str, &[BorrowedFd])`, `store(name, &[BorrowedFd])` (always with
`FDPOLL=0`, so that systemd doesn't close a stored socket whose peer hung up before the next
instance read what it sent), `remove(name)` and `barrier(Duration)`, plus `try_clone()`. States
stay strings (`"READY=1"`, `"STOPPING=1"`, `"EXTEND_TIMEOUT_USEC=…"`); typed ones can come later.

### `Store`: the store of the service, under a key

```rust
/// The store of the service, for handing it over to its next instance under `key`.
///
/// It keeps a few FDs open from its creation on, which `hand_over` gives up for those that storing
/// opens, so that storing can't fail for want of FDs, however many the service has open by then.
#[derive(Debug)]
pub struct Store { /* notifier, key, $FDSTORE, the FD limit, the reserved FDs */ }

impl Store {
    /// The store of the service that `$NOTIFY_SOCKET` names, taking `$FDSTORE` FDs, if set.
    pub fn from_env(key: &str) -> io::Result<Option<Self>>;
    /// The store of the service that `notifier` reaches, taking `max` FDs.
    pub fn new(notifier: Notifier, key: &str, max: Option<usize>) -> io::Result<Self>;
    pub fn notifier(&self) -> &Notifier;
    /// How many FDs the store takes, as `FileDescriptorStoreMax=` of the unit sets it.
    pub fn max(&self) -> Option<usize>;
    /// Check that `hand_over` can store anything: that the store takes the state memfd and a
    /// queue socket. It fails with `ErrorKind::Unsupported` otherwise, saying why, and which
    /// setting of the unit to change. It logs nothing, and `hand_over` checks the same.
    pub fn check_hand_over(&self) -> io::Result<()>;
    /// The soft limit of open FDs that the next instance is taken to have, `None` for none: the
    /// one that this process had as the store was created, unless set otherwise.
    pub fn set_fd_limit(&mut self, limit: Option<u64>);
    /// Take over from the previous instance: take the FDs stored under the key out of `passed`,
    /// remove them from the store, and read the hand-over that they hold, if any.
    pub fn take_over(&mut self, passed: &mut Passed) -> io::Result<Option<Restored>>;
    /// Store `handover` for the next instance, and wait for systemd to have dealt with it.
    pub fn hand_over(self, handover: Handover<'_>) -> io::Result<Stored>;
}
```

`take_over` removes the FDs from the store before it looks at them, as busd does, so that a
hand-over that the next instance fails on, or crashes on, isn't found by the one after it.

### `Handover`: what a service hands over

```rust
/// What a service hands over: its state, and items, such as connections, under ids of its own.
pub struct Handover<'a> { /* the state, the items, the rules of fitting them */ }

impl<'a> Handover<'a> {
    /// A hand-over whose state is empty.
    pub fn new() -> Self;
    pub fn with_state(state: impl Save + 'a) -> Self;
    /// Hand `item` over under `id`, which the crate doesn't look at: the state can refer to it.
    pub fn push(&mut self, id: u64, item: impl Save + 'a);
    /// How many FDs each item takes in the next instance besides those that it hands over, such
    /// as the pidfd of the peer of a connection that busd opens: none, unless set.
    pub fn set_extra_fds_per_item(&mut self, count: usize);
    /// How many FDs the next instance keeps free, besides the items, for those that it opens of
    /// its own as it starts: 32, as busd has it, unless set.
    pub fn set_spare_fds(&mut self, count: usize);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

`hand_over` fits the items under the limit of open FDs of the next instance, in order, leaving out
whole those that don't fit, as busd's `fit` does. An item that can't be saved, or whose `save`
fails, is left out alone. What was left out, and why, is in the `Stored` that it returns:

```rust
#[derive(Debug)]
pub struct Stored { /* how many items and FDs were stored, which items were left out and why */ }

#[derive(Debug)]
#[non_exhaustive]
pub enum LeftOut {
    /// It can't be saved: its `saved_len` is `None`.
    Unsaved,
    /// Its record failed to be saved.
    Save(io::Error),
    /// Its FDs don't fit under the limit of open FDs of the next instance.
    FdLimit,
    /// Its FDs didn't fit in the store, or failed to be queued.
    Store,
}
```

Nothing is stored of an item that is left out, so its peer is disconnected once its FDs close:
as `hand_over` returns, for an item that it was given by value, and as the caller drops it, for
one that it was given by reference. busd disconnects the clients that don't fit so.

### `Restored`: what the previous instance handed over

```rust
/// What the previous instance handed over: its state, and its items, each with its record and
/// its FDs. The records are read in place, in a mapping of the memfd that goes with this.
#[derive(Debug)]
pub struct Restored { /* the mapped memfd, the table, the FDs */ }

impl Restored {
    pub fn state(&self) -> &[u8];
    pub fn take_state_fds(&mut self) -> Vec<OwnedFd>;
    /// The items whose FDs are all there, in the order that they were handed over.
    pub fn take_items(&mut self) -> impl Iterator<Item = RestoredItem<'_>>;
    /// The ids of the items whose FDs are not all there, which are left out.
    pub fn left_out(&self) -> &[u64];
}

#[derive(Debug)]
pub struct RestoredItem<'r> {
    pub id: u64,
    pub record: &'r [u8],
    pub fds: Vec<OwnedFd>,
}

impl RestoredItem<'_> {
    /// Restore `T` from the record and the FDs, as `T::restore` does.
    pub fn restore<T: Restore>(self) -> Result<T, T::Error>;
}
```

### `raise_fd_limit`

```rust
/// Raise the soft limit of open FDs of the process to the hard one, and give the new soft limit,
/// `None` for none.
pub fn raise_fd_limit() -> io::Result<Option<u64>>;
```

The next instance takes the FDs of a hand-over out of the queue sockets under its soft limit, and
the kernel closes those that don't fit, so it raises the limit first; `hand_over` fits the items
under the limit that this instance had, which it takes the next one to have too.

## What the store holds

The FDs of a hand-over are stored under names of the key: `<key>/state` for the memfd, and
`<key>/fds` for each queue socket. The key is 1 to 200 printable ASCII characters other than `:`
(as for any name of the store) and `/`. A name that starts with `<key>/` is the crate's, the
names of another version of the crate included: `take_over` removes all of them from the store,
and closes those that it doesn't know. The sockets of socket activation can't be mistaken for
them: their names are those of their units by default, and a unit name has no `/`.

Two keys keep two hand-overs apart, as of a service that hands over its D-Bus connection through
zbus and its Varlink connections through zlink: each takes two or more FDs of the store.

The memfd holds, little-endian, with offsets from its start:

```text
header   magic `zoter\0\0\0`, major version, length of the header, features that a reader must
         know, total length, number of items, number of FDs, where the record and the FDs of
         the state are
table    for each item: its id, where its record is (offset, length), and where its FDs are
         (index of the first, how many)
records  that of the state, then those of the items, each at an offset that is a multiple of 8
```

As for zbus's records: a reader refuses a later major version, skips the fields that a later minor
version adds at the end of the header, and refuses a record that needs a feature it doesn't know.
The memfd is written in place, through a shared mapping whose memory is allocated first
(`MADV_POPULATE_WRITE`), so that a lack of memory fails the hand-over rather than kill the
process, and sealed against writes and resizing. The next instance maps it private and
read-only, and gives the records out of the mapping.

The FDs of the state and of the items follow each other in one list, in order, each with its index
in it. The queue sockets hold them in datagrams of up to 253 (`SCM_MAX_FD`), each with the index of
its first FD, so the order of the queue sockets doesn't matter, and each takes some 70000 FDs with
the default size of its send buffer. The store keeps one FD of each open file description, while a
queue keeps each FD as it is, so two FDs of one file, such as one that a client sent twice, go to
the next instance as two. The memfd is stored first, so that a store too small for all of it keeps
the part that matters most.

## When things fail

Taking over:

- No `<key>/state`: nothing was handed over (`Ok(None)`), and queue sockets without it are closed.
- A memfd that isn't a hand-over, or of a later major version: an error, and every FD of the
  hand-over is closed, which disconnects the peers. They are out of the store already.
- An item whose FDs aren't all there, as when the kernel closed those over the limit of open FDs
  of the process: left out, in `Restored::left_out`, with a warning that names `LimitNOFILE=`.
- FDs that no item has: closed.

Handing over:

- `check_hand_over` fails (no `$FDSTORE`, or fewer than 2): so does `hand_over`, with
  `ErrorKind::Unsupported`, and the caller should have shut down rather than paused.
- Items that can't be saved, items over the limit of open FDs, items whose `save` fails, and items
  whose FDs don't fit in the store (`FileDescriptorStoreMax=` too low) or fail to be queued: left
  out, in `Stored`, with a warning that names the setting of the unit to raise, if any.
- Creating, filling or storing the memfd fails: an error, and all of the hand-over is dropped.
- systemd takes longer than 5 s to deal with the messages: `ErrorKind::TimedOut`.

The warnings go through `tracing`, behind a default feature of the same name, as zbus and zruntime
have it; the counts and ids are in `Stored` and `Restored` for the caller to log its own way.

## Testing

Behind a `testing` feature, `zoter::testing::Manager` is busd's fake service manager, made
generic. It binds a notification socket and serves it on a thread of its own, keeping the store as
systemd does (one FD of each open file description, `FDSTOREREMOVE`, closing the pipe of a
`BARRIER`), and records the other states. It holds the sockets of socket activation that the test
gives it, under their names, and starts a service as systemd does: `Manager::command(program)`
gives a `std::process::Command` with `$NOTIFY_SOCKET`, `$FDSTORE`, `$LISTEN_FDS` and
`$LISTEN_FDNAMES` set, the passed FDs from 3 on, and `$LISTEN_PID` set to the PID of the service.
For a service that runs in the test process, `Manager::notifier` and `Manager::passed` give what
`Store::from_env` and `Passed::from_env` would. Tests wait for a state (`until("READY=1")`), look
at the store, and empty it, as a stop does. busd, zbus and zlink test their restarts with it.

It needs `libc`, which rustix doesn't replace there: `dup2` onto FDs that the child doesn't own yet,
and `kcmp(2)`, to tell FDs of one open file description apart as systemd does. Both are optional
dependencies of the feature.

## Platform, MSRV, dependencies

- Linux only, where systemd is: elsewhere the crate is empty, and its users depend on it under
  `[target.'cfg(target_os = "linux")'.dependencies]`. zlink-core, which builds for `no_std`,
  depends on it in its `std` layer only.
- Edition 2024, MSRV 1.88, which zlink has (busd and zbus have 1.89).
- `rustix` (`event`, `fs`, `io`, `mm`, `net`, `pipe`, `process`, `std`), as busd has it, on Linux;
  `tracing` (optional, default); `libc` (optional, `testing`).
- License: MIT, as the other z-galaxy crates.

## The other crates

Until their next releases, zbus, zlink and busd depend on the crate through git, all with the same
reference (`branch = "main"`, as busd depends on zbus already), so that Cargo resolves one copy of
it, and the impls of zbus and zlink are of the traits that busd and the services use. The crate
lands first; zbus and busd then follow, in that order, and zlink alongside them.

### zbus

- What the `handover` feature adds goes from every unix target (z-galaxy/zbus@f738dfc) to Linux
  only. A hand-over has one use, a restart through the store, and its one user, busd, hands over
  on Linux only, the hand-over within one process (`Bus::hand_over`, then `Bus::take_over`)
  included, and so do its tests. The feature still builds everywhere, and adds nothing outside
  Linux, as it adds nothing outside unix today. Restarts on other platforms, should they come,
  would widen it again, with the crate.
- `connection::Handover` implements `zoter::Save` and `zoter::Restore`, which take the place
  of its own `saved_len`, `save`, `fds` and `restore`, so that it has one API for its record. The
  feature depends on the crate. `MessageStream::hand_over`, `Builder::take_over` and
  `Connection::prepare_hand_over` stay as they are, but for the platforms: they are about D-Bus.
- D-Bus services, next: today only a connection whose messages all go through the stream of
  `Builder::build_message_stream` can be handed over, which suits a bus, but not a service with
  an object server and proxies. Such a service needs, with the `service` feature,
  `Connection::hand_over(&self) -> Result<Handover>`: the object server stops dispatching method
  calls, those under way are answered, and the connection is taken apart, with the calls not yet
  dispatched among its messages. The record also has to carry what zbus knows of the connection
  on the bus side: the well-known names that it owns or queues for, and its match rules with
  their counts, so that the connection that takes over (`Builder::take_over`, then `name` and
  `serve_at` as usual) neither requests those names nor adds those rules again. How the streams
  of proxies take part, whose messages are signals, is for zbus's own design of it. A service
  then hands its connection over as an item of `zoter::Handover`, with its own state, which it
  saves its own way.

### zlink

With a feature on Linux, with `std`, as the hand-over of zbus is:

- `zlink_core::connection::Handover`, implementing `zoter::Save` and `zoter::Restore`: the
  socket, what was read and not received yet, with its FDs (`ReadConnection::into_read_half`
  takes that out already), and what was enqueued and not written yet, with its FDs, in a record
  of zlink's own.
- `Connection::hand_over(self) -> Result<Handover>`, for a connection between calls, and
  `Connection::take_over(handover)`, which needs the socket made from an FD, which the runtime
  crates' sockets do (`Stream: TryFrom<std::os::unix::net::UnixStream>`). A peer that doesn't read
  holds a flush up, so `WriteConnection` keeps what a write cut off in the middle left, for the
  hand-over to take, as zbus does.
- `Server::run_until(self, stop) -> Result<Stopped<..>>`: once `stop` resolves, the server takes
  no more connections or calls, answers the calls under way, and gives back its listener, its
  service and its connections, which are between calls. That is also the graceful shutdown of
  z-galaxy/zlink#83: drop what it gives back. A connection whose replies still stream (`more`),
  or that was upgraded, is closed, as nothing generic hands those over; a later version can let a
  service hand them over. `Server` gets a way to serve connections that were taken over along
  with those it accepts.
- A service hands its connections over as items of `zoter::Handover`, under their ids, and its
  own state its own way: `Stopped` gives the service back for that.

### busd

- `src/systemd.rs` goes: busd uses `zoter::{Passed, Notifier, Store}`.
- `src/bus/systemd.rs` keeps the bus's side: `Bus::for_systemd` takes over through
  `Store::take_over`, decodes its state, restores each client's connection with
  `RestoredItem::restore::<connection::Handover>`, by id, and listens on the one socket of socket
  activation left in `Passed`. `Bus::hand_over_to_systemd` keeps busd's own rule (no hand-over
  without socket activation) and timeout, pushes each client's `connection::Handover` under its
  id, with one extra FD per client (its pidfd), and calls `Store::hand_over`. `queue`,
  `take_queued`, `take_fds`, `bytes_at`, `fit`, `Reserve`, `StoredFds` and their constants go to
  the crate. Its module docs keep what is the bus's, and point to the crate's for the rest.
- The state loses `Stored` (where each record and its FDs are), which the crate's table says now,
  and its version goes to 2. The stored names become `busd/state` and `busd/fds`.
- The in-process hand-over (`Bus::hand_over`, then `Bus::take_over`), which the tests and the
  benchmark use without systemd, stays as it is: it hands Rust values over, with no memfd.
- `tests/fdstore.rs` and the benchmark use `zoter::testing::Manager` rather than their own.
- `raise_fd_limit` stays in busd, which runs on macOS too and caps the limit there.

## Name

`zoter`, never capitalized, as zbus isn't: after Soter, the Greek daimon of safety, preservation,
and deliverance from harm, which is what it gives the clients of a service as the service
restarts. It is free on crates.io, and its repository is `z-galaxy/zoter`.

## Decisions

1. `Passed::from_env` is `unsafe`, as it removes the variables, rather than leave them and stop a
   second take with a static flag.
2. zbus's hand-over becomes Linux-only, as zlink's is, and the traits `Save` and `Restore` replace
   zbus's own methods of `connection::Handover`.
3. The fake service manager is in the crate, behind `testing`, rather than in a crate of its own.
4. The memfd and queue-socket helpers are private: the hand-over is what services need.
5. Stored names: `<key>/state` and `<key>/fds`.
6. Warnings go through `tracing`, a default feature.
7. zlink closes the connections whose replies stream, and the upgraded ones, as it hands over,
   rather than wait for them: a later version can let a service hand them over.

## The plan

1. This repository: the scaffolding, from zruntime (CI, release-plz, commitlint, git hooks,
   rustfmt, editorconfig, codespell, renovate, AGENTS.md and CLAUDE.md, CONTRIBUTING.md,
   SECURITY.md, the license, the README); then `Save` and `Restore`; then `Passed` and `Notifier`,
   moved from busd with their tests; then the memfd, the queue sockets, `Store`, `Handover` and
   `Restored`; then `testing`, and an example service that restarts with its clients, which the
   tests run under the fake manager.
2. zbus: the hand-over becomes Linux-only, and `connection::Handover` implements the traits, in
   place of its own methods.
3. busd: on the crate and on zbus's impls.
4. zlink: the hand-over of a connection, then `Server::run_until` and serving the connections that
   were taken over.
5. zbus: the hand-over of a D-Bus service's connection, with its object server, names and match
   rules, after a design of its own.
