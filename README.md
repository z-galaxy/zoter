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
</p>

<h1 align="center">zoter</h1>

Restart systemd services without dropping their clients.

A service that stops hands its state and the connections of its clients over to its next
instance, through the file descriptor store that systemd keeps for the service across its
restarts. The clients notice nothing but a pause: the next instance serves them on the same
connections, from where the previous one stopped.

zoter does what a service needs for that, except what depends on the service's protocol: the file
descriptors that systemd passes a service, its notifications, and the layout of a hand-over in the
store, within the limits of systemd and of the next instance. A protocol hands its connections over
through two traits: [zbus] implements them for D-Bus, and [zlink] for Varlink.

zoter is named after Soter, the Greek daimon of safety, preservation, and deliverance from harm.
It builds nothing on platforms other than Linux, where systemd is.

## Status

In development: nothing is released yet, and the API can change at any time.

## License

[MIT](https://github.com/z-galaxy/zoter/blob/main/LICENSE)

[zbus]: https://github.com/z-galaxy/zbus
[zlink]: https://github.com/z-galaxy/zlink
