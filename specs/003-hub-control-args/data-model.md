# Data Model: Hub Control Args

## Entities

### Hub Control Arg

A message argument with the `hub.` prefix, consumed by the hub dispatch layer and never passed to plugins.

| Property     | Description                                              |
|--------------|----------------------------------------------------------|
| Prefix       | `hub.` — constant, used for namespace detection          |
| Suffix       | Control arg name after the prefix (e.g., `skip_plugins`) |
| Value        | SpoeValue — type depends on the specific control arg     |

Currently defined control args:

| Name               | Expected Type | Behavior                                       |
|--------------------|---------------|-------------------------------------------------|
| `hub.skip_plugins` | String        | Comma-separated plugin names to skip            |

Unknown `hub.*` args are silently consumed (forward compatibility).

### Skip Set

A per-request set of plugin names to skip, derived from `hub.skip_plugins` and expanded via transitive dependency cascade.

**Lifecycle**:

1. Extracted from `hub.skip_plugins` arg (comma-split, trimmed)
2. Expanded per execution group: if a skipped plugin has dependents, those dependents are added
3. Checked before each plugin execution in `dispatch_message()`
4. Discarded after message dispatch completes (no persistence)

## Modified Entities

### PluginRegistry (existing, modified)

| Field         | Old                                  | New                                          |
|---------------|--------------------------------------|----------------------------------------------|
| `depends_on`  | Computed in `build()` then discarded | Stored as `HashMap<String, Vec<String>>` field |

New accessors: `dependencies_of(name) -> &[String]`, `all_dependencies() -> &HashMap`

### SpoeMessage (existing, modified at dispatch time)

Not structurally changed. The hub creates a cleaned copy with `hub.*` args removed before passing to plugins. The original message from the connection layer is not mutated.

## Data Flow

```text
HAProxy NOTIFY frame
  └── SpoeMessage { args: { src, method, hub.skip_plugins } }
        │
        ├── extract_hub_args()
        │     ├── skip_set = {"rate-limiter"}
        │     └── cleaned_message = SpoeMessage { args: { src, method } }
        │
        ├── for each execution group:
        │     ├── filter: skip plugins in skip_set
        │     ├── expand_skip_cascade: add dependents of skipped plugins
        │     └── execute remaining plugins with cleaned_message
        │
        └── DispatchResult (only active plugins' variables)
```
