//! JobKit — a background-work engine behind a small C ABI.
//!
//! The problem this solves (WinDevKit PAIN_POINTS #17): the
//! house standard demands Progress % and an ETA in status
//! reporting, but every app implemented progress, ETA, pause
//! and cancellation per job, per app — an installer's jobs
//! here, a download manager there, each with its own rate
//! math and its own idea of what "paused" does to the clock.
//! JobKit is the one engine:
//!
//! - **A progress engine with honest math.** Percent, rate
//!   and ETA are computed from progress samples on an
//!   *active-time* clock — paused intervals are excluded, so
//!   a job paused for lunch does not report a absurd ETA
//!   when it resumes. The rate blends an EWMA over recent
//!   samples with the overall average; ETA is null (not a
//!   made-up number) when it cannot be derived.
//! - **Steps with weights.** A job is a list of steps; each
//!   step owns a disjoint slice of the job's units equal to
//!   its weight. A step reports progress in its own units
//!   and the engine maps it to global progress, so percents
//!   are comparable across jobs.
//! - **Cooperative pause and cancellation.** A step calls
//!   `jk_checkpoint` regularly; the checkpoint blocks while
//!   the job is paused and reports cancellation when it is
//!   requested. A step that never checkpoints cannot be
//!   paused or cancelled mid-step — that is the documented
//!   contract, not a bug to discover in production.
//! - **No thread ever leaks.** `jk_close` on a running job
//!   cancels it and joins the worker before returning.
//!
//! The engine, the step executor and the status shaping
//! live here and in `json`; the monotonic clock lives in
//! `platform`; the C ABI lives in `ffi`. No OS APIs are
//! touched anywhere: threads are `std::thread`.

pub mod ffi;
pub mod json;
pub mod platform;

pub const VERSION: &str = "0.1.0";
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

// Error codes (mirror include/jobkit.h).
pub const JK_OK: i32 = 0;
pub const ERR_INVALID_ARG: i32 = 1;
pub const ERR_NOT_SUPPORTED: i32 = 2;
pub const ERR_NOT_FOUND: i32 = 3;
pub const ERR_ACCESS_DENIED: i32 = 4;
pub const ERR_PLATFORM: i32 = 5;
pub const ERR_INTERNAL: i32 = 6;
pub const ERR_PARSE: i32 = 7;
/// `jk_checkpoint` returns this when cancellation was
/// requested; a step must return it (or any nonzero code)
/// promptly so the job can end as cancelled.
pub const ERR_CANCELLED: i32 = 8;
/// The call is not valid in the job's current state (for
/// example, adding a step after `jk_start`).
pub const ERR_INVALID_STATE: i32 = 9;
/// `jk_wait` returns this when its timeout elapsed and the
/// job is still running. The job is untouched.
pub const ERR_TIMEOUT: i32 = 10;

pub type JkResult<T> = Result<T, (i32, String)>;

/// `jk_wait` timeout_ms value meaning "wait indefinitely".
pub const WAIT_INFINITE: u32 = u32::MAX;

/// Escape a string for inclusion inside JSON double quotes.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Progress engine
// ---------------------------------------------------------------------------

/// EWMA smoothing factor for the recent-rate estimate. 0.3
/// reacts to a genuine rate change within a handful of
/// samples while ignoring one-sample noise.
pub const EWMA_ALPHA: f64 = 0.3;

/// Pure progress mathematics: percent, rate and ETA from
/// `(active_ms, units_done)` samples. Deterministic and
/// fully unit-testable — all timestamps are ACTIVE time
/// (paused intervals already excluded by the caller), so
/// the engine never has to know about pauses.
///
/// Rules (documented in the header; they are the contract):
/// - Percent is `done / total * 100` clamped to 0..=100,
///   or exactly -1 when the total is unknown (0).
/// - Reported percent never decreases while the total is
///   unchanged. If the total grows, percent may legitimately
///   drop (more work was discovered); if the total shrinks
///   below `done`, percent clamps at 100.
/// - Units only move forward: a sample with fewer units
///   than the current position is ignored (reports are
///   cumulative; a backwards report is a host bug, and it
///   must not corrupt the rate).
/// - Rate samples are recorded on PROGRESS (done changed).
///   A stall therefore shows through the overall-average
///   half of the blend rather than by decaying the EWMA
///   with zero-rate samples on every checkpoint.
#[derive(Clone, Debug)]
pub struct ProgressTracker {
    total: u64,
    done: u64,
    /// Active-ms timestamp of the tracker's origin (the
    /// `(origin, 0)` point the overall rate is measured
    /// from).
    origin_ms: u64,
    /// Last PROGRESS sample `(active_ms, done)`.
    last_sample: (u64, u64),
    /// EWMA of instantaneous rates, units per ms.
    ewma_rate: Option<f64>,
    /// Monotonicity baseline: the last percent reported,
    /// and the total it was reported against.
    reported_percent: f64,
    reported_total: u64,
}

impl ProgressTracker {
    pub fn new(total: u64, now_ms: u64) -> Self {
        ProgressTracker {
            total,
            done: 0,
            origin_ms: now_ms,
            last_sample: (now_ms, 0),
            ewma_rate: None,
            reported_percent: 0.0,
            reported_total: total,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn done(&self) -> u64 {
        self.done
    }

    /// Change the total (late-learned totals, `jk_set_total`).
    /// A change invalidates the monotonicity baseline —
    /// percent is recomputed from scratch against the new
    /// total and may drop if the total grew.
    pub fn set_total(&mut self, total: u64) {
        if total != self.total {
            self.total = total;
            self.reported_total = total;
            self.reported_percent = self.raw_percent();
        }
    }

    /// Record progress at an active-time instant. Samples
    /// that do not advance `done` never touch the rate
    /// state (see the rules above).
    pub fn record(&mut self, done: u64, now_ms: u64) {
        if done <= self.done {
            return;
        }
        let (prev_ms, prev_done) = self.last_sample;
        if now_ms > prev_ms {
            let inst = (done - prev_done) as f64 / (now_ms - prev_ms) as f64;
            self.ewma_rate = Some(match self.ewma_rate {
                None => inst,
                Some(prev) => EWMA_ALPHA * inst + (1.0 - EWMA_ALPHA) * prev,
            });
        }
        self.last_sample = (now_ms, done);
        self.done = done;
    }

    fn raw_percent(&self) -> f64 {
        if self.total == 0 {
            return -1.0;
        }
        ((self.done as f64 / self.total as f64) * 100.0).clamp(0.0, 100.0)
    }

    /// Current percent: -1 when indeterminate, otherwise
    /// monotonic non-decreasing while the total is
    /// unchanged.
    pub fn percent(&mut self) -> f64 {
        let raw = self.raw_percent();
        if raw < 0.0 {
            self.reported_percent = 0.0;
            self.reported_total = self.total;
            return raw;
        }
        if self.total == self.reported_total {
            let p = raw.max(self.reported_percent);
            self.reported_percent = p;
            p
        } else {
            self.reported_percent = raw;
            self.reported_total = self.total;
            raw
        }
    }

    /// The EWMA (recent) rate in units per second, if any
    /// progress sample has been recorded.
    pub fn ewma_rate_per_sec(&self) -> Option<f64> {
        self.ewma_rate.map(|r| r * 1000.0)
    }

    /// The overall average rate in units per second since
    /// the tracker's origin, if it can be computed.
    pub fn overall_rate_per_sec(&self, now_ms: u64) -> Option<f64> {
        let elapsed = now_ms.saturating_sub(self.origin_ms);
        if elapsed == 0 || self.done == 0 {
            return None;
        }
        Some(self.done as f64 / elapsed as f64 * 1000.0)
    }

    /// Blended rate in units per second: the mean of the
    /// EWMA and overall rates when both exist, whichever
    /// exists alone, or 0 when neither can be computed.
    pub fn rate_per_sec(&self, now_ms: u64) -> f64 {
        let mut sum = 0.0;
        let mut n = 0.0;
        if let Some(r) = self.ewma_rate_per_sec() {
            sum += r;
            n += 1.0;
        }
        if let Some(r) = self.overall_rate_per_sec(now_ms) {
            sum += r;
            n += 1.0;
        }
        if n == 0.0 {
            0.0
        } else {
            sum / n
        }
    }

    /// Estimated milliseconds to completion. `None` when
    /// the total is unknown, nothing has been done yet, or
    /// no positive rate exists — the honest answer is
    /// "unknown", not a invented number. `Some(0)` once
    /// the work is done.
    pub fn eta_ms(&self, now_ms: u64) -> Option<u64> {
        if self.total == 0 || self.done == 0 {
            return None;
        }
        if self.done >= self.total {
            return Some(0);
        }
        let rate = self.rate_per_sec(now_ms) / 1000.0; // per ms
        if rate <= 0.0 {
            return None;
        }
        Some(((self.total - self.done) as f64 / rate).round() as u64)
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// A fully-validated job configuration (see `parse_config`).
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub name: String,
    /// Explicit total in job units. When absent, the job's
    /// total is the sum of its step weights (learned as
    /// steps are added); when both exist, the explicit
    /// total wins and step weights should sum to it.
    pub total_units: Option<u64>,
}

/// Parse and validate a `jk_create` config JSON object:
///
///   {"name": "nightly-build", "total_units": 1200}
///
/// `name` is required (a string, non-empty). `total_units`
/// is optional (a non-negative integer). Error discipline
/// follows the family convention: malformed JSON and
/// unknown keys are `ERR_PARSE`; missing/empty/wrong-typed
/// values are `ERR_INVALID_ARG`.
pub fn parse_config(text: &str) -> JkResult<Config> {
    let value =
        json::parse(text).map_err(|e| (ERR_PARSE, format!("config is not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or((ERR_PARSE, "config must be a JSON object".to_string()))?;
    for (key, _) in obj {
        if !matches!(key.as_str(), "name" | "total_units") {
            return Err((ERR_PARSE, format!("unknown config key \"{key}\"")));
        }
    }
    let name = match value.get("name") {
        None => {
            return Err((
                ERR_INVALID_ARG,
                "config is missing required key \"name\"".to_string(),
            ))
        }
        Some(json::Value::Str(s)) if !s.is_empty() => s.clone(),
        Some(json::Value::Str(_)) => {
            return Err((ERR_INVALID_ARG, "\"name\" must not be empty".to_string()))
        }
        Some(other) => {
            return Err((
                ERR_INVALID_ARG,
                format!("\"name\" must be a string, got {}", other.type_name()),
            ))
        }
    };
    let total_units = match value.get("total_units") {
        None => None,
        Some(v) => match v.as_i64() {
            Some(n) if n >= 0 => Some(n as u64),
            _ => {
                return Err((
                    ERR_INVALID_ARG,
                    format!(
                        "\"total_units\" must be a non-negative integer, got {}",
                        v.type_name()
                    ),
                ))
            }
        },
    };
    Ok(Config { name, total_units })
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, Weak};

/// A step callback, exactly as declared in the header:
/// returns JK_OK (0) on success, or any nonzero code to
/// fail the job (JK_ERR_CANCELLED from a checkpoint means
/// the job ends as cancelled). Called on the job's worker
/// thread, never under the job lock, so it may freely call
/// jk_report / jk_checkpoint / jk_fail through the handle.
pub type StepFn = extern "C" fn(user_data: *mut c_void, job: *mut Job) -> i32;

#[derive(Clone, Debug)]
struct StepDef {
    name: String,
    weight: u64,
    callback: StepFn,
    /// Stored as a word so the handle stays Send + Sync;
    /// cast back to a pointer only at call time.
    user_data: usize,
}

/// Job lifecycle, exactly the states named in the status
/// JSON and the header's state table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Created,
    Running,
    Paused,
    Cancelling,
    Cancelled,
    Completed,
    Failed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Created => "created",
            State::Running => "running",
            State::Paused => "paused",
            State::Cancelling => "cancelling",
            State::Cancelled => "cancelled",
            State::Completed => "completed",
            State::Failed => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, State::Cancelled | State::Completed | State::Failed)
    }
}

struct JobInner {
    name: String,
    state: State,
    steps: Vec<StepDef>,
    step_index: usize,
    /// Sum of the weights of all steps before `step_index`.
    step_base: u64,
    /// Highest in-step units reported for the current step.
    step_reported: u64,
    config_total: Option<u64>,
    explicit_total: Option<u64>,
    tracker: ProgressTracker,
    pause_requested: bool,
    cancel_requested: bool,
    fail_message: Option<String>,
    error: Option<String>,
    error_code: i32,
    /// The active clock: accumulated ms from finished
    /// running segments, plus the platform-clock reading
    /// at which the current running segment began (None
    /// while paused, not started, or terminal). Elapsed
    /// and all tracker timestamps are on this clock, so
    /// paused time is excluded by construction.
    active_accum_ms: u64,
    segment_start: Option<u64>,
}

impl JobInner {
    /// Current reading of the active clock, in ms.
    fn active_now(&self) -> u64 {
        match self.segment_start {
            Some(t0) => self.active_accum_ms + platform::now_ms().saturating_sub(t0),
            None => self.active_accum_ms,
        }
    }

    /// Stop the active clock (pause acknowledgement,
    /// terminal finalize).
    fn freeze_clock(&mut self) {
        self.active_accum_ms = self.active_now();
        self.segment_start = None;
    }

    /// (Re)start the active clock.
    fn unfreeze_clock(&mut self) {
        if self.segment_start.is_none() {
            self.segment_start = Some(platform::now_ms());
        }
    }

    fn weights_sum(&self) -> u64 {
        self.steps.iter().map(|s| s.weight).sum()
    }

    /// The job's effective total: the most explicit total
    /// wins (jk_set_total over the config value), else the
    /// sum of step weights, else 0 (indeterminate).
    fn effective_total(&self) -> u64 {
        self.explicit_total
            .or(self.config_total)
            .unwrap_or_else(|| self.weights_sum())
    }

    fn sync_tracker_total(&mut self) {
        let t = self.effective_total();
        self.tracker.set_total(t);
    }

    /// Global units done right now: full weights of the
    /// steps before the current one, plus the current
    /// step's reported units (clamped to its weight).
    fn global_done(&self) -> u64 {
        let cur = match self.steps.get(self.step_index) {
            Some(s) => self.step_reported.min(s.weight),
            None => 0,
        };
        self.step_base + cur
    }
}

/// The job handle (`JkJob` in the header). All access is
/// serialized on `inner`; the worker thread is owned by
/// the handle and joined by `shutdown` (jk_close), so a
/// handle never leaks a thread.
pub struct Job {
    inner: Mutex<JobInner>,
    changed: Condvar,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    self_weak: std::sync::OnceLock<Weak<Job>>,
}

fn invalid_state(msg: impl Into<String>) -> (i32, String) {
    (ERR_INVALID_STATE, msg.into())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock means a panic happened while holding
    // it; the job state is still structurally valid, so
    // recover the guard rather than wedging every later
    // call (the panicking step is failed by the worker's
    // catch_unwind path).
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Job {
    /// Create a job in an Arc (the worker thread needs a
    /// shared reference back to its own job).
    pub fn create(config: Config) -> Arc<Job> {
        let job = Arc::new(Job {
            inner: Mutex::new(JobInner {
                name: config.name.clone(),
                state: State::Created,
                steps: Vec::new(),
                step_index: 0,
                step_base: 0,
                step_reported: 0,
                config_total: config.total_units,
                explicit_total: None,
                tracker: ProgressTracker::new(config.total_units.unwrap_or(0), 0),
                pause_requested: false,
                cancel_requested: false,
                fail_message: None,
                error: None,
                error_code: JK_OK,
                active_accum_ms: 0,
                segment_start: None,
            }),
            changed: Condvar::new(),
            worker: Mutex::new(None),
            self_weak: std::sync::OnceLock::new(),
        });
        let _ = job.self_weak.set(Arc::downgrade(&job));
        job
    }

    /// Add a step. Steps run in the order added and must
    /// all be added before `start`.
    pub fn add_step(
        &self,
        name: &str,
        weight: u64,
        callback: StepFn,
        user_data: *mut c_void,
    ) -> JkResult<()> {
        if name.is_empty() {
            return Err((ERR_INVALID_ARG, "step name must not be empty".to_string()));
        }
        let mut g = lock(&self.inner);
        if g.state != State::Created {
            return Err(invalid_state(
                "steps cannot be added after jk_start".to_string(),
            ));
        }
        g.steps.push(StepDef {
            name: name.to_string(),
            weight,
            callback,
            user_data: user_data as usize,
        });
        g.sync_tracker_total();
        Ok(())
    }

    /// Spawn the worker thread and return immediately.
    pub fn start(&self) -> JkResult<()> {
        let weak = self.self_weak.get().ok_or((
            ERR_INTERNAL,
            "job was not created via Job::create".to_string(),
        ))?;
        let arc = weak
            .upgrade()
            .ok_or((ERR_INTERNAL, "job is being destroyed".to_string()))?;
        {
            let mut g = lock(&self.inner);
            if g.state != State::Created {
                return Err(invalid_state("job has already been started".to_string()));
            }
            g.state = State::Running;
            g.unfreeze_clock();
        }
        let handle = std::thread::spawn(move || run_worker(arc));
        *lock(&self.worker) = Some(handle);
        Ok(())
    }

    /// Ask the job to pause. Takes effect when the worker
    /// next reaches a checkpoint (or before the first step
    /// when requested before start). A no-op on a terminal
    /// job.
    pub fn pause(&self) {
        let mut g = lock(&self.inner);
        if !g.state.is_terminal() {
            g.pause_requested = true;
        }
    }

    /// Release a paused (or pause-pending) job.
    pub fn resume(&self) {
        {
            let mut g = lock(&self.inner);
            g.pause_requested = false;
        }
        self.changed.notify_all();
    }

    /// Ask the job to cancel. The state becomes
    /// `cancelling` (when running) and then `cancelled`
    /// when the worker acknowledges at a checkpoint or at
    /// a step boundary. Requested before start, it takes
    /// effect at start: no step ever runs. A no-op on a
    /// terminal job.
    pub fn cancel(&self) {
        {
            let mut g = lock(&self.inner);
            if g.state.is_terminal() {
                return;
            }
            g.cancel_requested = true;
            if matches!(g.state, State::Running | State::Paused) {
                g.state = State::Cancelling;
            }
        }
        self.changed.notify_all();
    }

    /// Report units completed WITHIN the current step
    /// (0..=weight). Reports are cumulative and only move
    /// forward; the engine maps them to global progress.
    pub fn report(&self, units_in_step: u64) {
        let mut g = lock(&self.inner);
        if units_in_step > g.step_reported {
            g.step_reported = units_in_step;
            let done = g.global_done();
            let now = g.active_now();
            g.tracker.record(done, now);
        }
    }

    /// Report additional units completed within the
    /// current step (saturating at the step's weight).
    pub fn report_delta(&self, delta: u64) {
        let cur = lock(&self.inner).step_reported;
        self.report(cur.saturating_add(delta));
    }

    /// Record a failure message from inside a step. The
    /// step should then return a nonzero code; the FIRST
    /// message recorded wins.
    pub fn fail(&self, message: &str) {
        let mut g = lock(&self.inner);
        if g.fail_message.is_none() {
            g.fail_message = Some(message.to_string());
        }
    }

    /// Set (or, with 0, clear) the job's explicit total —
    /// for totals learned after creation. Clearing falls
    /// back to the config total, then to the sum of step
    /// weights.
    pub fn set_total(&self, total: u64) {
        let mut g = lock(&self.inner);
        g.explicit_total = if total == 0 { None } else { Some(total) };
        g.sync_tracker_total();
    }

    /// The cooperative-pause / cancellation gate. Blocks
    /// while the job is paused; returns `JK_OK` to
    /// continue or `ERR_CANCELLED` once cancellation has
    /// been requested (the call itself is the worker's
    /// acknowledgement). Also records a progress sample.
    pub fn checkpoint(&self) -> i32 {
        let mut g = lock(&self.inner);
        let done = g.global_done();
        let now = g.active_now();
        g.tracker.record(done, now);
        loop {
            if g.cancel_requested {
                return ERR_CANCELLED;
            }
            if !g.pause_requested {
                return JK_OK;
            }
            // Acknowledge the pause: freeze the clock and
            // publish the paused state, then block until
            // resumed or cancelled.
            if g.state == State::Running || g.state == State::Cancelling {
                g.state = State::Paused;
                g.freeze_clock();
                self.changed.notify_all();
            }
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
            if !g.pause_requested && !g.cancel_requested {
                // Resumed. (If cancellation is pending the
                // loop head returns ERR_CANCELLED instead —
                // and the clock stays frozen, because a
                // cancelled job never resumes ticking.)
                if g.state == State::Paused {
                    g.state = State::Running;
                }
                g.unfreeze_clock();
                self.changed.notify_all();
                return JK_OK;
            }
        }
    }

    /// Snapshot the job's status.
    pub fn status(&self) -> Status {
        let mut g = lock(&self.inner);
        let now = g.active_now();
        let percent = g.tracker.percent();
        let rate = g.tracker.rate_per_sec(now);
        let eta = g.tracker.eta_ms(now);
        Status {
            name: g.name.clone(),
            state: g.state,
            step_index: g.step_index,
            step_count: g.steps.len(),
            step_name: if g.steps.is_empty() {
                None
            } else {
                g.steps
                    .get(g.step_index)
                    .or_else(|| g.steps.last())
                    .map(|s| s.name.clone())
            },
            units_done: g.tracker.done(),
            total_units: g.tracker.total(),
            percent,
            elapsed_ms: now,
            eta_ms: eta,
            rate_per_sec: rate,
            error: g.error.clone(),
        }
    }

    /// Block until the job reaches a terminal state or
    /// the timeout elapses. Returns true when terminal.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        let g = lock(&self.inner);
        if g.state.is_terminal() {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
        let mut g = g;
        loop {
            if g.state.is_terminal() {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return g.state.is_terminal();
            }
            let (guard, _) = self
                .changed
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            g = guard;
        }
    }

    /// jk_close semantics: if the job is still running,
    /// cancel it; then join the worker. Never returns
    /// while a worker thread is still alive. (A step that
    /// never returns from its callback can still block
    /// the join — the cooperative contract, documented in
    /// the header.)
    pub fn shutdown(&self) {
        self.cancel();
        let handle = lock(&self.worker).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

/// A point-in-time job status (the status JSON's shape).
#[derive(Clone, Debug)]
pub struct Status {
    pub name: String,
    pub state: State,
    pub step_index: usize,
    pub step_count: usize,
    pub step_name: Option<String>,
    pub units_done: u64,
    pub total_units: u64,
    pub percent: f64,
    pub elapsed_ms: u64,
    pub eta_ms: Option<u64>,
    pub rate_per_sec: f64,
    pub error: Option<String>,
}

/// Format a rate/percent without trailing zeros: 25.0
/// becomes "25", 0.125 stays "0.125". JSON numbers either
/// way.
fn fmt_num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

impl Status {
    pub fn to_json(&self) -> String {
        let step_name = match &self.step_name {
            Some(n) => format!("\"{}\"", json_escape(n)),
            None => "null".to_string(),
        };
        let eta = match self.eta_ms {
            Some(ms) => ms.to_string(),
            None => "null".to_string(),
        };
        let error = match &self.error {
            Some(e) => format!("\"{}\"", json_escape(e)),
            None => "null".to_string(),
        };
        format!(
            "{{\"name\":\"{}\",\"state\":\"{}\",\"step_index\":{},\"step_count\":{},\"step_name\":{},\"units_done\":{},\"total_units\":{},\"percent\":{},\"elapsed_ms\":{},\"eta_ms\":{},\"rate_per_sec\":{},\"error\":{}}}",
            json_escape(&self.name),
            self.state.as_str(),
            self.step_index,
            self.step_count,
            step_name,
            self.units_done,
            self.total_units,
            fmt_num(self.percent),
            self.elapsed_ms,
            eta,
            fmt_num(self.rate_per_sec),
            error
        )
    }
}

/// Move the job to a terminal state: freeze the clock,
/// record the final sample, store any error, wake all
/// waiters. The caller must NOT hold the lock.
fn finalize(job: &Job, state: State, error: Option<String>, error_code: i32) {
    {
        let mut g = lock(&job.inner);
        let done = g.global_done();
        g.freeze_clock();
        let now = g.active_now();
        g.tracker.record(done, now);
        if state == State::Completed {
            // A completed job has, by definition, done all
            // its units — credit any the steps under-
            // reported so percent lands on exactly 100.
            let total = g.tracker.total();
            if total > 0 {
                g.tracker.record(total, now);
            }
        }
        g.state = state;
        g.error = error;
        g.error_code = error_code;
    }
    job.changed.notify_all();
}

/// The worker thread's body: run the steps in order,
/// honouring pause/cancel at the pre-step gate and at
/// every step boundary.
fn run_worker(job: Arc<Job>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_steps(&job)));
    if result.is_err() {
        // A step panicked across the callback boundary
        // (the FFI catch_unwind should have caught it per
        // step; this is the last-resort net).
        let state = lock(&job.inner).state;
        if !state.is_terminal() {
            finalize(
                &job,
                State::Failed,
                Some("internal panic in worker thread".to_string()),
                ERR_INTERNAL,
            );
        }
    }
}

fn run_steps(job: &Job) {
    // Pre-step gate: pause-before-start blocks here and
    // cancel-before-start ends the job before any step
    // callback runs.
    if job.checkpoint() != JK_OK {
        finalize(job, State::Cancelled, None, ERR_CANCELLED);
        return;
    }
    let step_count = lock(&job.inner).steps.len();
    for i in 0..step_count {
        let (callback, user_data) = {
            let mut g = lock(&job.inner);
            // Boundary state from the previous step.
            g.step_base = g.steps[..i].iter().map(|s| s.weight).sum();
            g.step_reported = 0;
            g.step_index = i;
            let step = &g.steps[i];
            (step.callback, step.user_data)
        };
        job.changed.notify_all();
        let rc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(user_data as *mut c_void, job as *const Job as *mut Job)
        }));
        let rc = match rc {
            Ok(code) => code,
            Err(_) => ERR_INTERNAL,
        };
        // Cancellation acknowledged anywhere — at a
        // checkpoint the step saw, or requested while the
        // step ran — ends the job as cancelled, whatever
        // the step went on to return.
        let cancel_requested = lock(&job.inner).cancel_requested;
        if cancel_requested {
            finalize(job, State::Cancelled, None, ERR_CANCELLED);
            return;
        }
        if rc != JK_OK {
            let (name, fail_message) = {
                let g = lock(&job.inner);
                (g.steps[i].name.clone(), g.fail_message.clone())
            };
            let error = match fail_message {
                Some(m) => m,
                None if rc == ERR_INTERNAL => {
                    format!("step \"{name}\" panicked")
                }
                None => format!("step \"{name}\" failed with code {rc}"),
            };
            finalize(job, State::Failed, Some(error), rc);
            return;
        }
        // Step succeeded: credit its full weight.
        {
            let mut g = lock(&job.inner);
            let full = g.step_base + g.steps[i].weight;
            let now = g.active_now();
            g.tracker.record(full, now);
            g.step_reported = g.steps[i].weight;
        }
    }
    finalize(job, State::Completed, None, JK_OK);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn escapes_json() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    // -- Progress engine (pure; timestamps are active ms) --

    #[test]
    fn engine_constant_rate_eta() {
        let mut tr = ProgressTracker::new(100, 0);
        tr.record(10, 1000); // 10 units in 1000 active ms = 10/s
        assert_eq!(tr.percent(), 10.0);
        let eta = tr.eta_ms(1000).expect("eta must exist");
        // remaining 90 at 10/s = 9000 ms (tolerance for float dust)
        assert!((eta as i64 - 9000).abs() <= 50, "eta was {eta}");
    }

    #[test]
    fn engine_no_samples_no_eta() {
        let mut tr = ProgressTracker::new(100, 0);
        assert_eq!(tr.percent(), 0.0);
        assert_eq!(tr.rate_per_sec(0), 0.0);
        assert_eq!(tr.eta_ms(0), None);
        assert_eq!(tr.eta_ms(60_000), None); // done == 0: still unknown
    }

    #[test]
    fn engine_indeterminate_total() {
        let mut tr = ProgressTracker::new(0, 0);
        assert_eq!(tr.percent(), -1.0);
        tr.record(10, 1000);
        assert_eq!(tr.percent(), -1.0);
        assert_eq!(tr.eta_ms(1000), None);
    }

    #[test]
    fn engine_percent_clamps_at_100() {
        let mut tr = ProgressTracker::new(100, 0);
        tr.record(150, 1000);
        assert_eq!(tr.percent(), 100.0);
        assert_eq!(tr.eta_ms(1000), Some(0));
    }

    #[test]
    fn engine_monotonic_and_total_changes() {
        let mut tr = ProgressTracker::new(100, 0);
        tr.record(50, 1000);
        assert_eq!(tr.percent(), 50.0);
        // A backwards report is ignored: percent must not drop.
        tr.record(40, 2000);
        assert_eq!(tr.done(), 50);
        assert_eq!(tr.percent(), 50.0);
        // Total grows: percent may legitimately drop (documented).
        tr.set_total(200);
        assert_eq!(tr.percent(), 25.0);
        // Total shrinks below done: clamp at 100.
        tr.set_total(40);
        assert_eq!(tr.percent(), 100.0);
    }

    #[test]
    fn engine_ewma_reacts_faster_than_average() {
        let mut tr = ProgressTracker::new(100_000, 0);
        // Phase 1: 10 units/s for 5 s.
        for i in 1..=5u64 {
            tr.record(i * 10, i * 1000);
        }
        // Phase 2: 100 units/s for 0.5 s.
        for i in 1..=5u64 {
            tr.record(50 + i * 10, 5000 + i * 100);
        }
        let ewma = tr.ewma_rate_per_sec().unwrap();
        let overall = tr.overall_rate_per_sec(5500).unwrap();
        assert!(
            ewma > overall,
            "ewma {ewma} should lead overall {overall} after a speed-up"
        );
        assert!(ewma > 30.0, "ewma {ewma} should be well above the old 10/s");
    }

    #[test]
    fn engine_paused_time_is_excluded() {
        // Samples are fed in ACTIVE time: a 5 s wall pause
        // between the samples never appears in the stamps.
        // 20 units over 2000 active ms = 10/s, ETA 8 s for
        // the remaining 80 — NOT the ~2.9/s a wall clock
        // that counted the pause would produce.
        let mut tr = ProgressTracker::new(100, 0);
        tr.record(10, 1000);
        tr.record(20, 2000);
        let rate = tr.rate_per_sec(2000);
        assert!((rate - 10.0).abs() < 0.5, "rate was {rate}");
        let eta = tr.eta_ms(2000).unwrap();
        assert!((eta as i64 - 8000).abs() <= 100, "eta was {eta}");
    }

    #[test]
    fn engine_eta_null_without_positive_rate() {
        // Progress recorded at the origin instant: no time
        // has elapsed, so no rate exists yet.
        let mut tr = ProgressTracker::new(100, 500);
        tr.record(10, 500);
        assert_eq!(tr.eta_ms(500), None);
    }

    // -- Config parsing --

    #[test]
    fn config_parse_rules() {
        let ok = parse_config(r#"{"name":"build","total_units":1200}"#).unwrap();
        assert_eq!(ok.name, "build");
        assert_eq!(ok.total_units, Some(1200));
        let bare = parse_config(r#"{"name":"build"}"#).unwrap();
        assert_eq!(bare.total_units, None);

        assert_eq!(
            parse_config(r#"{"name":"#).unwrap_err().0,
            ERR_PARSE // malformed JSON
        );
        assert_eq!(
            parse_config(r#"{"name":"x","bogus":1}"#).unwrap_err().0,
            ERR_PARSE // unknown key
        );
        assert_eq!(
            parse_config(r#"{"name":"x","name":"y"}"#).unwrap_err().0,
            ERR_PARSE // duplicate key (family rule)
        );
        assert_eq!(
            parse_config(r#"{"name":"x","total_units":-1}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG // negative total
        );
        assert_eq!(
            parse_config(r#"{}"#).unwrap_err().0,
            ERR_INVALID_ARG // missing name
        );
        assert_eq!(
            parse_config(r#"{"name":""}"#).unwrap_err().0,
            ERR_INVALID_ARG // empty name
        );
        assert_eq!(
            parse_config(r#"{"name":3}"#).unwrap_err().0,
            ERR_INVALID_ARG // wrong type
        );
        assert_eq!(
            parse_config(r#"{"name":"x","total_units":"9"}"#)
                .unwrap_err()
                .0,
            ERR_INVALID_ARG
        );
    }

    // -- Executor (live, through real callbacks) --

    struct Ctl {
        entered: AtomicBool,
        stop: AtomicBool,
        ran: AtomicBool,
    }

    impl Ctl {
        fn new() -> Arc<Ctl> {
            Arc::new(Ctl {
                entered: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                ran: AtomicBool::new(false),
            })
        }
        fn ptr(&self) -> *mut c_void {
            self as *const Ctl as *mut c_void
        }
    }

    extern "C" fn step_ok(_ud: *mut c_void, _job: *mut Job) -> i32 {
        JK_OK
    }

    extern "C" fn step_flag(ud: *mut c_void, _job: *mut Job) -> i32 {
        let ctl = unsafe { &*(ud as *const Ctl) };
        ctl.ran.store(true, Ordering::SeqCst);
        JK_OK
    }

    /// Loops on checkpoint until told to stop (returns
    /// JK_OK) or the job is cancelled (returns the
    /// checkpoint's code).
    extern "C" fn step_gate(ud: *mut c_void, job: *mut Job) -> i32 {
        let ctl = unsafe { &*(ud as *const Ctl) };
        ctl.entered.store(true, Ordering::SeqCst);
        let j = unsafe { &*job };
        loop {
            let rc = j.checkpoint();
            if rc != JK_OK {
                return rc;
            }
            if ctl.stop.load(Ordering::SeqCst) {
                ctl.ran.store(true, Ordering::SeqCst);
                return JK_OK;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Reports 40 units, one every ~5 ms, checkpointing
    /// after each (its step weight must be 40).
    extern "C" fn step_progress(_ud: *mut c_void, job: *mut Job) -> i32 {
        let j = unsafe { &*job };
        for i in 1..=40u64 {
            j.report(i);
            let rc = j.checkpoint();
            if rc != JK_OK {
                return rc;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        JK_OK
    }

    extern "C" fn step_fail42(_ud: *mut c_void, _job: *mut Job) -> i32 {
        42
    }

    extern "C" fn step_failmsg(_ud: *mut c_void, job: *mut Job) -> i32 {
        let j = unsafe { &*job };
        j.fail("disk full");
        5
    }

    fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn make_job(name: &str) -> Arc<Job> {
        Job::create(parse_config(&format!("{{\"name\":\"{name}\"}}")).unwrap())
    }

    #[test]
    fn exec_weights_map_to_percent() {
        let job = make_job("weights");
        let ctl = Ctl::new();
        job.add_step("a", 1, step_ok, std::ptr::null_mut()).unwrap();
        job.add_step("b", 1, step_gate, ctl.ptr()).unwrap();
        job.add_step("c", 2, step_ok, std::ptr::null_mut()).unwrap();
        job.start().unwrap();
        wait_for("step b running with 1 unit done", || {
            let s = job.status();
            s.step_index == 1 && s.units_done == 1
        });
        let s = job.status();
        assert_eq!(s.total_units, 4);
        assert_eq!(s.percent, 25.0, "after step 1 of 1,1,2: {}", s.percent);
        assert_eq!(s.step_name.as_deref(), Some("b"));
        ctl.stop.store(true, Ordering::SeqCst);
        assert!(job.wait(WAIT_INFINITE));
        let s = job.status();
        assert_eq!(s.state, State::Completed);
        assert_eq!(s.units_done, 4);
        assert_eq!(s.percent, 100.0);
        job.shutdown();
    }

    #[test]
    fn exec_pause_freezes_progress_and_clock() {
        let job = make_job("pause");
        job.add_step("work", 40, step_progress, std::ptr::null_mut())
            .unwrap();
        job.start().unwrap();
        wait_for("some progress", || job.status().units_done >= 5);
        job.pause();
        wait_for("paused state", || job.status().state == State::Paused);
        let s1 = job.status();
        std::thread::sleep(Duration::from_millis(300));
        let s2 = job.status();
        assert_eq!(s1.units_done, s2.units_done, "units moved while paused");
        assert!(
            s2.elapsed_ms - s1.elapsed_ms < 100,
            "active clock advanced {} ms during a 300 ms pause",
            s2.elapsed_ms - s1.elapsed_ms
        );
        job.resume();
        assert!(job.wait(WAIT_INFINITE));
        assert_eq!(job.status().state, State::Completed);
        job.shutdown();
    }

    #[test]
    fn exec_cancel_stops_job_and_skips_rest() {
        let job = make_job("cancel");
        let gate = Ctl::new();
        let later = Ctl::new();
        job.add_step("gate", 1, step_gate, gate.ptr()).unwrap();
        job.add_step("later", 1, step_flag, later.ptr()).unwrap();
        job.start().unwrap();
        wait_for("gate entered", || gate.entered.load(Ordering::SeqCst));
        job.cancel();
        assert!(job.wait(WAIT_INFINITE));
        let s = job.status();
        assert_eq!(s.state, State::Cancelled);
        assert!(!later.ran.load(Ordering::SeqCst), "later step ran");
        job.shutdown();
    }

    #[test]
    fn exec_step_failure_records_error_and_skips_rest() {
        let job = make_job("fail");
        let later = Ctl::new();
        job.add_step("a", 1, step_ok, std::ptr::null_mut()).unwrap();
        job.add_step("b", 1, step_fail42, std::ptr::null_mut())
            .unwrap();
        job.add_step("c", 1, step_flag, later.ptr()).unwrap();
        job.start().unwrap();
        assert!(job.wait(WAIT_INFINITE));
        let s = job.status();
        assert_eq!(s.state, State::Failed);
        assert!(
            s.error.as_deref().unwrap_or("").contains("42"),
            "error was {:?}",
            s.error
        );
        assert!(!later.ran.load(Ordering::SeqCst));
        job.shutdown();
    }

    #[test]
    fn exec_jk_fail_message_surfaces() {
        let job = make_job("failmsg");
        job.add_step("a", 1, step_failmsg, std::ptr::null_mut())
            .unwrap();
        job.start().unwrap();
        assert!(job.wait(WAIT_INFINITE));
        let s = job.status();
        assert_eq!(s.state, State::Failed);
        assert_eq!(s.error.as_deref(), Some("disk full"));
        job.shutdown();
    }

    #[test]
    fn exec_cancel_before_start() {
        let job = make_job("cancel-first");
        let gate = Ctl::new();
        job.add_step("gate", 1, step_gate, gate.ptr()).unwrap();
        job.cancel();
        job.start().unwrap();
        assert!(job.wait(WAIT_INFINITE));
        assert_eq!(job.status().state, State::Cancelled);
        assert!(
            !gate.entered.load(Ordering::SeqCst),
            "a step ran despite cancel-before-start"
        );
        job.shutdown();
    }

    #[test]
    fn exec_pause_before_start() {
        let job = make_job("pause-first");
        let gate = Ctl::new();
        gate.stop.store(true, Ordering::SeqCst); // runs through once released
        job.add_step("gate", 1, step_gate, gate.ptr()).unwrap();
        job.pause();
        job.start().unwrap();
        wait_for("paused state", || job.status().state == State::Paused);
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !gate.entered.load(Ordering::SeqCst),
            "a step ran despite pause-before-start"
        );
        job.resume();
        assert!(job.wait(WAIT_INFINITE));
        assert_eq!(job.status().state, State::Completed);
        job.shutdown();
    }

    #[test]
    fn exec_wait_timeout_then_completion() {
        let job = make_job("wait");
        let gate = Ctl::new();
        job.add_step("gate", 1, step_gate, gate.ptr()).unwrap();
        job.start().unwrap();
        wait_for("gate entered", || gate.entered.load(Ordering::SeqCst));
        assert!(!job.wait(50), "wait should have timed out");
        assert_eq!(job.status().state, State::Running);
        gate.stop.store(true, Ordering::SeqCst);
        assert!(job.wait(WAIT_INFINITE));
        assert_eq!(job.status().state, State::Completed);
        job.shutdown();
    }

    #[test]
    fn exec_close_while_running_cancels_and_joins() {
        let job = make_job("close");
        let gate = Ctl::new();
        job.add_step("gate", 1, step_gate, gate.ptr()).unwrap();
        job.start().unwrap();
        wait_for("gate entered", || gate.entered.load(Ordering::SeqCst));
        // Must return (cancel + join), not hang.
        job.shutdown();
        assert_eq!(job.status().state, State::Cancelled);
    }

    #[test]
    fn exec_config_total_and_set_total() {
        // Explicit config total wins over the weights sum.
        let job = Job::create(parse_config(r#"{"name":"t","total_units":10}"#).unwrap());
        job.add_step("a", 4, step_ok, std::ptr::null_mut()).unwrap();
        assert_eq!(job.status().total_units, 10);
        job.start().unwrap();
        assert!(job.wait(WAIT_INFINITE));
        let s = job.status();
        assert_eq!(s.state, State::Completed);
        assert_eq!(s.percent, 100.0);
        job.shutdown();

        // jk_set_total replaces it; 0 clears back to weights.
        let job2 = make_job("t2");
        job2.add_step("a", 2, step_ok, std::ptr::null_mut())
            .unwrap();
        job2.set_total(8);
        assert_eq!(job2.status().total_units, 8);
        job2.set_total(0);
        assert_eq!(job2.status().total_units, 2);
        job2.shutdown();
    }

    #[test]
    fn exec_report_delta_and_clamp() {
        // In-step reports clamp at the step's weight.
        let job = make_job("delta");
        let ctl = Ctl::new();
        extern "C" fn step_delta(_ud: *mut c_void, job: *mut Job) -> i32 {
            let j = unsafe { &*job };
            j.report_delta(3);
            j.report_delta(99); // must clamp: weight is 5
            j.checkpoint()
        }
        job.add_step("a", 5, step_delta, ctl.ptr()).unwrap();
        job.add_step("b", 5, step_gate, ctl.ptr()).unwrap();
        job.start().unwrap();
        wait_for("step b with 5 units done", || {
            let s = job.status();
            s.step_index == 1 && s.units_done == 5
        });
        assert_eq!(job.status().percent, 50.0);
        ctl.stop.store(true, Ordering::SeqCst);
        assert!(job.wait(WAIT_INFINITE));
        job.shutdown();
    }
}
