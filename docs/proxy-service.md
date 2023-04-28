# Setup Proxy Service

## Configure Domain

Add A record `*.proxy.init.so`, the value is server IP.
Add A record `proxy.init.so`, the value is server IP.

You can now login the server with SSH,

```
ssh ubuntu@proxy.init.so
```

## Install Rust and localtunnel

```shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

```shell
cargo install localtunnel

# Set credentials if needed

export CLOUDFLARE_ACCOUNT=xxx
export CLOUDFLARE_NAMESPACE=xxx
export CLOUDFLARE_AUTH_EMAIL=xxx
export CLOUDFLARE_AUTH_KEY=xxx

localtunnel server --domain proxy.init.so --port 3000 --proxy-port 3001 --secure --require-auth
```

*Known issues:*

> failed to run custom build command for `openssl-sys v0.9.77`

Install libssl,
```shell
sudo apt install libssl-dev
```

## Run localtunnel with systemd

Add a systemd service `/etc/systemd/system/localtunnel.service` with following content,

```
[Unit]
Description=localtunnel

[Service]
Environment="CLOUDFLARE_ACCOUNT=xxx"
Environment="CLOUDFLARE_NAMESPACE=xxx"
Environment="CLOUDFLARE_AUTH_EMAIL=xxx"
Environment="CLOUDFLARE_AUTH_KEY=xxx"
Environment="RUST_LOG=debug"
Environment="RUST_BACKTRACE=1"
ExecStart=/root/.cargo/bin/localtunnel server --domain proxy.init.so --port 3000 --proxy-port 3001 --secure --require-auth
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

Start the service,

```shell
systemctl enable localtunnel.service
systemctl start localtunnel.service
systemctl status localtunnel.service

# view logs
journalctl -f -u localtunnel
```

## Setup Caddy proxy

Install Caddy,

```shell
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update
sudo apt install caddy
```

Create and modify Caddyfile,

```shell
touch Caddyfile
```

```
proxy.init.so {
  reverse_proxy http://127.0.0.1:3000
  tls {
    on_demand
  }
}

*.proxy.init.so {
  reverse_proxy http://127.0.0.1:3001
  tls {
    on_demand
  }
}
```

```shell
# use systemd instead
caddy stop
caddy start
```

Make new caddyfile the default config of caddy.service,

```shell
systemctl status caddy.service
systemctl stop caddy.service

cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile-origin
cp ~/caddy/Caddyfile /etc/caddy/Caddyfile

systemctl start caddy.service
```

## Configure Firewall

```shell
ufw allow https
ufw allow http
ufw allow ssh
ufw allow 1000:65535/tcp

ufw enable

ufw status
```

Note: *You may also need to open the ports with Firewall settings from cloud provider.*
