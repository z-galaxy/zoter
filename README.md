<p align="center">
  <a href="https://github.com/z-galaxy/zoter/actions/workflows/rust.yml">
    <img alt="CI Pipeline Status" src="https://github.com/z-galaxy/zoter/actions/workflows/rust.yml/badge.svg">
  </a>
  <a href="https://docs.rs/zoter/">
    <img alt="API Documentation" src="https://docs.rs/zoter/badge.svg">
  </a>
  <a href="https://crates.io/crates/zoter">
    <img alt="crates.io" src="https://img.shields.io/crates/v/zoter">
  </a>
  <a href="https://app.codspeed.io/z-galaxy/zoter?utm_source=badge">
    <img alt="CodSpeed" src="https://img.shields.io/endpoint?url=https://codspeed.io/badge.json">
  </a>
</p>

<p align="center">
  <img alt="zoter logo"
    src="https://raw.githubusercontent.com/z-galaxy/zoter/ef25fb8c4c17c9d33ada3e9b3edea7830f898774/data/logo.svg">
</p>

<h1 align="center">zoter</h1>

Restart systemd services without dropping their clients.

A service that stops hands its state and the connections of its clients over to its next
instance, through the file descriptor store that systemd keeps for the service across its
restarts. The clients notice nothing but a pause: the next instance serves them on the same
connections, from where the previous one stopped, while those that connect meanwhile wait in the
socket of socket activation, which systemd keeps open.

zoter does what a service needs for that, except what depends on the service's protocol, such as
D-Bus or Varlink:

* [`Passed`] takes the file descriptors that systemd passes a service: the sockets of socket
  activation, and what the previous instance stored.
* [`Notifier`] tells systemd that the service is ready, among other things, and stores file
  descriptors in its store.
* [`Store`] hands the service over to its next instance. In the instance that stops,
  [`Store::handover`] starts a [`Handover`], and [`Handover::store`] stores the state of the service
  and items, such as the connections of its clients, each a record of bytes and file descriptors,
  within the limits of systemd and of the next instance, however many there are. In the next
  instance, [`Store::take_over`] takes them over, as a [`Restored`].

A protocol takes part through two traits, [`Save`] and [`Restore`], which the hand-overs of its
connections implement, so that a service hands those over as they are. The `testing` feature adds
[`testing::Manager`], a fake service manager, for the tests of the restarts of a service.

zoter is named after Soter, the Greek daimon of safety, preservation, and deliverance from harm.
It builds nothing on platforms other than Linux, where systemd is. Its API is synchronous, and
depends on no async runtime.

## The life of a service

```rust
use std::{io, os::fd::OwnedFd};

use zoter::{Passed, Store};

fn main() -> io::Result<()> {
    // The next instance takes the file descriptors of a hand-over out of the store under this
    // limit.
    zoter::raise_fd_limit()?;
    // SAFETY: No other thread runs yet, so none reads or writes the environment meanwhile, and
    // nothing took the file descriptors that systemd passed.
    let mut passed = unsafe { Passed::from_env() }?;

    // `None` without `$NOTIFY_SOCKET`, as when systemd didn't start the service.
    let mut store = Store::from_env("example")?;
    // The file descriptors stored under `example/` are taken out of `passed`, and removed from the
    // store, and the hand-over that they hold is read, if there is one.
    let restored = match &mut store {
        Some(store) => store.take_over(&mut passed)?,
        None => None,
    };
    // What is left in `passed` is the sockets of socket activation, by the names of their units.
    let _listener = passed.take("example.socket");
    // The clients that the previous instance handed over, each a socket here.
    let mut clients: Vec<(u64, OwnedFd)> = Vec::new();
    if let Some(mut restored) = restored {
        let _state = restored.state();
        for item in restored.take_items() {
            let id = item.id;
            clients.push((id, item.restore()?));
        }
    }
    if let Some(store) = &store {
        store.notifier().notify("READY=1")?;
    }

    // Serve the clients, and the ones that connect, until systemd sends `SIGTERM`.

    // Without a store to hand over through, the service shuts down as it does without systemd, as
    // it does with one that can't take a hand-over.
    let Some(store) = store else {
        return Ok(());
    };
    let handover = match store.handover() {
        Ok(handover) => handover,
        Err(e) => {
            eprintln!("Not handing over to the next instance: {e}");

            return Ok(());
        }
    };
    // Stop taking requests, finish those taken, and hand the state and the clients over.
    let clients = clients.iter().map(|(id, client)| (*id, client));
    let stored = handover.store(b"the state".to_vec(), clients)?;
    for (id, why) in stored.left_out() {
        eprintln!("Client {id} was left out: {why}");
    }

    Ok(())
}
```

The units of a service that restarts so, `example`, are:

```ini
# example.socket
[Socket]
ListenStream=/run/example

[Install]
WantedBy=sockets.target
```

```ini
# example.service
[Service]
Type=notify
ExecStart=/usr/bin/example
# The memfd of the hand-over, and the sockets that hold its file descriptors, some 70000 in each.
FileDescriptorStoreMax=16
```

`systemctl restart example` then restarts the service with its clients, which needs systemd 254 or
later. A stop, or a start after a stop, doesn't keep them: systemd closes the store as the service
stops for good.

## Status

In development: nothing is released yet, and the API can change at any time.

## License

[MIT](https://github.com/z-galaxy/zoter/blob/main/LICENSE)

## Sponsors

<a href="https://codspeed.io/?utm_source=oss-sponsorship&utm_medium=z-galaxy">
  <picture>
    <source media="(prefers-color-scheme: dark)"
      srcset="https://codspeed.io/codspeed-logo-dark.svg">
    <source media="(prefers-color-scheme: light)"
      srcset="https://codspeed.io/codspeed-logo-light.svg">
    <img alt="CodSpeed logo" src="https://codspeed.io/codspeed-logo-light.svg" width="400">
  </picture>
</a>

[`Passed`]: https://docs.rs/zoter/latest/zoter/struct.Passed.html
[`Notifier`]: https://docs.rs/zoter/latest/zoter/struct.Notifier.html
[`Store`]: https://docs.rs/zoter/latest/zoter/struct.Store.html
[`Store::handover`]: https://docs.rs/zoter/latest/zoter/struct.Store.html#method.handover
[`Store::take_over`]: https://docs.rs/zoter/latest/zoter/struct.Store.html#method.take_over
[`Handover`]: https://docs.rs/zoter/latest/zoter/struct.Handover.html
[`Handover::store`]: https://docs.rs/zoter/latest/zoter/struct.Handover.html#method.store
[`Restored`]: https://docs.rs/zoter/latest/zoter/struct.Restored.html
[`Save`]: https://docs.rs/zoter/latest/zoter/trait.Save.html
[`Restore`]: https://docs.rs/zoter/latest/zoter/trait.Restore.html
[`testing::Manager`]: https://docs.rs/zoter/latest/zoter/testing/struct.Manager.html
