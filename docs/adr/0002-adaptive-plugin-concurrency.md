# ADR-0002: Adaptive per-plugin concurrency

- Status: proposed
- Date: 2026-07-22
- Decision driver: the per-plugin concurrency gate is a hand-tuned constant
  that either under-utilizes the CPU or over-subscribes it, and there is no
  value an operator can set that is correct across environments. Benchmarking
  the coraza (WAF) plugin on a 4-core sidecar showed it plateauing at
  ~2900 req/s per HAProxy pod while using **~6% of one core** — the throughput
  was bounded by the concurrency semaphore, not the CPU. We need the hub to
  find the right concurrency on its own.

## Context

Each plugin call is admitted through a per-plugin tokio `Semaphore`
(`PluginAdmission`, `registry.rs`). The permit count is fixed at load time by
`resolve_plugin_concurrency` (`config.rs`), which returns the plugin's
`max_concurrency` if set, else `std::thread::available_parallelism()`.

Two failure modes, and they are opposites:

1. **Hardcoded too low.** The HAPTIC chart pins `coraza` and `api-gateway` to
   `max_concurrency = 2` to bound hostile-traffic tail latency. That also caps
   *benign* throughput far below what the CPU can sustain.
2. **Node-cores too high.** `available_parallelism()` reflects the node's CPU
   affinity mask, **not** the container's CFS quota. On a large node a sidecar
   with a fractional `limits.cpu` sees the full node core count and
   over-subscribes; the summed default also drives `resolve_max_blocking_threads`,
   so a hand-set `max_blocking_threads` below that sum fails validation and
   crash-loops the pod.

The deeper problem is that the *optimal* concurrency is not the core count. A
plugin call holds its permit for its whole service time, but only a fraction of
that is CPU: the coraza bench showed ~0.7 ms of permit-held service time against
~0.08 ms of CPU per request — roughly **8×** headroom. By Little's Law the
concurrency that saturates the CPU is `service_time / cpu_time` × cores, which
varies by plugin, by request content (which CRS rules fire), and by node.
No static value — not `2`, not `cores` — can be right for all of them.

## Decision

Add an **opt-in, per-plugin adaptive concurrency controller** that resizes the
plugin's admission semaphore from a latency-feedback signal, bounded by a flat
thread/memory-budget backstop (not CPU-derived). Off by default; enabled with
`[[plugins]] adaptive_concurrency = true`.

### Algorithm: Gradient2 (Netflix)

Per completed call, using the plugin's **service time** (the `process()`
duration, not the permit-wait):

```
gradient  = clamp(0.5, 1.0, tolerance × long_rtt / short_rtt)
new_limit = limit × gradient + queue_headroom
new_limit = limit × (1 − smoothing) + new_limit × smoothing
new_limit = clamp(min_limit, max_limit, new_limit)
```

- `short_rtt` is the current sample; `long_rtt` is an exponentially-weighted
  moving average (the smoothed no-load baseline). Gradient2 uses a smoothed
  baseline rather than Envoy's periodically re-measured `minRTT`, which pins
  concurrency low during each recalculation and admits 503 spikes — unacceptable
  for an inline WAF — and because coraza's per-request cost is heterogeneous, the
  smoothed baseline is the design Gradient2 was built for.
- Grows (via `queue_headroom`) while service time stays near baseline, so it
  fills available CPU; shrinks when the ratio degrades, so a rule-heavy flood
  self-limits — the original purpose of the cap, done adaptively.
- Growth is suppressed while `in_flight < limit / 2` (application-limited), so
  the limit stays low at normal traffic and only climbs under real load.

### Guardrails: a flat backstop, not a CPU-derived cap

The controller operates strictly within `[min_limit, max_limit]`:

- `min_limit` (default 1) — never starve.
- `max_limit` — a **runaway backstop, not the operating point** (the latency
  signal governs that). Deliberately **not** derived from CPU cores. Two reasons,
  confirmed by surveying production limiters and the Kubernetes CPU model (see
  below):
  1. Every shipping adaptive limiter uses a flat, generous constant far above the
     operating point — Netflix Gradient2 = 200, Envoy adaptive concurrency = 1000,
     Vector ARC = 200 — so the feedback loop, not the cap, binds. Tie the ceiling
     to core count and it becomes the operating point: the loop never engages and
     you have silently shipped a *static* limiter (this is why "ceiling = num CPUs"
     is wrong — for partly-CPU-bound work the CPU-saturating concurrency is
     `cores × (1 + wait/compute)`, many times the core count).
  2. Unlike those async systems, the hub realises concurrency as **blocking OS
     threads** (one `spawn_blocking` per in-flight plugin call — plugins are
     synchronous cgo/FFI). So *N* concurrency = *N* thread stacks, and the binding
     constraint is the sidecar's **thread/memory budget, not CPU**. That is why the
     hub's ceiling is in the tens, not Envoy's hundreds.
  The hub default is a flat `DEFAULT_MAX_LIMIT` (64) when no `max_concurrency` is
  set; deployments (the HAPTIC chart) should set an explicit ceiling sized to the
  sidecar's **memory**. `max_blocking_threads` is the **sum of ceilings**, so growth
  never overflows the blocking pool and the chart/hub agree on the sum (fixing the
  crash-mismatch).
- The tokio `Semaphore` is resized in place via `add_permits` (grow, immediate)
  and permit-forgetting on release (shrink, drains as permits return).

### Kubernetes CPU & throttling

The controller's signal quality depends on the container's CPU configuration:

- **Run the sidecar with a CPU *request* but no CPU *limit*** (HAPTIC's default).
  A CPU limit is enforced by CFS bandwidth control: exhausting the ~100 ms quota
  parks every thread until the next period, injecting tens of ms of scheduler
  stall that a latency-feedback controller misreads as request queueing — biasing
  the ceiling low, poisoning the no-load baseline, and inducing limit-cycle
  hunting on the 100 ms quantum. With no limit there is no quota and no throttling,
  so latency is smooth and monotonic — exactly the signal Gradient2 assumes. Under
  node contention the container still drops to its request's fair share, which
  shows up cleanly as higher latency and the controller backs off. So "no CPU
  limit" is load-bearing for correctness, not incidental.
- **Do not read the cgroup quota to size the ceiling.** With no limit there *is* no
  quota (`cpu.max = "max"`); `available_parallelism()`, `automaxprocs`, and the JVM
  all fall back to *node cores* in that case — the worst value. The only CPU signal
  present is the *request*, and even that is the wrong dimension for the ceiling
  (which is thread/memory-bounded, per above). The hub must therefore not call
  `available_parallelism()` for the default — it reports node cores rather than the
  container's budget and can overcount under a quota.
- **Baseline-poisoning guard.** Envoy's documented real-world failure is the
  *opposite* of a binding cap: under sustained degradation the "no-load" RTT
  estimate gets corrupted and the limit drifts *up*. Gradient2's recovery decay
  (`long_rtt *= 0.95` when `long_rtt/short_rtt > 2`) is the guard against this and
  must stay.

### Overload composes with the existing gate

When the adaptive limit is reached, the current `max_queue` / `queue_timeout_ms`
path is unchanged: excess calls queue briefly then return
`PluginStatus::Overloaded`, surfaced to HAProxy as the `<plugin>.__status`
variable for the operator's fail-open/closed ACL. The controller keeps the limit
near capacity so the queue stays short, well inside HAProxy's SPOE
`timeout processing`.

## Alternatives considered

- **Static CPU-derived limit (chart-side).** Even a perfect `ceil(cpu_limit)`
  leaves most CPU idle, because the CPU *fraction* per request is ~10%, and it
  can't react to changing request cost. It also breaks in the common no-limit
  case (no quota to read). Rejected as the operating point.
- **Ceiling = number of CPUs (or `cores × k`).** The natural-looking self-scaling
  ceiling, but wrong: for partly-CPU-bound work the CPU-saturating concurrency is
  `cores × (1 + wait/compute)` (many times the core count), so a core-derived cap
  binds before the latency loop engages and degrades to a static limiter. No
  production limiter ties its max to cores; they use flat constants. Rejected.
- **Envoy adaptive-concurrency (periodic minRTT).** Its `minRTT` recalibration
  pins concurrency to `min_concurrency` and admits documented 503 spikes each
  window — wrong for an inline WAF.
- **Read the cgroup CPU quota to size the ceiling (cgroup-aware default).** Doesn't
  work for HAPTIC's no-CPU-limit sidecar: with no limit there is no quota, and the
  quota is the wrong dimension anyway (the ceiling is thread/memory-bounded). The
  hub still must stop calling `available_parallelism()` for its default — but the
  replacement is a flat backstop, not a quota read.

## Consequences

- Operators size the sidecar's **memory** once (the thread budget); the ceiling,
  the blocking pool, and the overload point follow. No per-plugin concurrency
  tuning, and — critically — the sidecar keeps a CPU request but no CPU limit so
  the latency signal stays clean.
- New per-plugin latency histogram (`spoa_plugin_duration_seconds`) — needed to
  feed the controller and to observe/A-B it; useful on its own.
- **Prototype limitation:** adaptive state lives in `PluginAdmission`, which is
  rebuilt on SIGHUP reload, so the learned limit cold-starts after a reload.
  Acceptable for the opt-in prototype; a reload-surviving name-keyed store (the
  `plugin_metrics::METRIC_CTX_INTERN` pattern) is required before it becomes a
  default.
- Rollout: land behind the flag, A/B against the static gate on the WAF bench
  route, then consider making it the chart default with a memory-sized ceiling.
  A homelab A/B (2026-07) confirmed the mechanism: the shipped fixed
  `maxConcurrency=2` fails **closed** under load bursts (25–44% 503, p99 to
  610 ms, coraza overloading), while adaptive auto-discovered the workload's ~4
  concurrent demand and held 0% 503 / p99 ~45 ms. Caveat from the same run: on
  hardware that never CPU-saturates, adaptive ties a well-chosen fixed cap — its
  distinct back-off value needs a CPU-constrained deployment to show, which is
  also the case where "no CPU limit" keeps the signal trustworthy.
