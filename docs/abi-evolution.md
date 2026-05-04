# Plugin ABI evolution — operator and contributor reference

A practical guide to evolving `haproxy-spoa-hub-plugin-api` without
breaking field deployments. The design rationale is in
[ADR-0001](./adr/0001-abi-evolution.md); this page focuses on **what
to do** when you want to add a method, change a type, or audit
compatibility.

## Mental model in one paragraph

Every plugin exports a `#[repr(C)]` struct of function pointers,
addressed via the `get_plugin_vtable` symbol. The struct's first
field is a `u32` `api_version` that tells the hub which fields beyond
the v1 baseline are valid. The hub uses `dlopen` + `dlsym` to reach
the struct and version-gates access to newer fields. As long as we
only ever **append** to the struct and bump `PLUGIN_API_VERSION` per
append, plugins compiled against older versions of plugin-api keep
loading on newer hubs — the new fields are simply unavailable for
them and the host falls back to a default.

## What kind of change am I making?

| Change | Impact | Required action |
|--------|--------|-----------------|
| Adding a new method to `PluginVTable` | Minor bump | Append the field, bump `PLUGIN_API_VERSION_V*` and `PLUGIN_API_VERSION`, add a host accessor with a version gate, update matrix-test expectations, optionally extend `define_plugin!` macro to expose a new optional arm. |
| Adding a new variant to `ConfigValue` / `SpoeValue` / `VarScope` | Minor bump | Add the variant immediately before `__Other`. Plugins using `_ =>` wildcard arms see no break. |
| Adding a new field to `PluginContext` / `SpoeMessage` / `Diagnostic` / `ProcessingResult` / `TxnVariable` | Major bump | These are passed by reference across FFI; layout is part of the contract. Talk to the team before doing this; the migration plan probably involves shipping the new field as a separate type. |
| Changing the type of an existing field | Major bump | Rebuild every plugin. Avoid. |
| Removing a field, method, or variant | Major bump | Rebuild every plugin. Avoid. |
| Bumping the pinned `abi_stable` version | Major bump | `RString` / `RVec` / `RHashMap` layouts could shift between abi_stable versions. The pin is `=0.11.3`; touching it is a coordinated rollout, not a routine patch. |

## Worked example: appending a method

Goal: add a `metrics(&self) -> Vec<MetricSample>` method that lets
plugins report runtime metrics.

### 1. Append the field in `crates/plugin-api/src/vtable.rs`

```rust
pub const PLUGIN_API_VERSION_V1: u32 = 1;
pub const PLUGIN_API_VERSION_V2: u32 = 2;        // NEW
pub const PLUGIN_API_VERSION: u32 = PLUGIN_API_VERSION_V2;   // bumped

#[repr(C)]
pub struct PluginVTable {
    // … v1 baseline fields unchanged …
    pub validate: extern "C" fn(state: *const c_void, ctx: &PluginContext) -> RVec<Diagnostic>,
    // ============================================================
    // End of v1 baseline. Future additions appear below this line.
    // ============================================================
    /// Plugin metrics. Only valid when api_version >= V2.   // NEW
    pub metrics: extern "C" fn(state: *const c_void) -> RVec<MetricSample>,
}
```

Do **not** reorder or remove anything above the v1 marker comment.
The marker comment is itself part of the contract.

### 2. Extend the `define_plugin!` macro

The existing pattern for `validate` and `config_schema` is to accept
an optional arm. Mirror it for `metrics`. Plugins that don't override
get a thunk returning `RVec::new()`. Plugins that do get their body
wrapped in a thunk that casts the state pointer back to `&Self` and
calls the implementation method.

### 3. Add a version-gated host accessor in `crates/hub/src/plugin.rs`

```rust
impl Plugin {
    /// Plugin metrics, available only on plugins built against
    /// `plugin-api >= 0.5.0` (api_version V2). Returns `None` for
    /// older plugins.
    #[must_use]
    pub fn metrics(&self) -> Option<RVec<MetricSample>> {
        if self.api_version < PLUGIN_API_VERSION_V2 {
            return None;
        }
        // SAFETY: api_version >= V2, so the metrics field is in-bounds.
        let f = unsafe { (*self.vtable).metrics };
        Some(f(self.state))
    }
}
```

The version gate is what makes the `unsafe { (*self.vtable).metrics }`
read sound. **Without it, the read goes past the end of older plugins'
allocations** — the kind of UB that doesn't crash on every machine
(reads what's adjacent in memory, which could be anything) but
absolutely will eventually.

### 4. Update the matrix test

`crates/hub/tests/abi-matrix-versions.toml` lists every published
plugin version. After the bump, every cell from the previous plugin
generation should be marked `compatible` with `metrics_supported = false`.
The test asserts `plugin.metrics().is_none()` for those cells.

### 5. Bump versions

- `crates/plugin-api/Cargo.toml`: `0.4.x` → `0.5.0`.
- Plugins update their dependency: `haproxy-spoa-hub-plugin-api = "0.5"`.
  Their own version is a minor bump too; old binaries built against
  0.4 keep working.
- `CHANGELOG.md`: entry under "Added".

### What NOT to change

- Do NOT bump `PLUGIN_API_VERSION_V1`. It pins the v1 baseline
  forever.
- Do NOT move the `// End of v1 baseline` marker comment up to
  include the new field. New fields go below the marker; the marker
  itself only moves on a major version bump where we redefine the
  baseline.
- Do NOT remove the host accessor's version gate just because every
  plugin you happen to have rebuilt today reports V2. Older plugins
  will still exist in the field.

## Auditing for compat

Before merging a plugin-api change:

1. **Run `cargo test --workspace`** — catches obvious breaks.
2. **Run `cargo test --workspace --test abi_matrix`** — catches
   layout drift against every published plugin version.
3. **`git diff crates/plugin-api/src/vtable.rs`** — eyeball that the
   change is append-only and that `PLUGIN_API_VERSION` was bumped
   if a field was appended.
4. **Eyeball every change to `crates/hub/src/plugin.rs`** for missing
   version gates.
5. **`grep -n "abi_stable" crates/{plugin-api,hub}/Cargo.toml`** —
   the pin should still be `=0.11.3`. If it changed, the matrix test
   needs to run against published plugin .so's that link against the
   PREVIOUS abi_stable version too — and the change is no longer a
   routine patch.

## When the matrix test goes red

Possibilities, in order of likelihood:

1. **A field was reordered or removed.** Revert the change. The matrix
   test is doing exactly its job.
2. **`PLUGIN_API_VERSION` wasn't bumped after appending a field.** The
   hub thinks every plugin has the new field; older plugins fail the
   load check or get UB on the field read.
3. **`abi_stable` was unpinned and a different patch version was
   resolved.** Re-pin or update the pin everywhere consistently.
4. **The plugin .so is genuinely from an unsupported era.** Update
   `abi-matrix-versions.toml` to mark that cell as `hard_incompatible`
   with the expected error message.

If it's #4, document the policy: which plugin versions does the
current hub support? When a plugin is too old to load, the hub should
surface a clear error pointing operators at the upgrade path.

## When the design itself doesn't fit anymore

We might want to break the model entirely if any of these become true:

- We need to support plugins from a totally different language (C,
  Go) — at which point the macro's Rust-specific bits are wasted.
  Switching to a flat `extern "C"` ABI without the macro becomes
  reasonable.
- A plugin needs to call back INTO the hub mid-method (today the FFI
  is one-directional). That requires either passing a callback table
  in `PluginContext` or moving to a richer protocol like Wasm.
- We pick up so many new fields that the v1 baseline becomes a
  performance footgun (vtable getting too large for cache lines).
  At ~10 fields today there's no concern; revisit if we ever hit 30+.

If any of these come up, write a new ADR superseding 0001. Don't
patch this one.
