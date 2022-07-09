# Localtunnel

Localtunnel exposes your localhost endpoint to the world, user cases are:
- API testing
- multiple devices access to single data store
- peer to peer connection, workaround for NAT hole punching.

## Usage

CLI:

```shell
cargo run
```

As a Rust library:

```Rust
let result = open_tunnel(
    Some("http://proxy.ad4m.dev"),
    Some("kaichao"), 
    None, 
    12000,
).await;
```

## Resources

- localtunnel: https://github.com/localtunnel/localtunnel
- bore: https://github.com/ekzhang/bore/
- tunnelto: https://github.com/agrinman/tunnelto