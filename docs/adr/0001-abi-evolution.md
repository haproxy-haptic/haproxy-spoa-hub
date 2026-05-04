# ADR-0001: Hand-rolled `#[repr(C)]` vtable for plugin ABI

- Status: accepted
- Date: 2026-05-04
- Decision driver: an `abi_stable` minor-version bump silently broke
  every published plugin in the field; the chart that depended on it
  shipped to main with a green CI gate. We need an ABI evolution model
  that is verifiable at runtime, supports adding methods to plugin-api
  without re-releasing every plugin, and survives the maintenance death
  of any underlying crate.

## Context

The original plugin-api (v0.1 – v0.3) used `abi_stable`'s
`#[sabi_trait]` for the `SpoePlugin` trait, plus a `PluginMod` prefix-
type reached via `RootModule::load_from_file`. This worked while the
plugin-api was append-only inside a single `abi_stable` minor version.
It broke when:

1. We bumped `haproxy-spoa-hub-plugin-api` from 0.2 to 0.3 to add a
   `validate()` method (with a default impl on the trait, which is
   source-compatible).
2. `abi_stable` regenerated the prefix-type's package-version stamp
   from `CARGO_PKG_VERSION`, so plugins compiled against 0.2 reported
   `package:'plugin-api' version:'0.2.0'` and the hub built against
   0.3 expected `version:'0.3.0'`. The runtime layout-compatibility
   check at `abi_stable/src/library/lib_header.rs` rejected every
   load with `IncompatibleVersionNumber`.
3. CI's chart unit suite passed (it didn't exercise the rendered
   pod). The integration suite caught it. The merge-results pipeline
   (the auto-merge gate) didn't run the integration suite. The break
   shipped.

We investigated four alternatives, each with a verified spike under
`/tmp/`:

| Option | Verified | Verdict |
|--------|----------|---------|
| `abi_stable` prefix-type with `last_prefix_field` + `missing_field` | `/tmp/abi-spike` | Rejected. The model only supports plugin-grows-faster-than-host (newer plugin loaded by older hub). The reverse direction we need (host adds a method, old plugin lacks it) is rejected at the layout walker before `missing_field` runs. |
| `stabby` `#[stabby::stabby] trait` vtable | `/tmp/stabby-trait-spike` | Rejected. Same fundamental issue as `abi_stable`: vtable layout must match exactly. Adding a method to the trait fails the report check. |
| `stabby` `#[stabby::export]` per-function exports | `/tmp/stabby-spike` | Works mechanically. Rejected on operational grounds: (a) bus factor 1, (b) hard-coded `_stabbied_v3` symbol suffix that is bumped without a documented migration story, (c) zero verified production users for dynamic plugin VTables, (d) **stabby's own authors at ZettaScale don't use stabby for Zenoh's plugin VTables** — they use option D below. |
| Hand-rolled `#[repr(C)]` vtable + `version: u32` | `/tmp/handrolled-spike` | Accepted. Used in production by tree-sitter, Eclipse Zenoh, memflow, plus decades of nginx / Apache / BIND / PostgreSQL FDW precedent. No third-party runtime dependency on the critical path. Bidirectional compat verified across four spike scenarios. |

## Decision

The plugin-api crate exposes a `#[repr(C)] PluginVTable` struct with
a leading `api_version: u32` field. Plugins emit a static instance via
the `define_plugin!` macro and export a single `get_plugin_vtable`
symbol. The hub uses `dlopen` + `dlsym` to reach that symbol, reads
the version, and dispatches through the vtable's function pointers.
New methods are appended to `PluginVTable` (never reordered, never
removed), `PLUGIN_API_VERSION` increments by 1 per appended field, and
the hub version-gates access to fields beyond the v1 baseline.

`abi_stable`'s data types (`RString`, `RVec`, `RHashMap`, etc.)
continue to be used in vtable signatures because their `#[repr(C)]`
layout is well-understood and the migration to a hand-rolled
equivalent would balloon the scope. `abi_stable` is pinned to exact
version `=0.11.3` in both plugin-api's and the hub's `Cargo.toml` so
plugins and hub see byte-identical layouts.

## Consequences

**Positive:**

- Adding a method to plugin-api no longer requires re-releasing every
  plugin. Old plugins continue to load on newer hubs and the host
  falls back gracefully for the methods they don't expose.
- No third-party dependency on the critical-path load mechanism. If
  `abi_stable` goes unmaintained we can replace its data types
  incrementally; the vtable mechanism itself doesn't depend on it.
- Pattern is operational-textbook. Every Linux distro has a dozen
  examples in production, and any future contributor recognizes it
  on sight.

**Negative:**

- Plugin authors no longer get a free trait-impl ergonomic. The
  `define_plugin!` macro reproduces it, but the macro is now
  load-bearing code in plugin-api that must be reviewed carefully on
  every change.
- We hand-rolled the panic-safety wrapper instead of inheriting it
  from `abi_stable`'s vtable machinery. Code review must verify the
  `catch_unwind` wrapper around `process` is intact on every macro
  change.
- Layout discipline (append-only, never reorder, never remove) is now
  enforced by code review + the matrix test rather than by a
  third-party type-system. Both must remain healthy. `tests/abi_matrix.rs`
  is the regression net.

**Migration cost (one time):**

- plugin-api crate rewrite (this change).
- 4 in-tree examples: zero source change (the macro absorbed it).
- 6 external plugins: each needs a re-release built against
  plugin-api 0.5.0+. Drop their direct `abi_stable` dependency at the
  same time.
- HAPTIC chart's `versions-spoa.env`: bump every plugin pin in
  lockstep with the plugin-api 0.5.0 release.

**Going forward:** every appendable plugin-api change is a routine
minor bump. No more chain-rebuild events.

## Rules for future contributors

See `crates/plugin-api/CLAUDE.md`, `crates/hub/CLAUDE.md`, and
`docs/abi-evolution.md` for the operational reference.

The five load-bearing invariants:

1. `api_version` MUST be the first field of `PluginVTable`.
2. Existing fields MUST NOT be reordered or removed.
3. New fields MUST be appended after the existing ones.
4. `PLUGIN_API_VERSION` MUST increment by 1 each time a vtable field
   is appended.
5. The hub MUST read `api_version` before accessing any field beyond
   the v1 baseline.

The matrix test enforces 1–4 by loading every published plugin
version against the current hub. Invariant 5 is enforced by code
review against `crates/hub/src/plugin.rs::Plugin`.

## Related

- The four spike directories under `/tmp/` mentioned above are kept
  for one release cycle as reproducible evidence; after that they
  age out.
- The bug that drove this decision is HAPTIC MR !890, reverted in
  MR !892. The post-mortem is the comments on those MRs.
