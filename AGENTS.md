# JobKit — agent notes

- The checkpoint contract is load-bearing, not a style preference: pause blocks inside `jk_checkpoint`, cancellation is *acknowledged* there, and once acknowledged the job ends as cancelled whatever the step returns. Do not add a path that pauses or cancels a step from outside — there is no safe version of that against arbitrary host code, and the docs promise we don't fake one.
- Percent monotonicity rule: reported percent never decreases while the total is unchanged; a total change resets the baseline (growth may legitimately drop it). The rule lives in `ProgressTracker::percent`/`set_total` — a "fix" that makes percent follow raw done/total exactly reintroduces the flickering-progress bug this exists to kill.
- The active clock is the ETA's integrity: elapsed, rate and ETA are computed only from active time (paused intervals excluded). Timestamps enter the tracker exclusively via `JobInner::active_now` — never feed `platform::now_ms()` (wall segments) or `Instant` readings into the tracker directly.
- Progress samples are recorded on progress (done changed), by design: zero-progress checkpoints must not decay the EWMA. Stalls show through the overall-average half of the blend. Don't "improve" the rate by sampling on every checkpoint.
- Callbacks run WITHOUT the job lock held (they call back into report/checkpoint/fail). Acquiring `inner` around the callback invocation in `run_steps` deadlocks the job instantly. The callback + user_data are copied out under the lock; the call happens after it drops.
- `jk_close` cancels and JOINS. That guarantee is the orphan-thread story of this DLL; a close that detaches "for responsiveness" silently breaks it.
- The JSON engine (src/json.rs) is the family design — duplicate keys in one object are a PARSE ERROR by design; do not "fix" that into last-wins.
- Zero dependencies; the only platform surface is the monotonic clock in src/platform.rs (std::time, no OS APIs, no #[link] blocks needed anywhere in this crate).
- Gate: WinDevKit tools/gate.sh (fmt + clippy both targets + full tests). Check/clippy do not link — CI's real Windows build is the link gate.
