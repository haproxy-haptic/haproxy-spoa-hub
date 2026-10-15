# Validator Sidecar Wire Protocol

**Authoritative spec:** [haproxy-template-ic `docs/development/validator-protocol.md`](https://github.com/haproxy-haptic/haptic/blob/main/docs/development/validator-protocol.md)

The wire protocol spoken over the unix socket bound by `--validate-socket <path>` is owned by HAProxy Template Ingress Controller (HAPTIC). Any change to framing, request/response shape, connection-lifecycle semantics, or error-class enumeration is decided in the HAPTIC repo; this hub implementation conforms.

The HAPTIC spec covers:

- Length-prefixed JSON framing.
- Request and response object shapes.
- Three-result semantics (`valid` / `warning` / `error`).
- Persistent keep-alive connections with sequential pipelined request-response and a connection pool on the controller side.
- Idle close, transport-poison close, and shutdown semantics.
- Versioning rules.

The hub-side implementation lives under `crates/hub/src/validate/` and is described in `../spec.md`. For internal hub design choices that are NOT part of the wire protocol — plugin dispatch, panic recovery, the Coraza captured-logger trick, etc. — see `../spec.md` and the per-plugin READMEs.

This file used to carry the full protocol text; that content moved to HAPTIC when the controller-side wiring landed.
