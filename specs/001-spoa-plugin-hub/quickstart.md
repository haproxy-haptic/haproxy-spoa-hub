# Quickstart: HAProxy SPOA Plugin Hub

## Prerequisites

- Rust toolchain (stable, >= 1.85)
- HAProxy >= 2.0 (with SPOE support)

## Build the Hub

```bash
git clone <repository-url>
cd haproxy-spoa-hub
cargo build --release
```

The binary is at `target/release/haproxy-spoa-hub`.

## Build the Example Plugin

```bash
cargo build --release -p noop-plugin
```

The plugin `.so` is at `target/release/libnoop_plugin.so`.

## Configure

Create `/etc/haproxy-spoa-hub/config.toml`:

```toml
[[listeners]]
type = "unix"
address = "/var/run/haproxy-spoa-hub.sock"

plugin_dir = "/etc/haproxy-spoa-hub/plugins"

[[plugins]]
name = "noop"
library = "libnoop_plugin.so"
messages = ["check-request"]
```

Copy the plugin:

```bash
mkdir -p /etc/haproxy-spoa-hub/plugins
cp target/release/libnoop_plugin.so /etc/haproxy-spoa-hub/plugins/
```

## Configure HAProxy

Add to your `haproxy.cfg`:

```haproxy
global
    # (existing global settings)

frontend https
    bind *:443 ssl crt /path/to/cert.pem
    filter spoe engine spoa-hub config /etc/haproxy/spoe-hub.conf
    # Use variables set by plugins, e.g.:
    http-request set-header X-Noop %[var(txn.noop.result)]

backend app
    server app1 127.0.0.1:8080
```

Create `/etc/haproxy/spoe-hub.conf`:

```
[spoa-hub]
spoe-agent spoa-hub-agent
    messages check-request
    option var-prefix hub
    timeout hello      2s
    timeout idle       120s
    timeout processing 100ms
    use-backend spoa-hub-backend

spoe-message check-request
    args src=src dst=dst method=method path=path
    event on-frontend-http-request

backend spoa-hub-backend
    mode tcp
    server hub /var/run/haproxy-spoa-hub.sock
```

## Run

```bash
# Validate configuration
haproxy-spoa-hub --config /etc/haproxy-spoa-hub/config.toml --check

# Start the hub
haproxy-spoa-hub --config /etc/haproxy-spoa-hub/config.toml
```

## Verify

Send a request through HAProxy and check the hub logs (stderr) for
plugin load and message dispatch events:

```bash
curl -k https://localhost:443/
```

Expected log output:
```json
{"timestamp":"...","level":"INFO","message":"plugin loaded","plugin":"noop","version":"0.1.0"}
{"timestamp":"...","level":"INFO","message":"listening","transport":"unix","address":"/var/run/haproxy-spoa-hub.sock"}
{"timestamp":"...","level":"DEBUG","message":"message dispatched","message_name":"check-request","plugins":["noop"],"latency_ms":0.05}
```

## Write Your Own Plugin

See [contracts/plugin-api.md](contracts/plugin-api.md) for the full
plugin API contract and author workflow.
