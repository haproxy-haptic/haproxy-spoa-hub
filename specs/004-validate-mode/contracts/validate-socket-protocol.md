# Validator Sidecar Wire Protocol

**Feature**: [../spec.md](../spec.md)

> **Ownership note (interim).** This protocol document lives in the hub repo for the lifetime of MR !134 because the hub is the first concrete validator implementation and we needed a single place to pin the design while the spec was being written. **The canonical home for this protocol is the HAProxy Template Ingress Controller (HAPTIC) repo**, because the protocol exists to serve HAPTIC's pluggable-validation feature — HAPTIC is the client that drives the contract, and any future validator (a haproxy-cfg validator, a third-party validator implementation, etc.) needs to conform to a HAPTIC-owned definition. When the HAPTIC-side wiring MR lands, this document moves to `docs/development/validator-protocol.md` (or equivalent) in the HAPTIC repo, and the hub's contract here becomes a one-liner pointer ("the hub implements HAPTIC validator protocol vN, see <link>"). Until that move happens, treat this document as load-bearing; after, treat the HAPTIC copy as canonical.

The hub binary, started with `--validate-socket <path>`, opens a unix domain socket at the given path and serves length-prefixed JSON requests. This document specifies the wire protocol.

## Framing

Each frame on the socket is:

```
| 4 bytes (big-endian, unsigned)  | N bytes        |
|         length = N              |  JSON payload  |
```

- Length is an unsigned 32-bit big-endian integer.
- Maximum frame size: configurable on the hub side via `--validate-max-frame-bytes`, default `1048576` (1 MiB). Requests exceeding the limit get an error response (see "Error responses" below) and then the connection is closed.
- The JSON payload MUST be valid UTF-8, no BOM. Decoder errors are returned as a single error diagnostic.

## Connection lifecycle

- Each accepted connection serves exactly one request-response cycle.
- Client writes one request frame.
- Server writes one response frame.
- Server closes the connection.
- Persistent / multiplexed connections are out of scope for this version.

## Request

```json
{
  "protocol_version": 1,
  "files": [
    {
      "path": "config.toml",
      "content": "[hub]\nlisten = \"0.0.0.0:9000\"\n\n[[plugins]]\nname = \"coraza\"\nlibrary = \"libcoraza.so\"\n\n[plugins.params.coraza]\ndirectives = '''\nSecRuleEngine On\nSecRule ARGS \"@rx evil\" \"id:1001,deny\"\n'''\n"
    }
  ]
}
```

Field semantics:

- `protocol_version` (integer, required) — currently `1`. Validator MUST reject any other value.
- `files` (array, required) — non-empty. Each entry has:
  - `path` (string, required) — operator-facing file identifier. Echoed back in diagnostics' `path` fields. The validator does NOT open this path on disk; it processes `content` directly.
  - `content` (string, required) — the file's bytes as a UTF-8 string. For TOML files, this is the raw TOML text.

The validator processes each file independently. For hub-TOML files, the validator: parses TOML → structurally checks `[hub]` and `[[plugins]]` → for each `[plugins.params.<name>]` subtree, looks up the named plugin among loaded plugins and dispatches to its `validate()` → aggregates diagnostics under that file's path.

## Response

```json
{
  "protocol_version": 1,
  "result": "error",
  "warnings": [],
  "errors": [
    {
      "path": "config.toml",
      "line": 6,
      "column": 0,
      "message": "unknown directive \"secrulremovebyid\""
    }
  ]
}
```

Field semantics:

- `protocol_version` (integer, always present) — `1` in this version.
- `result` (string, always present) — `"valid"`, `"warning"`, or `"error"`. Computed as: `error` if `errors` is non-empty; else `warning` if `warnings` is non-empty; else `valid`.
- `warnings` (array, always present, possibly empty) — list of diagnostic objects with `severity = Warning`.
- `errors` (array, always present, possibly empty) — list of diagnostic objects with `severity = Error`.

Each diagnostic object:

- `path` (string, required) — the file the diagnostic refers to, matching one of the request's `files[].path` values, except for protocol-level diagnostics which use `path: ""`.
- `line` (integer, required) — 1-based line number, or `0` for "unknown / file-level".
- `column` (integer, required) — 1-based column number, or `0` for "unknown / file-level".
- `message` (string, required) — human-readable error message. SHOULD be self-explanatory in the `kubectl apply` context (e.g., `"unknown directive 'secrulremovebyid'"` rather than `"validation error #4"`).

## Result aggregation rules

When multiple plugins each produce diagnostics, the hub aggregates them into a single response:

- Diagnostics across all files and all plugins are collected.
- Severity counts determine `result` (any error → "error"; any warning, no errors → "warning"; else "valid").
- Order in the response: file-level (TOML parse, plugin-not-loaded) first, then per-plugin in plugin-load order.
- Within a single plugin's diagnostics, the order is what the plugin produced — plugins SHOULD return diagnostics in source order where they can.

## Error responses (protocol-level)

These responses come from the validator framework itself, not from a plugin's `validate()`:

- **Frame too large.** Single error diagnostic `{path: "", line: 0, column: 0, message: "request frame exceeds maximum size of N bytes"}`. Connection is closed after the response is written.
- **Malformed JSON.** Single error diagnostic `{path: "", line: 0, column: 0, message: "request body is not valid JSON: <reason>"}`.
- **Wrong protocol version.** Single error diagnostic `{path: "", line: 0, column: 0, message: "protocol version N not supported (max: 1)"}`.
- **Missing required field.** Single error diagnostic `{path: "", line: 0, column: 0, message: "missing required field 'files'"}` etc.
- **Empty files array.** Single error diagnostic `{path: "", line: 0, column: 0, message: "'files' array must be non-empty"}`.
- **Per-request timeout exceeded.** Single error diagnostic `{path: "", line: 0, column: 0, message: "validation timed out after 5s"}`. Configurable timeout via `--validate-timeout-ms`, default 5000.
- **Plugin panic.** Single error diagnostic `{path: <plugin's TOML header line>, line: <header line>, column: 0, message: "internal validator error in plugin <name>: <panic message>"}`. The sidecar continues serving subsequent requests.

In all protocol-error cases, `result` is `"error"`, `warnings` is empty, `errors` contains the single diagnostic.

## Versioning

The current protocol is version `1`. Future evolution rules:

- Adding optional fields to request or response: same protocol version.
- Adding new severity levels (e.g., `Info`): new protocol version.
- Changing the meaning of any existing field: new protocol version.
- New protocol versions MUST be backward-compatible at the connection level: a v1-only validator that receives a v2 request returns the protocol-version error and closes cleanly.

## Example: full request/response round-trip

### Request

```
00 00 00 D6  # 4-byte length: 214 bytes of JSON below

{"protocol_version":1,"files":[{"path":"config.toml","content":"[hub]\nlisten = \"0.0.0.0:9000\"\n\n[[plugins]]\nname = \"coraza\"\nlibrary = \"libcoraza.so\"\n\n[plugins.params.coraza]\ndirectives = '''\nSecRulRemoveById 942100\n'''\n"}]}
```

### Response

```
00 00 00 D2  # length: 210 bytes

{"protocol_version":1,"result":"error","warnings":[],"errors":[{"path":"config.toml","line":11,"column":0,"message":"invalid WAF config from string: unknown directive \"secrulremovebyid\""}]}
```

The `line: 11` corresponds to the line within the embedded TOML file at which the bad SecLang directive appears. The validator uses Coraza's captured-logger trick (see FR-015) to extract that line number from the WAF parser's structured logs.
