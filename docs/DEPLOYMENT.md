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

Authorization is fail-closed (ADR-0023): a cube is reachable only by a server
admin or the holder of a matching `Cube` grant. There is no open-by-default
posture; grant access through the security admin surface.

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

The binary registers with the Service Control Manager natively (no wrapper):

```bat
REM From an elevated (Administrator) prompt:
epiphany-server.exe service install
REM Set the service environment (EPIPHANY_DATA_DIR, EPIPHANY_BIND, ...), e.g.:
sc start Epiphany
REM ... and to remove it:
epiphany-server.exe service uninstall
```

`service install` registers an auto-start service that runs `service run`; a
`sc stop Epiphany` (or system shutdown) sends `SERVICE_CONTROL_STOP`, which the
server drains cleanly. Configure it through the service's environment block
(`EPIPHANY_*`), and point `EPIPHANY_DATA_DIR` at a service-writable,
ACL-restricted path (on Windows the secret files rely on directory ACLs). The
service runs as `LocalSystem` by default; assign a dedicated service account for
least privilege.

(WinSW or NSSM still work if you prefer an external supervisor, but are no longer
required.)

## Notes

- Put a reverse proxy (or `EPIPHANY_TLS`) in front for public exposure; the
  default bind is loopback-only.
- The scheduler and audit/run-ledger recover on restart, so a hard kill is safe
  for durability - SIGTERM handling just makes the stop graceful.
