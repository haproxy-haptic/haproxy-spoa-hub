# Data Model: HAProxy SPOA Plugin Hub

**Date**: 2026-03-30
**Feature**: 001-spoa-plugin-hub

## Entities

### Hub (runtime singleton)

The main process managing the SPOP listener(s), plugin registry, and
message dispatch.

| Field | Type | Description |
|-------|------|-------------|
| config | HubConfig | Parsed TOML configuration |
| plugins | PluginRegistry | Loaded and validated plugins |
| listeners | Vec\<Listener\> | Active Unix/TCP listeners |

**Lifecycle**: Created at startup, runs until SIGTERM/SIGINT.

### HubConfig

Deserialized from the TOML configuration file.

| Field | Type | Description |
|-------|------|-------------|
| listeners | Vec\<ListenerConfig\> | Socket bind addresses |
| plugin_dir | PathBuf | Directory containing plugin `.so` files |
| plugins | Vec\<PluginConfig\> | Per-plugin configuration entries |
| metrics_addr | Option\<SocketAddr\> | Prometheus endpoint bind address |
| log_level | String | Default log level (env-filter syntax) |
| default_timeout_ms | u64 | Default per-plugin timeout |

### ListenerConfig

| Field | Type | Description |
|-------|------|-------------|
| type | "unix" \| "tcp" | Transport type |
| address | String | Socket path or `host:port` |

### PluginConfig

Maps a plugin `.so` to one or more SPOE message names.

| Field | Type | Description |
|-------|------|-------------|
| name | String | Plugin identifier (used as variable namespace prefix) |
| library | String | Filename of the `.so` in plugin_dir |
| messages | Vec\<String\> | SPOE message names this plugin handles |
| depends_on | Vec\<String\> | Plugin names that must complete before this one |
| timeout_ms | Option\<u64\> | Per-plugin timeout override |

### Plugin (runtime)

A loaded and initialized plugin instance.

| Field | Type | Description |
|-------|------|-------------|
| name | String | From PluginConfig |
| version | String | Reported by the plugin at load time |
| abi_version | u32 | Checked against hub's expected version |
| instance | PluginBox | abi_stable trait object (Send + Sync) |
| messages | Vec\<String\> | SPOE messages this plugin handles |
| depends_on | Vec\<String\> | Dependency plugin names |
| timeout | Duration | Effective timeout for this plugin |

**Lifecycle**: Created during plugin loading at startup. Destroyed on
shutdown (or on SIGHUP removal in the reload scenario).

### PluginRegistry

| Field | Type | Description |
|-------|------|-------------|
| plugins | HashMap\<String, Plugin\> | Loaded plugins keyed by name |
| message_map | HashMap\<String, Vec\<ExecutionPlan\>\> | SPOE message name → ordered execution groups |

### ExecutionPlan

Represents how plugins are executed for a given SPOE message. Plugins
within the same group run in parallel; groups execute sequentially.

| Field | Type | Description |
|-------|------|-------------|
| groups | Vec\<Vec\<String\>\> | Ordered list of parallel execution groups (plugin names) |

Example for message "check-request" with plugins A, B (independent)
and C (depends on A):
```
groups: [["A", "B"], ["C"]]
```
A and B run in parallel, then C runs with A's output merged into its
input.

### SpoeMessage (plugin API)

Pre-parsed SPOE message passed to plugins.

| Field | Type | Description |
|-------|------|-------------|
| name | String | SPOE message name |
| args | HashMap\<String, SpoeValue\> | Typed message arguments |
| stream_id | u64 | HAProxy stream identifier |
| frame_id | u64 | HAProxy frame identifier |

### SpoeValue (plugin API)

Typed value from an SPOE message argument.

| Variant | Payload | Description |
|---------|---------|-------------|
| Null | — | No value |
| Bool | bool | Boolean |
| Int32 | i32 | Signed 32-bit integer |
| Uint32 | u32 | Unsigned 32-bit integer |
| Int64 | i64 | Signed 64-bit integer |
| Uint64 | u64 | Unsigned 64-bit integer |
| Ipv4 | [u8; 4] | IPv4 address |
| Ipv6 | [u8; 16] | IPv6 address |
| String | String | UTF-8 string |
| Binary | Vec\<u8\> | Raw byte buffer |

### ProcessingResult (plugin API)

Returned by a plugin after processing a message.

| Field | Type | Description |
|-------|------|-------------|
| variables | Vec\<TxnVariable\> | Transaction variables to set |

### TxnVariable (plugin API)

A single transaction variable set by a plugin.

| Field | Type | Description |
|-------|------|-------------|
| scope | VarScope | Variable scope (process/session/txn/req/resp) |
| name | String | Variable name (without namespace prefix — hub adds it) |
| value | SpoeValue | Variable value |

### VarScope (plugin API)

| Variant | Description |
|---------|-------------|
| Process | Persists for the HAProxy process lifetime |
| Session | Persists for the HAProxy session |
| Transaction | Persists for the current transaction |
| Request | Persists for the current request |
| Response | Persists for the current response |

## Relationships

```
HubConfig 1──* ListenerConfig
HubConfig 1──* PluginConfig
PluginRegistry 1──* Plugin
PluginRegistry 1──* ExecutionPlan
Plugin *──* SpoeMessage (via message_map dispatch)
SpoeMessage 1──* SpoeValue (args)
ProcessingResult 1──* TxnVariable
TxnVariable 1──1 SpoeValue
TxnVariable 1──1 VarScope
```

## State Transitions

### Plugin Lifecycle

```
Loading → Validating → Initializing → Ready → [Processing ↔ Idle] → Destroying
                ↓              ↓
           Rejected       InitFailed
           (ABI mismatch) (init() returned Err)
```

### Connection Lifecycle

```
Accepted → Handshake → Active → [Processing ↔ Idle] → Disconnecting → Closed
               ↓                       ↓
          HandshakeFailed         ErrorDisconnect
```
