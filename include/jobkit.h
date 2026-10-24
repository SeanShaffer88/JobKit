/* JobKit.dll — a background-work engine: progress %,
 * ETA, pause/resume and cooperative cancellation behind
 * one small C ABI.
 *
 * The problem this solves (WinDevKit PAIN_POINTS #17):
 * the house standard demands Progress % and an ETA in
 * status reporting, but every app implemented them per
 * job, per app. JobKit is the one engine:
 *
 *   JkJob *job;
 *   jk_create("{\"name\":\"export\"}", &job);
 *   jk_add_step(job, "render", 3, render_step, ctx);
 *   jk_add_step(job, "encode", 1, encode_step, ctx);
 *   jk_start(job);
 *   while (jk_wait(job, 250) == JK_ERR_TIMEOUT) {
 *       char *s; jk_status(job, &s);
 *       printf("%s\n", s); jk_free_string(s);
 *   }
 *   jk_close(job);
 *
 * CONFIG (one JSON object):
 *
 *   {"name": "export", "total_units": 400}
 *
 *   name         (string, required, non-empty)
 *   total_units  (uint, optional) the job's total work
 *                in job units. When absent, the total is
 *                the SUM OF STEP WEIGHTS. When both exist
 *                the explicit total wins and step weights
 *                should sum to it. jk_set_total replaces
 *                it at runtime (totals learned late).
 *
 * Unknown keys are an error (JK_ERR_PARSE), never ignored;
 * duplicate keys in one object are a parse error too.
 *
 * STEPS AND UNITS. Steps run in the order added, on ONE
 * worker thread the job owns. Each step owns a disjoint
 * slice of the job's units equal to its weight. Inside a
 * step, report progress in STEP-LOCAL units:
 *
 *   jk_report(job, n)        n units of THIS step are
 *                            complete (cumulative,
 *                            0..weight, clamped, only
 *                            moves forward)
 *   jk_report_delta(job, d)  d MORE units are complete
 *
 * The engine maps step-local reports to global progress:
 * global done = weights of finished steps + in-step.
 * A step that returns JK_OK is credited its FULL weight.
 *
 * THE CHECKPOINT CONTRACT (load-bearing). A step MUST
 * call jk_checkpoint(job) regularly — between chunks of
 * work, never in a hot inner loop:
 *
 *   - It BLOCKS while the job is paused.
 *   - It returns JK_OK to continue, or JK_ERR_CANCELLED
 *     once cancellation has been requested. The step
 *     must then return that code (or any nonzero code)
 *     promptly; once cancellation has been acknowledged
 *     at a checkpoint the job ends as cancelled whatever
 *     the step returns afterwards.
 *   - A step that never checkpoints can be neither
 *     paused nor cancelled mid-step, and jk_close will
 *     block in its join until the step returns. Pause
 *     and cancellation are cooperative BY DESIGN —
 *     there is no safe way to kill a thread running
 *     arbitrary host code, and this header will not
 *     pretend otherwise.
 *
 * To fail a job with a message, call
 * jk_fail(job, "disk full") and then return any nonzero
 * code; the message (first one recorded) surfaces in the
 * status "error" field. Without jk_fail, the error is
 * "step \"<name>\" failed with code <rc>".
 *
 * STATES (status "state" is exactly one of these):
 *
 *   created     jk_create done; steps may still be added
 *   running     worker is executing steps
 *   paused      worker is blocked in a checkpoint
 *   cancelling  jk_cancel seen; worker has not yet
 *               acknowledged (next checkpoint / step end)
 *   cancelled   terminal: cancellation acknowledged
 *   completed   terminal: every step returned JK_OK
 *   failed      terminal: a step returned nonzero
 *
 * Pause and cancel requested in `created` take effect at
 * jk_start: a paused job blocks before its first step; a
 * cancelled job ends as cancelled without running any
 * step. jk_pause / jk_resume / jk_cancel on a terminal
 * job are JK_OK no-ops.
 *
 * STATUS (one JSON object):
 *
 *   {"name":"export","state":"running","step_index":0,
 *    "step_count":2,"step_name":"render","units_done":150,
 *    "total_units":400,"percent":37.5,"elapsed_ms":4200,
 *    "eta_ms":7100,"rate_per_sec":35.7,"error":null}
 *
 *   percent     done/total*100, clamped 0..100; exactly
 *               -1 while the total is unknown. Reported
 *               percent never decreases while the total
 *               is unchanged; if the total GROWS it may
 *               legitimately drop (more work was
 *               discovered); if the total shrinks below
 *               done, it clamps at 100.
 *   elapsed_ms  ACTIVE time only: paused intervals are
 *               excluded by construction.
 *   rate_per_sec  the mean of an EWMA over recent
 *               progress samples (alpha 0.3) and the
 *               overall average rate, whichever exist;
 *               0 when no rate can be computed.
 *   eta_ms      remaining / rate, or null when the total
 *               is unknown, nothing is done yet, or no
 *               positive rate exists. 0 once complete.
 *               An estimate, not a promise: early ETAs
 *               swing as the rate settles.
 *   step_name   the current (or last) step's name; null
 *               when the job has no steps.
 *   error       null, or the failure message when failed.
 *
 * jk_wait(job, ms) returns JK_OK once the job is
 * terminal, or JK_ERR_TIMEOUT when the wait elapsed (the
 * job is untouched). JK_WAIT_INFINITE (0xFFFFFFFF) waits
 * forever; 0 polls once.
 *
 * jk_close(job) cancels a still-running job and JOINS
 * its worker before returning: a handle never leaks a
 * thread. NULL is a no-op. Anything through the handle
 * after jk_close is use-after-close.
 *
 * THREADING. The handle is thread-safe; every call is
 * serialized inside. Step callbacks run on the worker
 * thread, never under the job lock, so they may call
 * jk_report / jk_checkpoint / jk_fail through the job
 * pointer they are handed. The handle must outlive its
 * callbacks: jk_close is the only close, and it joins.
 *
 * All strings are UTF-8, NUL-terminated. char* results
 * are caller-owned: free with jk_free_string(). Errors
 * set a thread-local jk_last_error() (message + code).
 */
#ifndef JOBKIT_H
#define JOBKIT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(JK_BUILD)
#define JK_API __declspec(dllexport)
#elif defined(_WIN32)
#define JK_API __declspec(dllimport)
#else
#define JK_API
#endif

enum {
    JK_OK = 0,
    JK_ERR_INVALID_ARG = 1,  /* null pointers, missing/empty name, wrong value types */
    JK_ERR_NOT_SUPPORTED = 2,
    JK_ERR_NOT_FOUND = 3,
    JK_ERR_ACCESS_DENIED = 4,
    JK_ERR_PLATFORM = 5,     /* an OS/thread call failed; see jk_last_error() */
    JK_ERR_INTERNAL = 6,
    JK_ERR_PARSE = 7,        /* config is not valid JSON, or has an unknown key */
    JK_ERR_CANCELLED = 8,    /* jk_checkpoint: cancellation was requested */
    JK_ERR_INVALID_STATE = 9,/* e.g. adding a step after jk_start, starting twice */
    JK_ERR_TIMEOUT = 10      /* jk_wait elapsed; the job is still running */
};

typedef struct JkJob JkJob;

/* A step callback. Runs on the job's worker thread.
 * Return JK_OK on success; JK_ERR_CANCELLED (from a
 * checkpoint) or any other nonzero code to fail the job.
 * `job` is the job handle — call jk_report,
 * jk_checkpoint and jk_fail through it. */
typedef int (*JkStepFn)(void *user_data, JkJob *job);

JK_API const char *jk_version(void);
JK_API const char *jk_last_error(void);
JK_API int jk_last_error_code(void);
JK_API void jk_free_string(char *p);

/* Create a job from a config JSON object (see above).
 * Returns JK_OK and a handle in *out_job, or a JK_ERR_*
 * code and *out_job is NULL. */
JK_API int jk_create(const char *config_json, JkJob **out_job);

/* Add a step (weight in job units; 0 is allowed for a
 * pure-action step that owns no units). Steps must all
 * be added before jk_start — afterwards this returns
 * JK_ERR_INVALID_STATE. The callback must not be NULL;
 * user_data is handed back to it verbatim. */
JK_API int jk_add_step(JkJob *job, const char *name, uint64_t weight,
                       JkStepFn callback, void *user_data);

/* Spawn the worker thread and return immediately.
 * Starting twice returns JK_ERR_INVALID_STATE. */
JK_API int jk_start(JkJob *job);

/* Controls — callable from any thread, JK_OK no-ops on
 * a terminal job. See the state table above for when
 * pause and cancel take effect. */
JK_API int jk_pause(JkJob *job);
JK_API int jk_resume(JkJob *job);
JK_API int jk_cancel(JkJob *job);

/* Set the job's explicit total (totals learned late).
 * 0 clears it, falling back to the config total, then
 * to the sum of step weights. */
JK_API int jk_set_total(JkJob *job, uint64_t total);

/* Status as one JSON object (caller-owned; free with
 * jk_free_string()). Shape and field rules: see above. */
JK_API int jk_status(JkJob *job, char **out_json);

/* Block until the job is terminal (JK_OK) or the wait
 * elapses (JK_ERR_TIMEOUT; the job is untouched). */
#define JK_WAIT_INFINITE 0xFFFFFFFFu
JK_API int jk_wait(JkJob *job, uint32_t timeout_ms);

/* Step-side calls (through the handle a callback was
 * handed; safe from any thread, meaningful on the
 * worker). jk_report is cumulative within the step. */
JK_API int jk_report(JkJob *job, uint64_t units_in_step);
JK_API int jk_report_delta(JkJob *job, uint64_t delta);

/* The cooperative gate: blocks while paused; returns
 * JK_OK to continue or JK_ERR_CANCELLED when
 * cancellation was requested. See the contract above. */
JK_API int jk_checkpoint(JkJob *job);

/* Record a failure message from inside a step, then
 * return a nonzero code from the step. The first
 * message recorded wins. A NULL message records a
 * generic one. */
JK_API int jk_fail(JkJob *job, const char *message);

/* Close and free the handle, cancelling and JOINING a
 * still-running job first. NULL is a no-op. */
JK_API void jk_close(JkJob *job);

#ifdef __cplusplus
}
#endif
#endif /* JOBKIT_H */
