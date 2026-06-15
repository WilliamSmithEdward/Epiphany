# Running Epiphany as a service

Epiphany is a single self-contained binary. It binds an HTTP (or HTTPS) port,
persists to a data directory, and **shuts down gracefully on SIGTERM and Ctrl-C**
(SIGINT) - draining in-flight requests - so it behaves correctly under a service
manager. Durability does not depend on a clean stop (every write is fsynced to
the WAL), but a clean stop avoids cutting off active requests.

Sample units live in [`deploy/`](../deploy): a systemd unit, a launchd plist, and
a Dockerfile.

## Configuration

All configuration is environment variables (so it drops straight into a unit
file). Defaults in parentheses.

| Variable | Purpose |
|---|---|
| `EPIPHANY_BIND` (`127.0.0.1:8080`) | Address to bind. Use `0.0.0.0:8080` to listen on all interfaces. |
| `EPIPHANY_DATA_DIR` (`data`) | Where cubes, security, audit, and run-ledger files live. |
| `EPIPHANY_TLS` | `on` serves HTTPS with an auto-generated self-signed cert (ADR-0019). |
| `EPIPHANY_TLS_CERT` / `EPIPHANY_TLS_KEY` | Operator PEM cert + key (takes precedence over the self-signed path). |
| `EPIPHANY_LOG` | Log filter (e.g. `info`, `epiphany=debug`). |
| `EPIPHANY_SCHEDULER_TICK_SECS` (`1`) | Scheduler tick; `0` disables the scheduler. |
| `EPIPHANY_DEFAULT_CUBE_ACCESS` | `open` for the trusted-single-org posture; default is closed (secure). |

On first run the server writes the generated admin password to
`<data_dir>/server/admin-password.txt` (owner-only) and logs only its path.

## Linux (systemd)

```sh
sudo useradd --system --home /var/lib/epiphany epiphany
sudo cp epiphany-server /usr/local/bin/
sudo cp deploy/epiphany.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now epiphany
sudo systemctl status epiphany
journalctl -u epiphany -f
```

The unit pins a dedicated unprivileged user, a managed `StateDirectory`, and
sandbox hardening (`ProtectSystem=strict`, `NoNewPrivileges`, `PrivateTmp`).
`systemctl stop` sends SIGTERM, which drains cleanly.

## Containers (Docker)

```sh
docker build -f deploy/Dockerfile -t epiphany .
docker run -p 8080:8080 -v epiphany-data:/var/lib/epiphany epiphany
```

The image runs as an unprivileged user with the data directory on a volume;
`docker stop` sends SIGTERM (clean drain). Set `EPIPHANY_*` with `-e`.

## macOS (launchd)

```sh
sudo cp epiphany-server /usr/local/bin/
sudo cp deploy/com.epiphany.server.plist /Library/LaunchDaemons/
sudo launchctl load /Library/LaunchDaemons/com.epiphany.server.plist
```

`launchctl unload` (or a system stop) sends SIGTERM, which drains cleanly.

## Windows

The binary does not register with the Windows Service Control Manager directly;
wrap it with a service shim:

- **WinSW** or **NSSM**: point the wrapper at `epiphany-server.exe`, set the
  `EPIPHANY_*` variables, and it runs as a Windows service. Both forward a stop
  to the process so the server's Ctrl-C / close-event handler drains in-flight
  requests.
- Set the data directory to a service-writable, ACL-restricted path via
  `EPIPHANY_DATA_DIR` (the secret files rely on directory ACLs on Windows).

A native SCM integration (so `sc.exe` controls it without a wrapper) is a
possible future addition.

## Notes

- Put a reverse proxy (or `EPIPHANY_TLS`) in front for public exposure; the
  default bind is loopback-only.
- The scheduler and audit/run-ledger recover on restart, so a hard kill is safe
  for durability - SIGTERM handling just makes the stop graceful.
