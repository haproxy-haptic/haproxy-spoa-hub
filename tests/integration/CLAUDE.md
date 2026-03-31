# tests/integration/

Docker-based integration tests that run HAProxy + the hub with real SPOE
communication over a Unix socket.

## Commands

```sh
make test              # Run all three tests
make test-quickstart   # T057: curl → HAProxy → hub → verify plugin ran
make test-sigterm      # T048: SIGTERM graceful shutdown, exit code 0
make test-sighup       # T049: SIGHUP config reload, new plugin loaded
make build             # Build the hub Docker image only
make clean             # Tear down containers and prune images
```

## Architecture

- **docker-compose.yml**: Two services (`hub` + `haproxy`) sharing a named
  volume (`spoe-sock`) for the Unix socket.
- **Dockerfile.hub**: Multi-stage build — `rust:1.85-bookworm` builder,
  `debian:bookworm-slim` runtime. Bakes in all three plugin `.so` files.
- **HAProxy** connects to the hub via `/var/run/spoe/hub.sock` (SPOE
  backend defined in `haproxy.cfg`, not in `spoe-hub.conf`).

## Test Scripts

Each script in `scripts/` is self-contained:
- Generates a random `HAPROXY_PORT` to avoid conflicts
- Uses a unique compose project name (`t057-$$`, `t048-$$`, `t049-$$`)
- Cleans up via `trap cleanup EXIT`
- Prints `PASS` or `FAIL` and exits non-zero on failure

## Configuration Files

| File | Purpose |
|------|---------|
| `hub-config/config.toml` | Noop plugin on Unix socket |
| `hub-config/config-with-echo.toml` | Noop + echo (for SIGHUP test) |
| `hub-config/config-delay.toml` | Delay plugin (for SIGTERM test) |
| `haproxy/haproxy.cfg` | Frontend with SPOE filter + reply backend |
| `haproxy/spoe-hub.conf` | SPOE agent definition, `check-request` message |

## TOML Ordering

Hub config files must place scalar keys (`plugin_dir`, `default_timeout_ms`,
`log_level`) **before** any `[[listeners]]` or `[[plugins]]` table arrays.
Otherwise TOML parses them as part of the preceding table array entry.

## SIGHUP Test Details

The `test_sighup.sh` script:
1. Starts hub with noop-only config
2. Sends a request, verifies noop runs (via hub logs)
3. Copies `config-with-echo.toml` over `config.toml` inside the container
4. Sends SIGHUP → hub reloads config via ArcSwap registry swap
5. Sends another request, verifies echo plugin now runs too

The `/config` volume is a bind mount (writable) so `docker exec cp` works.
