# Production deployment

This guide covers building or downloading `sixm-server`, configuring
`server.cfg`, managing server identity keys and certificate fingerprints,
running as a service on Linux and Windows, putting TLS in front of the HTTP
listener, and configuring firewalls for UDP and TCP.

For a full list of server convars and console commands, see
[server.md](../server.md).

## 1. Binary and directory layout

### Release archives

Prebuilt archives for Linux (`x86_64-unknown-linux-gnu`) and Windows
(`x86_64-pc-windows-msvc`) are attached to each release on the
[Releases page](https://github.com/SixM-labs/SixM/releases). Each archive
contains:

```text
sixm-vX.Y.Z-<target>/
├── sixm-server(.exe)
├── sixm-client(.exe)
├── LICENSE
├── README.md
└── server-data/
    ├── server.cfg
    └── resources/
```

### Building from source

Building requires a Rust toolchain (edition 2024, minimum version `1.88`) and a
C compiler for the bundled Lua 5.4 library (`gcc` or `clang` on Linux; Visual
Studio Build Tools / MSVC on Windows):

```sh
cargo build --release --locked -p sixm-server
```

This produces `target/release/sixm-server` (or `target\release\sixm-server.exe`
on Windows).

### Server working directory

`sixm-server` resolves `server.cfg` and its default data paths relative to the
current working directory. A clean production layout looks like this:

```text
/srv/sixm/
├── server.cfg
├── resources/                  # sixm_resourcesPath (default: resources)
│   └── ...
└── cache/
    ├── server-identity.bin     # sixm_identityFile (default: cache/server-identity.bin)
    └── kvp/                    # sixm_kvpPath (default: cache/kvp)
        └── <resource>.kvp
```

Keep the `sixm-server` binary outside the writable data directory (for example
`/usr/local/bin/sixm-server` on Linux) and set the service's working directory
to `/srv/sixm`.

If you start `sixm-server` with no `+command` arguments, it automatically runs
`exec server.cfg` from the working directory. If you pass any `+command` on the
command line, `server.cfg` is **not** run unless you include `+exec server.cfg`.

## 2. Production `server.cfg`

Start from a minimal, explicit `server.cfg`:

```text
# Network address (UDP for QUIC game traffic, TCP for built-in HTTP)
endpoint_add_udp 0.0.0.0:30120

# Server identity and capacity
set sv_hostname "My Production Server"
set sv_maxclients 64
set sixm_tickRate 20

# Persistent paths (relative to the working directory)
set sixm_resourcesPath resources
set sixm_identityFile cache/server-identity.bin
set sixm_kvpPath cache/kvp

# HTTP listener (set to false if you do not need /info.json, /players.json,
# /dynamic.json, or SetHttpHandler routes)
set sv_httpEnabled true
set sv_exposePlayerIdentifiers false

# Entity scoping and client entity limits
set sixm_cullRadius 424
set sv_entityLockdown relaxed
set sixm_maxClientEntities 64

# Permissions (never leave wildcard grants without verifying licenses)
# add_principal identifier.license:<64 hex chars> group.admin
add_ace group.admin command allow

# Resources
ensure my_gamemode
```

Production recommendations:

- **Keep `sv_exposePlayerIdentifiers false`** (the default) unless an internal
  tool specifically needs `/players.json` to include `license:` and `ip:`
  identifiers.
- **Set `sv_entityLockdown`** to `relaxed` (blocks client-created peds) or
  `strict` (blocks all client-created vehicles, peds, and objects) if your
  server scripts spawn everything. You can override this per routing bucket with
  `SetRoutingBucketEntityLockdownMode`.
- **Keep `sixm_identityFile` non-empty.** Setting `sixm_identityFile` to an
  empty string creates an ephemeral certificate on every start, which breaks
  fingerprint pinning.

## 3. Identity key and certificate fingerprint pinning

Every connection to `sixm-server` uses QUIC with TLS 1.3. On the first start,
the server generates a self-signed certificate and PKCS#8 private key and saves
them to `sixm_identityFile` (`cache/server-identity.bin` by default):

- **File permissions:** On Unix, the file is created with mode `0600`. On
  Windows, its ACL is replaced so only the file owner has access (with
  inheritance disabled).
- **Back up and protect `server-identity.bin`:** Anyone who obtains this file
  can impersonate your server even to clients that pin the fingerprint. Back it
  up securely when migrating hosts, and never commit it to version control.
- **Fingerprint output:** At startup, the server logs:

  ```text
  Listening on 0.0.0.0:30120
  Server fingerprint: <64 lowercase hex characters>
  ```

- **Pinning on clients:** Without `--fingerprint`, `sixm-client` encrypts the
  connection but accepts any server certificate (vulnerable to man-in-the-middle
  attacks). Publish your server's 64-character SHA-256 fingerprint so players
  and automation tools can verify it:

  ```sh
  sixm-client your.server.example:30120 PlayerName --fingerprint <64 hex chars>
  ```

  `sixm-client` does not persist server fingerprints across runs, so
  `--fingerprint` must be passed on every launch.

## 4. Running on Linux with systemd

`sixm-server` stays running when standard input is closed (as under a service
manager), logs to stdout/stderr (controlled by `RUST_LOG`, defaulting to
`info`), and performs a graceful shutdown on `SIGINT` or `SIGTERM`—stopping
resources in reverse start order, flushing KVP writes to disk, and
disconnecting clients with `server shutting down`.

Create a dedicated system user and data directory:

```sh
sudo useradd --system --home-dir /srv/sixm --shell /usr/sbin/nologin sixm
sudo mkdir -p /srv/sixm/resources /srv/sixm/cache/kvp
sudo chown -R sixm:sixm /srv/sixm
sudo install -m 0755 target/release/sixm-server /usr/local/bin/sixm-server
```

Copy your `server.cfg` and resources into `/srv/sixm/`, then create
`/etc/systemd/system/sixm.service`:

```ini
[Unit]
Description=SixM Dedicated Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=sixm
Group=sixm
WorkingDirectory=/srv/sixm
ExecStart=/usr/local/bin/sixm-server
Environment=RUST_LOG=info

Restart=on-failure
RestartSec=5
KillSignal=SIGTERM
TimeoutStopSec=30

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/srv/sixm/cache

[Install]
WantedBy=multi-user.target
```

> [!NOTE]
> If any server-side resource calls `SaveResourceFile` to write files inside
> `resources/`, add `/srv/sixm/resources` to `ReadWritePaths` as well.

Enable, start, and inspect logs:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now sixm
sudo journalctl -u sixm -f
```

After the first start, read the server's fingerprint from the journal:

```sh
sudo journalctl -u sixm -n 50 --no-pager | grep "Server fingerprint"
```

## 5. Running on Windows

`sixm-server.exe` is a console application and does not implement the Windows
Service Control Manager (SCM) protocol directly. To run it in the background on
Windows Server:

- Use a service wrapper such as **NSSM** (Non-Sucking Service Manager) or
  create a **Task Scheduler** task triggered at system startup.
- Set the **working directory** (`AppDirectory` in NSSM, or "Start in" in Task
  Scheduler) to the folder containing `server.cfg`, `resources\`, and `cache\`.
- Run the service under a dedicated service account and ensure that same
  account owns `cache\server-identity.bin`, because `sixm-server` restricts
  that file's ACL to its owner.
- Capture stdout/stderr to a log file (or set `RUST_LOG=info`) so you can read
  the startup `Server fingerprint` and any script errors.

## 6. HTTP port, `sv_httpEnabled`, and TLS reverse proxy

When `sv_httpEnabled` is `true` (the default), `sixm-server` binds a **plain
HTTP/1.1** listener on the **TCP** port of `endpoint_add_udp` (default
`30120`), serving:

- `/info.json`
- `/players.json`
- `/dynamic.json`
- `/<resource>/...` for resources using `SetHttpHandler`

Because this TCP listener does not speak TLS:

### Option A: Disable HTTP if unused

If you do not need `/info.json`, `/players.json`, `/dynamic.json`, or resource
HTTP handlers, disable the TCP listener in `server.cfg`:

```text
set sv_httpEnabled false
```

The server will log `HTTP is off: sv_httpEnabled is false` and open only the
UDP socket.

### Option B: Put a TLS reverse proxy in front of HTTP

If external users or tools need HTTPS access to the JSON endpoints or
`SetHttpHandler` routes:

1. Keep `endpoint_add_udp 0.0.0.0:30120` so players can reach UDP `30120` from
   the internet.
2. **Block external access to TCP `30120`** in your host/cloud firewall so only
   local connections (`127.0.0.1`) can reach SixM's plain HTTP port.
3. Terminate TLS on port `443` with a reverse proxy (for example Caddy or
   Nginx) on the same machine and proxy to `http://127.0.0.1:30120`.

Example Caddy configuration (`/etc/caddy/Caddyfile`):

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:30120
}
```

Example Nginx server block:

```nginx
server {
    listen 443 ssl;
    server_name api.example.com;

    ssl_certificate     /etc/letsencrypt/live/api.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/api.example.com/privkey.pem;

    client_max_body_size 1m;

    location / {
        proxy_pass http://127.0.0.1:30120;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 130s;
    }
}
```

> [!NOTE]
> SixM's built-in HTTP listener enforces its own per-peer-address rate limit
> (`20` requests/second sustained, burst `60`, max `128` concurrent
> connections) on the TCP peer IP. When traffic arrives through a local reverse
> proxy, SixM sees `127.0.0.1` as the peer for all requests, so configure
> per-client-IP rate limiting at the reverse proxy if your HTTP endpoints are
> exposed to untrusted traffic.

## 7. Firewall rules (UDP + TCP)

`sixm-server` uses **one port number** (set by `endpoint_add_udp`, default
`30120`) for two protocols:

| Protocol | Port | Purpose | Expose publicly? |
|---|---|---|---|
| **UDP** | `30120` | QUIC game traffic (encrypted with TLS 1.3) | Yes (required for players) |
| **TCP** | `30120` | Plain HTTP/1.1 (`/info.json`, `/players.json`, `/dynamic.json`, `SetHttpHandler`) | Only if `sv_httpEnabled` is `true` and no reverse proxy handles TLS; otherwise block externally |
| **TCP** | `443` | Reverse proxy HTTPS (optional) | Yes, if using a TLS reverse proxy |

Remember to configure both your **host firewall** and any **cloud security
group / router port forwarding**.

### Linux (`ufw`)

With HTTP disabled (`sv_httpEnabled false`) or proxied locally over HTTPS:

```sh
sudo ufw allow 30120/udp comment "SixM QUIC game traffic"
sudo ufw allow 443/tcp comment "HTTPS reverse proxy for SixM HTTP"
```

If you intentionally expose SixM's TCP HTTP port directly on a trusted network:

```sh
sudo ufw allow 30120/tcp comment "SixM HTTP"
```

### Linux (`firewalld`)

```sh
sudo firewall-cmd --permanent --add-port=30120/udp
# Add 443/tcp for a TLS reverse proxy, or 30120/tcp only if exposing HTTP directly:
sudo firewall-cmd --permanent --add-port=443/tcp
sudo firewall-cmd --reload
```

### Windows Defender Firewall (PowerShell)

```powershell
New-NetFirewallRule -DisplayName "SixM Game (UDP 30120)" -Direction Inbound -Protocol UDP -LocalPort 30120 -Action Allow
# Optional, only if exposing HTTP directly on port 30120:
# New-NetFirewallRule -DisplayName "SixM HTTP (TCP 30120)" -Direction Inbound -Protocol TCP -LocalPort 30120 -Action Allow
```
