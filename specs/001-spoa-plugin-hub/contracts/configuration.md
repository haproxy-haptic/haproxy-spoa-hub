# Configuration Contract

**Format**: TOML
**Path**: Configurable via CLI (`-c` / `--config`), default
`/etc/haproxy-spoa-hub/config.toml`

## Example Configuration

```toml
# Listener configuration
[[listeners]]
type = "unix"
address = "/var/run/haproxy-spoa-hub.sock"

[[listeners]]
type = "tcp"
address = "127.0.0.1:12345"

# Plugin directory
plugin_dir = "/etc/haproxy-spoa-hub/plugins"

# Default timeout for all plugins (milliseconds)
default_timeout_ms = 100

# Prometheus metrics endpoint (omit to disable)
metrics_addr = "0.0.0.0:9090"

# Log level (RUST_LOG syntax, overrides env var)
log_level = "info"

# Plugin definitions
[[plugins]]
name = "ja3"
library = "libja3_fingerprint.so"
messages = ["check-client-hello"]
timeout_ms = 50

[[plugins]]
name = "geoip"
library = "libgeoip_lookup.so"
messages = ["check-request"]
timeout_ms = 30

[[plugins]]
name = "bot-detect"
library = "libbot_detection.so"
messages = ["check-request"]
depends_on = ["ja3"]
timeout_ms = 80
```

## Field Reference

### Top Level

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| listeners | array of table | yes | — | At least one listener |
| plugin_dir | string | yes | — | Path to plugin `.so` directory |
| default_timeout_ms | integer | no | 100 | Default per-plugin timeout |
| metrics_addr | string | no | disabled | Prometheus endpoint bind address |
| log_level | string | no | "info" | Log level (RUST_LOG syntax) |
| worker_threads | integer | no | num CPUs | Tokio runtime worker threads |
| max_connections | integer | no | unlimited | Maximum concurrent SPOP connections |
| plugins | array of table | yes | — | Plugin configurations |

### [[listeners]]

| Field | Type | Required | Values | Description |
|-------|------|----------|--------|-------------|
| type | string | yes | "unix", "tcp" | Transport type |
| address | string | yes | — | Socket path or `host:port` |

### [[plugins]]

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| name | string | yes | — | Unique plugin identifier (used as variable namespace) |
| library | string | yes | — | Filename of `.so` in plugin_dir |
| messages | array of string | yes | — | SPOE message names to handle |
| depends_on | array of string | no | [] | Plugin names that must complete first |
| timeout_ms | integer | no | default_timeout_ms | Per-plugin timeout override |
| params | table | no | {} | Typed parameters passed to `init()` (supports nested tables, arrays, all TOML types) |

## Validation Rules

Applied at startup (FR-015, SC-007):

1. At least one listener must be configured.
2. `plugin_dir` must exist and be readable.
3. Each plugin `name` must be unique.
4. Each plugin `library` must exist in `plugin_dir`.
5. `depends_on` references must name plugins that are also configured.
6. Dependency graph must be acyclic (no circular dependencies).
7. Each SPOE message name must have at least one plugin registered.
8. `timeout_ms` must be positive.
9. `metrics_addr` must be a valid socket address if provided.

## CLI Arguments

| Flag | Short | Description |
|------|-------|-------------|
| `--config <path>` | `-c` | Path to config file |
| `--check` | — | Validate config and exit |
| `--version` | `-V` | Print version and exit |
