# Localtunnel

[![localtunnel](https://img.shields.io/crates/v/localtunnel.svg)](https://crates.io/crates/localtunnel)
[![localtunnel-cli](https://img.shields.io/crates/v/localtunnel-cli.svg)](https://crates.io/crates/localtunnel-cli)

Localtunnel exposes your localhost endpoint to the world, user cases are:
- API testing
- multiple devices access to single data store
- peer to peer connection, workaround for NAT hole punching.

## Client Usage

Use in CLI:

```shell
cargo install localtunnel

localtunnel client --host https://localtunnel.me --subdomain kaichao --port 3000
```

Use as a Rust library:

```shell
cargo add localtunnel-client
```

```Rust
use localtunnel_client::{open_tunnel, broadcast};

let (notify_shutdown, _) = broadcast::channel(1);
let result = open_tunnel(
    Some("https://localtunnel.me"),
    Some("kaichao"),
    Some("locallhost"),
    3000,
    notify_shutdown.clone(),
    10,
    None,
).await;

// Shutdown the background tasks by sending a signal.
let _ = notify_shutdown.send(());
```

## Server Usage

Use in CLI:

```shell
localtunnel server --domain localtunnel.me --port 3000 --proxy-port 3001 --secure
```

Use as a Rust library,

```shell
cargo install localtunnel-server
```

```Rust
use localtunnel_server::start;

start("localtunnel.me", 3000, true, 10, 3001, false).await?
```

## Resources

- [localtunnel](https://github.com/localtunnel/localtunnel)