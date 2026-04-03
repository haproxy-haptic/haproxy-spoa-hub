# Plugin API Contract

**Crate**: `haproxy-spoa-hub-plugin-api`
**ABI Framework**: `abi_stable` 0.11
**Stability**: Semver-governed. Minor versions add optional methods
via prefix types. Major versions may break the ABI.

## Root Module

Every plugin `.so` exports a root module via `#[export_root_module]`.
The root module provides a factory function to create plugin instances.

```rust
#[repr(C)]
#[derive(StableAbi)]
#[sabi(kind(Prefix(prefix_ref = PluginMod_Ref)))]
#[sabi(missing_field(panic))]
pub struct PluginMod {
    /// Create a new plugin instance.
    #[sabi(last_prefix_field)]
    pub new: extern "C" fn() -> RResult<PluginBox, RBoxError>,
}
```

`BASE_NAME` is not used for loading (hub uses `load_from_file()`
with explicit paths from config).

## Plugin Trait

```rust
#[sabi_trait]
pub trait SpoePlugin: Send + Sync + Debug {
    /// Initialize the plugin. Called once after loading.
    /// Return RErr to abort registration.
    fn init(&mut self) -> RResult<(), RBoxError>;

    /// Process an SPOE message. Return transaction variables.
    /// MUST NOT panic — use the define_plugin! macro.
    fn process(
        &self,
        message: &SpoeMessage,
    ) -> RResult<ProcessingResult, RBoxError>;

    /// Human-readable plugin name.
    fn name(&self) -> RStr<'_>;

    /// SemVer version string.
    #[sabi(last_prefix_field)]
    fn version(&self) -> RStr<'_>;

    // Methods below are after last_prefix_field — optional with defaults.

    /// Return a JSON Schema string to validate plugin configuration.
    /// Default: RNone (no validation).
    fn config_schema(&self) -> ROption<RString> {
        ROption::RNone
    }
}
```

### Thread Safety

`Send + Sync` is required from v0.1.0. Plugin instances are shared
across connections via `Arc`. The `process` method takes `&self`
(shared reference) — plugins must use interior mutability (e.g.,
`RwLock`, `Mutex`, atomics) for any mutable state.

### Panic Safety

The `define_plugin!` macro wraps the plugin's `process`
implementation in `std::panic::catch_unwind`, converting panics to
`RResult::RErr` before they reach the `extern "C"` boundary.

Plugin authors MUST use this macro. Direct trait implementations
without catch_unwind will abort the hub process on panic.

## Data Types

### SpoeMessage

```rust
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct SpoeMessage {
    pub name: RString,
    pub args: RHashMap<RString, SpoeValue>,
    pub stream_id: u64,
    pub frame_id: u64,
}
```

### SpoeValue

```rust
#[repr(u8)]
#[derive(Debug, Clone, StableAbi)]
pub enum SpoeValue {
    Null,
    Bool(bool),
    Int32(i32),
    Uint32(u32),
    Int64(i64),
    Uint64(u64),
    Ipv4([u8; 4]),
    Ipv6([u8; 16]),
    String(RString),
    Binary(RVec<u8>),
}
```

### ProcessingResult

```rust
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct ProcessingResult {
    pub variables: RVec<TxnVariable>,
}
```

### TxnVariable

```rust
#[repr(C)]
#[derive(Debug, Clone, StableAbi)]
pub struct TxnVariable {
    pub scope: VarScope,
    pub name: RString,
    pub value: SpoeValue,
}
```

### VarScope

```rust
#[repr(u8)]
#[derive(Debug, Clone, StableAbi)]
pub enum VarScope {
    Process = 0,
    Session = 1,
    Transaction = 2,
    Request = 3,
    Response = 4,
}
```

## Plugin Author Workflow

1. Create a new Rust crate:
   ```
   cargo new --lib my-spoa-plugin
   ```

2. Set crate type and add dependency:
   ```toml
   [lib]
   crate-type = ["cdylib"]

   [dependencies]
   haproxy-spoa-hub-plugin-api = "0.1"
   abi_stable = "0.11"
   ```

3. Implement the plugin using the `define_plugin!` macro:
   ```rust
   use haproxy_spoa_hub_plugin_api::*;

   struct MyPlugin;

   define_plugin!(MyPlugin, |plugin, message| {
       // Access message arguments
       let client_ip = message.args.get(&"src".into());

       // Return transaction variables
       Ok(ProcessingResult {
           variables: vec![
               TxnVariable {
                   scope: VarScope::Session,
                   name: "result".into(),
                   value: SpoeValue::String("processed".into()),
               },
           ].into(),
       })
   });

   impl MyPlugin {
       fn new() -> Self { MyPlugin }
   }
   ```

4. Build: `cargo build --release`

5. Deploy: copy `target/release/libmy_spoa_plugin.so` to the hub's
   plugin directory.

## Versioning Rules

- Hub loads plugins with matching major version (or matching minor
  for 0.x).
- Methods after `#[sabi(last_prefix_field)]` may not exist in older
  plugins — the hub must handle this gracefully.
- Adding new `SpoeValue` variants is a major version bump (changes
  enum layout).
- Adding new fields to `SpoeMessage` requires prefix types on the
  struct.

## Variable Namespacing

Plugins set variables using unprefixed names. The hub automatically
prefixes each variable with `<plugin_name>.` before building the ACK
response. For example, a plugin named "geoip" setting variable
"country_code" results in the HAProxy variable `geoip.country_code`.

## Dependency Data Flow

When plugin B depends on plugin A, the hub merges A's output variables
(with namespace prefix) into the `args` map passed to B. Plugin B
receives both the original SPOE message arguments and A's results
as additional entries keyed by their prefixed names (e.g.,
`ja3.hash`).
