//! What zoter costs a service to hand over to its next instance: storing a hand-over, as the
//! instance that stops does, and taking it over, as the next one does, with an item for each client
//! of the service, a unix socket and a record, as the hand-over of a connection is.
//!
//! The store is that of zoter's fake service manager, `testing::Manager`, which serves its
//! notification socket on a thread of its own, and keeps what is stored, as systemd does. Both
//! instances are this process, one after the other, and the clients keep their ends of the sockets
//! open throughout.
//!
//! - `store/100-clients`, `store/1000-clients`: `Handover::store` of the state of the service, 64
//!   bytes for each client, and of an item for each, with a record of 256 bytes, from the start of
//!   storing until the manager dealt with what was stored, which `store` waits for.
//! - `take-over/100-clients`, `take-over/1000-clients`: `Store::take_over` of such a hand-over, out
//!   of what the manager passes the next instance, and the restoring of each item, its record and
//!   its socket.
//!
//! What a round needs besides, which the service doesn't do as it hands over or takes over, is done
//! between the rounds, untimed: creating the store of an instance, starting a hand-over, which only
//! checks the store, the manager passing the next instance what is stored, and closing what was
//! taken over. The rounds check, untimed, that no item was left out, as a hand-over that leaves
//! items out is faster.

#[cfg(target_os = "linux")]
criterion::criterion_main!(linux::benches);

/// zoter builds nothing on other platforms, where systemd isn't, and this measures nothing there.
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        io,
        os::{
            fd::{AsFd, BorrowedFd, OwnedFd},
            unix::net::UnixStream,
        },
        time::{Duration, Instant},
    };

    use criterion::{Criterion, criterion_group};
    use zoter::{Restore, Save, Store, Stored, testing::Manager};

    criterion_group!(benches, stores, take_overs);

    /// The ids of `store/`: storing hand-overs.
    fn stores(c: &mut Criterion) {
        let mut group = c.benchmark_group("store");
        for clients in CLIENTS {
            let service = Service::new(clients);
            group.bench_function(format!("{clients}-clients"), |b| {
                b.iter_custom(|rounds| (0..rounds).map(|_| service.time_store()).sum())
            });
        }
        group.finish();
    }

    /// The ids of `take-over/`: taking hand-overs over.
    fn take_overs(c: &mut Criterion) {
        let mut group = c.benchmark_group("take-over");
        for clients in CLIENTS {
            let service = Service::new(clients);
            group.bench_function(format!("{clients}-clients"), |b| {
                b.iter_custom(|rounds| (0..rounds).map(|_| service.time_take_over()).sum())
            });
        }
        group.finish();
    }

    /// A service, with its clients, under a fake service manager.
    struct Service {
        manager: Manager,
        /// What the service hands over as its state.
        state: Vec<u8>,
        /// What the service hands over of each client.
        connections: Vec<Connection>,
        /// The clients' ends of the sockets of the connections.
        _clients: Vec<UnixStream>,
    }

    impl Service {
        /// A service of `clients` clients.
        fn new(clients: usize) -> Self {
            // The process has both ends of the sockets of the clients open, and those that it
            // takes over.
            zoter::raise_fd_limit().expect("the limit of open file descriptors is raised");
            let (connections, ends) = (0..clients)
                .map(|_| {
                    let (socket, end) = UnixStream::pair().expect("a pair of sockets");
                    let record = vec![RECORD_BYTE; RECORD_LEN];

                    (Connection { record, socket }, end)
                })
                .unzip();

            Self {
                manager: Manager::new().expect("a fake service manager"),
                state: vec![STATE_BYTE; STATE_PER_CLIENT * clients],
                connections,
                _clients: ends,
            }
        }

        /// How long storing a hand-over of the service takes, from the start of storing until the
        /// manager dealt with what was stored.
        fn time_store(&self) -> Duration {
            let handover = self
                .store()
                .handover()
                .expect("the store takes a hand-over");
            let start = Instant::now();
            let stored = handover
                .store(&self.state[..], self.items())
                .expect("the hand-over is stored");
            let time = start.elapsed();
            self.check(&stored);
            // The next round stores into an empty store, as an instance does that took over.
            self.manager.clear_store();

            time
        }

        /// How long taking a hand-over of the service over takes, and restoring its items.
        fn time_take_over(&self) -> Duration {
            let handover = self
                .store()
                .handover()
                .expect("the store takes a hand-over");
            let stored = handover
                .store(&self.state[..], self.items())
                .expect("the hand-over is stored");
            self.check(&stored);
            let mut passed = self.manager.passed().expect("the manager passes the store");
            let mut store = self.store();

            let start = Instant::now();
            let mut restored = store
                .take_over(&mut passed)
                .expect("the hand-over is taken over")
                .expect("a hand-over was stored");
            let connections: Vec<(u64, Connection)> = restored
                .take_items()
                .map(|item| (item.id, item.restore().expect("the connection is restored")))
                .collect();
            let time = start.elapsed();
            assert!(restored.left_out().is_empty(), "{:?}", restored.left_out());
            assert_eq!(connections.len(), self.connections.len());
            // The manager removes what was taken over from its store before the next round stores
            // into it, as it keeps no more than its store takes.
            store
                .notifier()
                .barrier(WAIT)
                .expect("the manager removes what was taken over");

            time
        }

        /// The store of an instance of the service, as the instance creates it as it starts.
        fn store(&self) -> Store {
            let notifier = self.manager.notifier().expect("a notifier of the manager");

            Store::new(notifier, KEY, Some(self.manager.store_max())).expect("a store")
        }

        /// The items of a hand-over of the service: its connections, under the numbers of their
        /// clients.
        fn items(&self) -> impl Iterator<Item = (u64, &Connection)> {
            (0..).zip(&self.connections)
        }

        /// Check that no item of the service was left out of `stored`.
        fn check(&self, stored: &Stored) {
            assert!(stored.left_out().is_empty(), "{stored:?}");
            assert_eq!(stored.items(), self.connections.len());
        }
    }

    /// What a service hands over of a client, as the hand-over of a connection is: the socket of
    /// the connection, and a record of what goes with it, which the next instance restores the
    /// connection from.
    struct Connection {
        record: Vec<u8>,
        socket: UnixStream,
    }

    impl Save for Connection {
        fn saved_len(&self) -> Option<usize> {
            Some(self.record.len())
        }

        fn save(&self, dest: &mut [u8]) -> io::Result<()> {
            dest.copy_from_slice(&self.record);

            Ok(())
        }

        fn fds(&self) -> Vec<BorrowedFd<'_>> {
            vec![self.socket.as_fd()]
        }
    }

    impl Restore for Connection {
        type Error = io::Error;

        fn restore(record: &[u8], fds: Vec<OwnedFd>) -> io::Result<Self> {
            let [socket] = <[OwnedFd; 1]>::try_from(fds).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "a connection has one socket")
            })?;

            Ok(Self {
                record: record.to_vec(),
                socket: UnixStream::from(socket),
            })
        }
    }

    /// How many clients the services of the ids have: as many as those of busd's benchmarks of its
    /// restarts, and ten times as many, which take a few datagrams of file descriptors to store.
    const CLIENTS: [usize; 2] = [100, 1000];
    /// The key that the services hand over under.
    const KEY: &str = "bench";
    /// How long the record of each connection is.
    const RECORD_LEN: usize = 256;
    /// What the records of the connections are made of.
    const RECORD_BYTE: u8 = 0x5a;
    /// How many bytes of the state of the service are of each client.
    const STATE_PER_CLIENT: usize = 64;
    /// What the state of the service is made of.
    const STATE_BYTE: u8 = 0xa5;
    /// How long a round waits for the manager to deal with what it was sent, at most.
    const WAIT: Duration = Duration::from_secs(10);
}
