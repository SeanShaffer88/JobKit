//! Platform layer. JobKit needs no OS APIs at all — the
//! executor is `std::thread` + `Mutex`/`Condvar` on every
//! target — so this module holds the one platform-flavoured
//! thing the engine needs: a monotonic clock.
//!
//! Wall clocks can jump (NTP corrections, the user changing
//! the time), and a progress engine fed wall time would see
//! rates go negative or infinite. Every timestamp in JobKit
//! is milliseconds on this clock: `std::time::Instant`
//! relative to a process-wide epoch, which never runs
//! backwards.

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since a process-wide monotonic epoch. The
/// epoch itself is meaningless; only differences are used
/// (elapsed times, sample intervals).
pub fn now_ms() -> u64 {
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_monotonic() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a);
    }
}
