# Research: Hub Control Args for Per-Request Plugin Skip

## R1: Control arg namespace convention

**Decision**: Use `hub.` prefix for hub control args. All `hub.*` args are stripped before plugins see the message.

**Rationale**: Dots are explicitly allowed in SPOE arg names (`a-z`, `A-Z`, `0-9`, `.`, `_` per the SPOE spec). The `hub.` prefix mirrors the existing variable namespacing convention (e.g., `noop.result`) and looks intentional rather than hacky (compared to `_skip_plugins` underscore prefix). It creates a clean, extensible namespace for future hub control args.

**Alternatives considered**:

- `_skip_plugins` underscore prefix: Looks like a private/internal arg. No clear extensibility path.
- Hub config-based skip rules: Requires a mini-DSL in TOML. HAProxy already has superior ACL capabilities.
- Plugin trait method `should_process()`: FFI overhead per plugin per request. Wrong layer — the caller (HAProxy) knows the skip decision, not the callee.

## R2: Transitive dependency cascade

**Decision**: When a plugin is skipped, all its transitive dependents are automatically skipped. Cascade is computed inline during the group walk using the topological ordering already established by the execution plan.

**Rationale**: The execution plan's groups are topologically sorted. By processing groups in order and expanding the skip set after each group, upstream skip decisions are finalized before downstream groups are evaluated. No separate graph traversal is needed.

**Alternatives considered**:

- No cascade (run dependents without upstream data): Fragile — dependents would need to handle missing upstream output gracefully. Error-prone for plugin authors.
- Pre-compute full transitive closure upfront: Requires building a reverse dependency graph. More code for the same result, since the group walk achieves the same effect incrementally.

## R3: Storing depends_on in registry

**Decision**: Store the `depends_on` map as a field in `PluginRegistry` (it's already computed during `build()` but currently discarded). Add `all_dependencies()` and `dependencies_of()` accessors.

**Rationale**: The cascade logic in `dispatch_message()` needs to know which plugins depend on a skipped plugin. The dependency map is already computed during registry build — storing it avoids recomputation and keeps the dispatch path allocation-free.

**Alternatives considered**:

- Recompute from execution plan groups: The plan doesn't store dependency edges, only grouped names. Would need reverse-engineering from the topological order, which is lossy.
- Store reverse dependency map (dependents-of): More direct for cascade, but the forward map is what's already available. Building the reverse map adds code for marginal benefit — iterating the forward map is sufficient given the small number of plugins.
