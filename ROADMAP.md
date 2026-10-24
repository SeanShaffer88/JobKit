# JobKit Roadmap

## v0.1.0 (current)
The background-work engine behind a C ABI: weighted steps on a library-owned worker thread, a pure progress engine (percent with monotonicity rules, EWMA + overall-average blended rate, ETA that is null when it cannot be derived), an active-time clock that excludes paused intervals, cooperative pause/cancellation through `jk_checkpoint`, `jk_fail` failure messages, one-JSON status, and a close that cancels and joins — never leaking a thread.

## Next, in order
1. **Parallel step groups**: a group of steps whose weights share one slice of the job and run concurrently (downloads in parallel, renders in parallel), with per-step progress still mapping into the same global units. Needs a careful answer for checkpoint/pause semantics across concurrent steps before it ships.
2. **Progress events**: an optional `on_progress` callback (throttled, fired on percent/ETA changes) and a terminal-state callback, for hosts that poll reluctantly. Polling `jk_status` stays the baseline — events are an optimization, never the only channel.
3. **Sub-steps**: a step may open a nested unit scope so a long step (a 40-minute encode) reports smooth progress without the job definition pretending it is forty steps.
4. **Persistence / resume**: serialize a job definition plus per-step completion to disk so an app restart can resume a long job (the SafeReturn backup case). Only the definition and step granularity — resuming *inside* a step stays the host's problem, and the format must say so.

## Explicitly not planned
- Preemptive pause/kill of step code: impossible to do safely to arbitrary host code; the checkpoint contract is the honest mechanism, and it stays.
- A job queue / scheduler (priorities, dependencies between jobs): that's an app's policy. JobKit runs one job correctly; AppHost is where family-level scheduling would live if it is ever needed.
- Network-distributed jobs: a different product wearing this one's name.
