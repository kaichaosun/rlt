# Localtunnel

Localtunnel exposes your localhost endpoint to the world, user cases are:
- local API testing
- multiple devices access to single data store
- peer to peer connection, workaround for NAT hole punching.

## Usage

Use as a Rust library:

```Rust
let result = open_tunnel(
    Some("http://proxy.ad4m.dev"),
    Some("kaichao"), 
    None, 
    12000,
).await;
```
