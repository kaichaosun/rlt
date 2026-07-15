# Localtunnel

[![localtunnel](https://img.shields.io/crates/v/localtunnel.svg)](https://crates.io/crates/localtunnel)
[![localtunnel-client](https://img.shields.io/crates/v/localtunnel-client.svg)](https://crates.io/crates/localtunnel-client)
[![localtunnel-server](https://img.shields.io/crates/v/localtunnel-server.svg)](https://crates.io/crates/localtunnel-server)

Localtunnel exposes your localhost endpoint to the world, user cases are:
- API testing
- multiple devices access to single data store
- peer to peer connection, workaround for NAT hole punching.

## Encrypted tunnel

Since v0.2.0 all tunnel connections between client and server are encrypted
with the [Noise protocol](https://noiseprotocol.org/) (`NK` pattern,
X25519 + ChaCha20-Poly1305), with fresh session keys per connection. The
server hands its public key and a per-tunnel session token to the client in
the registration response, and only connections that complete the handshake
and present the token can join the tunnel pool.

This is a breaking protocol change: v0.2.0 clients and servers do not
interoperate with older releases — upgrade both sides.

## Client Usage

Known issue: *the public proxy server is down, please setup your own server.*

Use in CLI:

```shell
cargo install localtunnel

localtunnel client --host https://your-domain.com --subdomain kaichao --port 3000
```

Use as a Rust library:

```shell
cargo add localtunnel-client
```

```Rust
use localtunnel_client::{open_tunnel, broadcast, ClientConfig};

let (notify_shutdown, _) = broadcast::channel(1);

let config = ClientConfig {
    server: Some("https://your-domain.com".to_string()),
    subdomain: Some("demo".to_string()),
    local_host: Some("localhost".to_string()),
    local_port: 3000,
    shutdown_signal: notify_shutdown.clone(),
    max_conn: 10,
    credential: None,
    reregister_after: None,
};
let result = open_tunnel(config).await?;

// Shutdown the background tasks by sending a signal.
let _ = notify_shutdown.send(());
```

## Server Usage

Use in CLI:

```shell
localtunnel server --domain your-domain.com --port 3000 --proxy-port 3001 --secure
```

Use as a Rust library,

```shell
cargo install localtunnel-server
```

```Rust
use localtunnel_server::{start, ServerConfig};

let config = ServerConfig {
    domain: "your-domain.com".to_string(),
    api_port: 3000,
    secure: true,
    max_sockets: 10,
    proxy_port: 3001,
    require_auth: false,
};

start(config).await?
```

## Sponsor

__Please help me build OSS__ 👉 [GitHub Sponsors](https://github.com/sponsors/kaichaosun)

## Resources

- [localtunnel](https://github.com/localtunnel/localtunnel)