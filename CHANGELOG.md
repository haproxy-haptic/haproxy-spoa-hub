# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Nested plugin configuration: `[plugins.params]` now supports all
  native TOML value types (integers, floats, booleans, arrays, nested
  tables) in addition to strings
- `ConfigValue` type in the plugin API for accessing typed configuration
  values with `as_str()`, `as_integer()`, `as_float()`, `as_bool()`,
  `as_array()`, and `as_table()` accessor methods
- Optional JSON Schema validation for plugin configuration: plugins can
  declare a schema via `config_schema()` and the hub validates config
  at startup before calling `init()`
- `define_plugin!` macro now accepts an optional `fn config_schema()`
  block for declaring a JSON Schema

### Changed

- **Breaking**: `PluginContext.config` type changed from
  `RHashMap<RString, RString>` to `RHashMap<RString, ConfigValue>`.
  Plugins accessing string parameters must now use
  `context.config.get(&key).and_then(|v| v.as_str())` instead of
  `context.config.get(&key)`.
- `PluginConfig.params` changed from `HashMap<String, String>` to
  `toml::Table` internally. Existing TOML config files with flat
  string params continue to work without modification.

### Added (prior)

- Core SPOA hub binary with plugin dispatch architecture
- Plugin API crate (`haproxy-spoa-hub-plugin-api`) for building
  plugins as independently compiled shared libraries
- `define_plugin!` macro with automatic panic safety via
  `catch_unwind`
- SPOP protocol support (HAPROXY-HELLO, NOTIFY/ACK, DISCONNECT)
  via the `spop` crate
- Unix domain socket and TCP listener support
- Parallel plugin execution for independent plugins
- Plugin dependency declarations with topological execution ordering
- Per-plugin configurable timeouts with partial result return
- Automatic variable namespace prefixing (`<plugin>.<varname>`)
- TOML configuration with startup validation
- Structured JSON logging to stderr via `tracing`
- Prometheus metrics endpoint (`/metrics`) with per-plugin latency
  histograms
- ABI version checking at plugin load time via `abi_stable`
- Example plugins: noop, echo, delay
- Graceful shutdown on SIGTERM/SIGINT
