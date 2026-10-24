# JobKit

## TL;DR

One background-work engine for every app, behind a tiny zero-dependency DLL: define a job as weighted steps, run it on a worker thread the library owns, and get honest Progress %, rate, and ETA — with pause/resume and cooperative cancellation — from one status call. No more per-app progress math, per-app pause semantics, or threads that outlive the handle that was supposed to own them.

```c
#include "jobkit.h"

static int render(void *ctx, JkJob *job) {
    for (int i = 1; i <= 300; i++) {
        /* ... render chunk i ... */
        jk_report(job, i);                       /* units of THIS step */
        int rc = jk_checkpoint(job);             /* blocks while paused */
        if (rc != JK_OK) return rc;              /* cancelled: bail out */
    }
    return JK_OK;
}

int main(void) {
    JkJob *job;
    jk_create("{\"name\":\"export\"}", &job);
    jk_add_step(job, "render", 300, render, NULL);
    jk_add_step(job, "encode", 100, encode, NULL);
    jk_start(job);
    while (jk_wait(job, 250) == JK_ERR_TIMEOUT) {
        char *s; jk_status(job, &s);
        printf("%s\n", s);                       /* {"percent":...,"eta_ms":...} */
        jk_free_string(s);
    }
    jk_close(job);
}
```

## Why this exists

The house standard demands Progress % and an ETA wherever work takes time — and every app was implementing them from scratch: an installer's job queue here, a download manager there, each with its own rate math, its own answer to "what does pause do to the ETA clock," and its own thread lifetime bugs (WinDevKit pain point #17). JobKit makes the job a constant:

- **Steps with weights.** A job is a list of steps; each step owns a disjoint slice of the job's units equal to its weight (render 300, encode 100 → encode starts at 75%). A step reports in its own units and the engine maps to global progress, so percents mean the same thing in every app.
- **Progress %, rate and ETA with honest math.** See the ETA model below — including when the answer is `null` instead of an invented number.
- **Cooperative pause and cancellation.** A step calls `jk_checkpoint` between chunks of work: it blocks while the job is paused and returns `JK_ERR_CANCELLED` when cancellation is requested. Controls (`jk_pause`, `jk_resume`, `jk_cancel`) are callable from any thread.
- **An active-time clock.** Elapsed time — and therefore rate and ETA — excludes paused intervals by construction. A job paused over lunch resumes with its ETA intact, not inflated by an hour of doing nothing.
- **No leaked threads.** `jk_close` on a running job cancels it and joins the worker before returning. The handle owns the thread, completely.

## The ETA model, honestly

Percent is `units_done / total_units × 100`, clamped to 0–100, or exactly −1 while the total is unknown (no total configured and no step weights yet — or a total genuinely not knowable yet; `jk_set_total` exists for totals learned late). Reported percent never decreases while the total is unchanged; if the total *grows*, percent may legitimately drop — more work was discovered — and if it shrinks below done, percent clamps at 100.

The rate blends two estimates: an **EWMA over recent progress samples** (α = 0.3), which reacts when the work speeds up or slows down, and the **overall average** since the job started, which keeps one odd burst from dominating. Progress samples are taken on progress — reports and checkpoints — so a stall shows up through the overall-average half of the blend rather than by decaying the EWMA on every checkpoint. ETA is `remaining / rate`, and it is **null** — not zero, not a guess — when the total is unknown, nothing has completed yet, or no positive rate exists.

An ETA is an estimate, not a promise. Early in a job, with one or two samples, it will swing; it settles as samples accumulate. JobKit's job is to compute it the same honest way everywhere, not to make the future predictable.

## Pause and cancellation are cooperative

There is no safe way to pause or kill a thread running arbitrary host code from outside, and JobKit does not pretend otherwise. The contract is the checkpoint: a step that calls `jk_checkpoint` regularly can be paused and cancelled promptly; a step that never checkpoints runs to its end (or its first checkpoint) before either takes effect, and `jk_close` will block in its join until the step returns. Keep chunks of work between checkpoints small — the checkpoint itself is cheap: a lock, a sample, and usually an immediate return.

Pause and cancel requested before `jk_start` are not lost: a paused job blocks before its first step, and a cancelled job ends as `cancelled` without running any step. Once cancellation has been *acknowledged* at a checkpoint, the job ends as cancelled whatever the step returns afterwards.

## The API in one screen

| Area | Calls |
|---|---|
| Lifecycle | `jk_create` (config JSON), `jk_close` (cancels + joins a running job) |
| Definition | `jk_add_step` (name, weight, callback, user_data — all before `jk_start`) |
| Execution | `jk_start`, `jk_wait` (JK_OK terminal / JK_ERR_TIMEOUT still running) |
| Control | `jk_pause`, `jk_resume`, `jk_cancel`, `jk_set_total` (late totals; 0 clears) |
| Status | `jk_status` → one JSON object: name, state, step_index/count/name, units_done, total_units, percent, elapsed_ms, eta_ms, rate_per_sec, error |
| Step-side | `jk_report` / `jk_report_delta` (step-local units), `jk_checkpoint` (pause/cancel gate), `jk_fail` (record a failure message) |
| Errors | Integer `JK_*` codes + thread-local `jk_last_error()` |

States: `created → running ⇄ paused → cancelling → cancelled`, ending in `completed` or `failed`. A step returning nonzero fails the job (remaining steps are skipped); the status `error` carries the `jk_fail` message when one was recorded, otherwise `step "<name>" failed with code <rc>`.

## Honest caveats

- **One worker, in order.** Steps run sequentially on a single thread. Parallel step groups are roadmap, not v0.1 — model independent work as separate jobs if you need it today.
- **Percent needs a total.** Without one (no `total_units`, no weights), percent is −1 and ETA is null no matter how much progress is reported. That's the truth of the situation, reported as such.
- **In-step reports only move forward.** A lower report than the current position is ignored (reports are cumulative by contract); a step that discovers it over-reported cannot walk progress back — it can only fail, or the job's total can be corrected with `jk_set_total`.
- **jk_close trusts the checkpoint contract.** A step stuck in an uninterruptible call (a hung network read, say) blocks the join. JobKit cannot fix host code that never returns; it can only refuse to leak silently — and it doesn't: the block is in *your* close call, where you can see it.

## Verification status

30 tests: the pure engine (constant-rate ETA, EWMA reacting to a speed-up faster than the overall average, indeterminate totals, percent clamping and monotonicity, total growth/shrink rules, paused time excluded from rate/ETA), config parsing (malformed JSON, unknown and duplicate keys, negative totals, wrong types), and live executor runs through real callbacks — weights mapping to percent (1,1,2 → 25% after step 1), pause freezing both progress *and* the elapsed clock, cancellation skipping later steps, step failure and `jk_fail` messages surfacing in status, cancel- and pause-before-start, wait timeouts, close-while-running joining cleanly — plus FFI end-to-end status JSON field checks. Windows CI runs the full WinDevKit gate — format, clippy, the whole suite, and a real MSVC build and link.

Building: `cargo build --release` → `jobkit.dll` / `libjobkit.so`; `cargo test --locked`.

License: MIT. Part of the WinDevKit family — see `AGENTS.md` for build discipline and `ROADMAP.md` for what's next.
