//! Adaptive per-plugin concurrency (opt-in).
//!
//! A latency-feedback controller (Netflix's Gradient2) that resizes a plugin's
//! admission [`Semaphore`] to the concurrency that keeps the plugin's *service
//! time* near its smoothed baseline — filling available CPU under load and
//! backing off when a heavier request mix inflates latency. It replaces the
//! fixed `max_concurrency` gate only when `adaptive_concurrency = true`.
//!
//! See `docs/adr/0002-adaptive-plugin-concurrency.md` for the rationale.
//!
//! The controller runs entirely on the request-completion path (no background
//! task): each completed call reports its service time via [`AdaptiveLimiter::on_complete`],
//! which updates the limit under a short mutex and resizes the semaphore in place.

use std::cmp::Ordering as CmpOrdering;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Tunables for the Gradient2 controller. Defaults follow Netflix's
/// `Gradient2Limit`; only `min`/`max`/`initial` are environment-specific.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveConfig {
    /// Never drop the limit below this. Keeps at least some capacity live.
    pub min_limit: usize,
    /// The ceiling the controller may never exceed — a runaway backstop, not the
    /// operating point (the latency signal governs that). Because the hub runs
    /// each plugin call on a blocking OS thread, the real bound is the sidecar's
    /// thread/memory budget, not CPU; deployments size it to memory and the
    /// blocking pool is the sum of ceilings. NOT derived from CPU cores.
    pub max_limit: usize,
    /// Starting limit (the semaphore is created with this many permits).
    pub initial: usize,
    /// How much the average RTT may rise before the limit shrinks. `1.5` allows
    /// a 50% increase before backing off. Must be `>= 1.0`.
    pub tolerance: f64,
    /// How fast the limit tracks its computed target (`0.0..=1.0`). Higher
    /// reacts faster (and noisier); `0.2` is a calm default.
    pub smoothing: f64,
    /// EWMA sample count for the no-load baseline. Larger is smoother / slower.
    pub long_window: f64,
}

/// Flat fallback ceiling when a plugin enables adaptive concurrency without an
/// explicit `max_concurrency`. Deliberately **not** derived from CPU cores: the
/// operating point is found by the latency signal, so the ceiling is only a
/// runaway backstop, and the hub realises concurrency as blocking OS threads
/// (one per in-flight plugin call), so the binding constraint is the sidecar's
/// thread/memory budget rather than core count. `available_parallelism()` is
/// doubly wrong here — it reports node cores, not the container's cgroup budget,
/// and can overcount under a CPU quota. Deployments (the HAPTIC chart) should
/// set an explicit `max_concurrency` sized to the sidecar's memory; this
/// constant is a safe default for chart-less/standalone usage.
const DEFAULT_MAX_LIMIT: usize = 64;

/// Per-plugin Gradient2 tunable overrides, sourced from **top-level**
/// `[[plugins]]` fields — NOT `[plugins.params]`. `[plugins.params]` is the
/// plugin's own config table (validated against the plugin's schema), so putting
/// hub-level knobs there makes a strict plugin (e.g. coraza, `additionalProperties:
/// false`) reject its whole config and fail to load. Each field is optional; `None`
/// keeps the Gradient2 default.
#[derive(Debug, Clone, Copy, Default)]
pub struct AdaptiveTunables {
    pub initial: Option<usize>,
    pub min_limit: Option<usize>,
    pub tolerance: Option<f64>,
    pub smoothing: Option<f64>,
    pub long_window: Option<f64>,
}

impl AdaptiveConfig {
    /// Build a config for a plugin: `max_concurrency` (if set) is the ceiling,
    /// otherwise `DEFAULT_MAX_LIMIT` (a flat backstop, not core-derived — see that
    /// constant's docs). Gradient2 tunables come from top-level plugin fields via
    /// [`AdaptiveTunables`]; each unset one uses the Netflix default.
    #[must_use]
    pub fn resolve(max_concurrency: Option<usize>, t: &AdaptiveTunables) -> Self {
        let max_limit = max_concurrency.unwrap_or(DEFAULT_MAX_LIMIT).max(1);
        Self {
            min_limit: t.min_limit.unwrap_or(1).clamp(1, max_limit),
            max_limit,
            // Default start = min(16, ceiling): a proven-safe concurrency, so
            // startup and every config-reload cold-start (which rebuilds the
            // admission state) begin at a level that absorbs a burst instead of
            // fail-closing it before the controller grows — never worse than the
            // old fixed default, and still clamped to a tiny ceiling.
            initial: t.initial.unwrap_or(max_limit.min(16)).clamp(1, max_limit),
            tolerance: t.tolerance.unwrap_or(1.5).max(1.0),
            smoothing: t.smoothing.unwrap_or(0.2).clamp(0.01, 1.0),
            long_window: t.long_window.unwrap_or(600.0).max(1.0),
        }
    }
}

/// Pure Gradient2 limit estimator. Separated from the semaphore actuator so the
/// control law is unit-testable without a tokio runtime.
#[derive(Debug)]
struct Gradient2 {
    /// EWMA of service time (ns) — the smoothed no-load baseline. `0.0` until the
    /// first sample seeds it.
    long_rtt_ns: f64,
    /// Current fractional limit (rounded when applied to the semaphore).
    limit: f64,
    cfg: AdaptiveConfig,
}

impl Gradient2 {
    fn new(cfg: AdaptiveConfig) -> Self {
        Self {
            long_rtt_ns: 0.0,
            #[allow(clippy::cast_precision_loss)]
            limit: cfg.initial as f64,
            cfg,
        }
    }

    /// Fold one completed call's service time into the estimate and return the
    /// new integer limit. `in_flight` is the concurrency observed at completion,
    /// used to suppress growth when the plugin is application-limited (not
    /// actually using the concurrency it already has).
    #[allow(clippy::cast_precision_loss)]
    fn update(&mut self, short_ns: f64, in_flight: usize) -> usize {
        if self.long_rtt_ns <= 0.0 {
            self.long_rtt_ns = short_ns;
        } else {
            // Gradient2 recovery: when the baseline has drifted far above the
            // current sample (load just dropped), decay it toward the truth so
            // the limit can recover instead of staying pinned.
            if self.long_rtt_ns / short_ns > 2.0 {
                self.long_rtt_ns *= 0.95;
            }
            self.long_rtt_ns += (short_ns - self.long_rtt_ns) / self.cfg.long_window;
        }

        let gradient = (self.cfg.tolerance * self.long_rtt_ns / short_ns).clamp(0.5, 1.0);
        // Headroom = sqrt(limit): the term that lets the limit grow while the
        // gradient is ~1, self-scaling so growth slows as the limit rises.
        let headroom = self.limit.max(1.0).sqrt();
        let mut target = self.limit * gradient + headroom;

        // Application-limited: don't grow past what's actually being used.
        if target > self.limit && (in_flight as f64) < self.limit / 2.0 {
            target = self.limit;
        }

        // Smooth the transition (damps oscillation).
        self.limit = self
            .limit
            .mul_add(1.0 - self.cfg.smoothing, target * self.cfg.smoothing);
        self.limit = self
            .limit
            .clamp(self.cfg.min_limit as f64, self.cfg.max_limit as f64);

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.limit.round() as usize
        }
    }
}

/// Actuator: owns the plugin's admission [`Semaphore`] and grows/shrinks its
/// permit count to track the Gradient2 estimate.
///
/// `total` mirrors the semaphore's permit count (available + in-use). Growth is
/// immediate via [`Semaphore::add_permits`]; shrink removes available permits
/// immediately and forgets in-use permits as they are returned through
/// [`AdaptiveLimiter::release`], so the total converges to `target` without
/// stranding capacity that's currently in flight.
#[derive(Debug)]
pub struct AdaptiveLimiter {
    permits: Arc<Semaphore>,
    inner: Mutex<Gradient2>,
    target: AtomicUsize,
    total: AtomicUsize,
}

impl AdaptiveLimiter {
    /// Create a limiter over a freshly-built semaphore. The semaphore MUST have
    /// been created with `cfg.initial` permits.
    #[must_use]
    pub fn new(permits: Arc<Semaphore>, cfg: AdaptiveConfig) -> Self {
        Self {
            permits,
            inner: Mutex::new(Gradient2::new(cfg)),
            target: AtomicUsize::new(cfg.initial),
            total: AtomicUsize::new(cfg.initial),
        }
    }

    /// Concurrency currently in flight (permits handed out but not yet returned).
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.total
            .load(Ordering::Acquire)
            .saturating_sub(self.permits.available_permits())
    }

    /// Record a completed call's service time and resize toward the new estimate.
    pub fn on_complete(&self, service: Duration, in_flight: usize) {
        #[allow(clippy::cast_precision_loss)]
        let short_ns = service.as_nanos() as f64;
        if short_ns <= 0.0 {
            return;
        }
        let new_target = {
            let mut g = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.update(short_ns, in_flight)
        };
        self.resize(new_target);
    }

    /// Move the semaphore's permit total toward `new_target`. Grows immediately;
    /// shrinks the available portion now, leaving the rest to [`Self::release`].
    fn resize(&self, new_target: usize) {
        self.target.store(new_target, Ordering::Release);
        loop {
            let cur = self.total.load(Ordering::Acquire);
            match new_target.cmp(&cur) {
                CmpOrdering::Greater => {
                    if self
                        .total
                        .compare_exchange(cur, new_target, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        self.permits.add_permits(new_target - cur);
                        return;
                    }
                }
                CmpOrdering::Less => {
                    // Forget the available portion, then record EXACTLY what was
                    // removed with an unconditional `fetch_sub` — never a CAS. A CAS
                    // could lose to a concurrent `total` update (another call's
                    // `resize`/`release`) and orphan permits we already forgot from the
                    // semaphore, drifting the real capacity below `total` (a slow leak).
                    // `forget_permits` reclaims only what's currently available; permits
                    // held by in-flight calls drain through `release()` while
                    // `total > target`. `removed <= available <= total`, so no underflow.
                    let removed = self.permits.forget_permits(cur - new_target);
                    self.total.fetch_sub(removed, Ordering::AcqRel);
                    return;
                }
                CmpOrdering::Equal => return,
            }
        }
    }

    /// Return a permit obtained for a completed call. If the semaphore is over
    /// its target (a shrink is pending), the permit is forgotten (shrinking the
    /// total by one) instead of returned; otherwise it is released normally.
    pub fn release(&self, permit: OwnedSemaphorePermit) {
        loop {
            let cur = self.total.load(Ordering::Acquire);
            let tgt = self.target.load(Ordering::Acquire);
            if cur > tgt {
                if self
                    .total
                    .compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    permit.forget();
                    return;
                }
            } else {
                drop(permit);
                return;
            }
        }
    }

    #[cfg(test)]
    fn current_limit(&self) -> usize {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .limit
                .round() as usize
        }
    }

    #[cfg(test)]
    fn total(&self) -> usize {
        self.total.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_ceiling_is_flat_default_not_core_derived() {
        // No explicit max_concurrency -> the flat backstop, independent of how
        // many cores the host reports (the whole point of not using
        // available_parallelism()).
        let c = AdaptiveConfig::resolve(None, &AdaptiveTunables::default());
        assert_eq!(c.max_limit, DEFAULT_MAX_LIMIT);
        // Explicit max_concurrency is the ceiling verbatim.
        assert_eq!(
            AdaptiveConfig::resolve(Some(8), &AdaptiveTunables::default()).max_limit,
            8
        );
        // Tunables come from top-level fields; initial/min are clamped to the ceiling.
        let clamped = AdaptiveConfig::resolve(
            Some(8),
            &AdaptiveTunables {
                initial: Some(1000),
                min_limit: Some(1000),
                ..Default::default()
            },
        );
        assert_eq!(clamped.initial, 8);
        assert_eq!(clamped.min_limit, 8);
    }

    fn cfg(min: usize, max: usize, initial: usize) -> AdaptiveConfig {
        AdaptiveConfig {
            min_limit: min,
            max_limit: max,
            initial,
            tolerance: 1.5,
            smoothing: 0.2,
            long_window: 100.0,
        }
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn grows_when_latency_is_stable_and_load_is_present() {
        let mut g = Gradient2::new(cfg(1, 100, 4));
        // Seed baseline, then feed steady samples with in_flight == limit (fully
        // loaded) so the app-limited guard doesn't suppress growth.
        let start = g.limit;
        for _ in 0..200 {
            let inflight = g.limit.round() as usize;
            g.update(1_000_000.0, inflight); // 1ms, constant
        }
        assert!(
            g.limit > start,
            "limit should climb under steady latency + load: {start} -> {}",
            g.limit
        );
        assert!(g.limit <= 100.0, "must respect the ceiling");
    }

    #[test]
    fn does_not_grow_when_application_limited() {
        let mut g = Gradient2::new(cfg(1, 100, 20));
        let start = g.limit;
        for _ in 0..200 {
            g.update(1_000_000.0, 1); // stable latency but only 1 in flight
        }
        assert!(
            g.limit <= start + 1.0,
            "limit must not balloon when the plugin isn't using its concurrency: {start} -> {}",
            g.limit
        );
    }

    #[test]
    fn shrinks_when_latency_rises() {
        let mut g = Gradient2::new(cfg(1, 100, 50));
        // Establish a low baseline.
        for _ in 0..300 {
            g.update(1_000_000.0, 25);
        }
        let before = g.limit;
        // Latency jumps 5x (overload); limit should fall.
        for _ in 0..50 {
            g.update(5_000_000.0, 50);
        }
        assert!(
            g.limit < before,
            "limit should shrink when latency spikes: {before} -> {}",
            g.limit
        );
        assert!(g.limit >= 1.0, "must respect the floor");
    }

    #[test]
    fn respects_min_and_max_clamps() {
        let mut g = Gradient2::new(cfg(3, 8, 5));
        for _ in 0..500 {
            g.update(1_000_000.0, 8);
        }
        assert!(g.limit <= 8.0);
        for _ in 0..500 {
            g.update(100_000_000.0, 8); // sustained huge latency
        }
        assert!(g.limit >= 3.0, "floor holds even under extreme latency");
    }

    #[tokio::test]
    async fn limiter_grows_the_semaphore() {
        let sem = Arc::new(Semaphore::new(4));
        let lim = AdaptiveLimiter::new(Arc::clone(&sem), cfg(1, 64, 4));
        // Drive stable low latency under full load until it grows.
        for _ in 0..300 {
            let inflight = lim.current_limit();
            lim.on_complete(Duration::from_micros(500), inflight);
        }
        assert!(
            lim.total() > 4,
            "semaphore total should grow: {}",
            lim.total()
        );
        assert_eq!(
            sem.available_permits(),
            lim.total(),
            "no permits held, so available == total after growth"
        );
    }

    #[tokio::test]
    async fn release_forgets_permits_while_over_target() {
        let sem = Arc::new(Semaphore::new(10));
        let lim = AdaptiveLimiter::new(Arc::clone(&sem), cfg(1, 64, 10));
        // Acquire 4 permits (in flight), then force a shrink to 2.
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(Arc::clone(&sem).try_acquire_owned().unwrap());
        }
        lim.resize(2); // available (6) forgotten immediately; total tracks down
        // Releasing held permits while over target must forget, not return them.
        for p in held.drain(..) {
            lim.release(p);
        }
        assert_eq!(lim.total(), 2, "total should converge to the shrink target");
        assert_eq!(
            sem.available_permits(),
            2,
            "available should reflect the shrunk total"
        );
    }

    /// Regression test for the shrink-branch CAS race: `resize` down, `release`,
    /// and `resize` up hammered concurrently must never orphan forgotten permits.
    /// The invariant is `total == live semaphore capacity`; a leak drifts the real
    /// capacity below `total`, which this catches once everything quiesces.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_resize_and_release_never_leak_permits() {
        let sem = Arc::new(Semaphore::new(16));
        let lim = Arc::new(AdaptiveLimiter::new(Arc::clone(&sem), cfg(1, 64, 16)));
        let mut tasks = Vec::new();
        for t in 0..8 {
            let lim = Arc::clone(&lim);
            let sem = Arc::clone(&sem);
            tasks.push(tokio::spawn(async move {
                for i in 0..600 {
                    // Vary the target so shrinks and grows interleave across threads.
                    let target = 1 + ((t * 7 + i) % 40);
                    if let Ok(p) = Arc::clone(&sem).try_acquire_owned() {
                        lim.resize(target);
                        lim.release(p);
                    } else {
                        lim.resize(target);
                    }
                    tokio::task::yield_now().await;
                }
            }));
        }
        for h in tasks {
            h.await.unwrap();
        }
        // Grow back to a fixed ceiling so no shrink is pending, then assert the
        // accounting invariant: nothing is held, so total must equal available.
        lim.resize(64);
        assert_eq!(
            lim.total(),
            sem.available_permits(),
            "total must equal live semaphore capacity — no orphaned/leaked permits (total={}, available={})",
            lim.total(),
            sem.available_permits()
        );
    }
}
