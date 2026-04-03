# Quickstart: Hub Control Args

## Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
```

## Skipping a plugin for allowlisted traffic

### HAProxy configuration

Define the SPOE message with `hub.skip_plugins` as an arg:

```
[spoa-hub]
spoe-agent spoa-hub-agent
    messages check-request
    option var-prefix hub
    use-backend spoa-hub-backend

spoe-message check-request
    args src=src method=method path=path hub.skip_plugins=var(txn.skip_plugins)
    event on-frontend-http-request
```

Use ACLs to set the skip variable conditionally:

```haproxy
frontend http-in
    bind *:8080
    filter spoe engine spoa-hub config /etc/haproxy/spoe-hub.conf

    acl allowlisted src -f /etc/haproxy/allowlist.txt
    http-request set-var(txn.skip_plugins) str("rate-limiter") if allowlisted

    # Health endpoints skip all plugins
    acl is_health path /health /ready
    http-request set-var(txn.skip_plugins) str("rate-limiter,geoip,bot-detect") if is_health

    default_backend app
```

### Hub configuration

No hub config changes needed. The hub automatically recognizes `hub.*` args.

```toml
plugin_dir = "/opt/spoa-plugins"

[[listeners]]
type = "unix"
address = "/var/run/haproxy-spoa-hub.sock"

[[plugins]]
name = "rate-limiter"
library = "librate_limiter.so"
messages = ["check-request"]

[[plugins]]
name = "geoip"
library = "libgeoip.so"
messages = ["check-request"]
```

### Verify

Send a request from an allowlisted IP:

```sh
curl -H "X-Forwarded-For: 10.0.0.1" http://localhost:8080/
```

Check hub logs — the rate-limiter plugin should be skipped:

```json
{"level":"DEBUG","message":"plugin skipped","plugin":"rate-limiter"}
{"level":"DEBUG","message":"plugin completed","plugin":"geoip","latency_us":42}
```

## Dependency cascade

If `bot-detect` depends on `ja3`:

```toml
[[plugins]]
name = "ja3"
library = "libja3.so"
messages = ["check-request"]

[[plugins]]
name = "bot-detect"
library = "libbot_detect.so"
messages = ["check-request"]
depends_on = ["ja3"]
```

Skipping `ja3` automatically cascades to `bot-detect`:

```
hub.skip_plugins = "ja3"  →  both ja3 and bot-detect are skipped
```

## Monitoring

The hub exposes a `spoa_plugin_skips_total` counter:

```
# HELP spoa_plugin_skips_total Plugins skipped via hub.skip_plugins arg
# TYPE spoa_plugin_skips_total counter
spoa_plugin_skips_total{plugin="rate-limiter",service="haproxy-spoa-hub"} 1542
```
