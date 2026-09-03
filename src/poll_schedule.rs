//! Pure scheduling and sizing policy shared by the window, tray-icon and poller
//! code. This module deliberately has no Win32 dependency and no `unsafe` so its
//! invariants can be unit-tested anywhere.
//!
//! Background: an earlier version of the app could spawn an unbounded number of
//! concurrent poll threads (each of which launches `wsl.exe` and opens TLS
//! connections) because a periodic 5-second "did the usage window reset yet?"
//! timer re-armed itself forever and every tick spawned a new thread with no
//! in-flight check. Sustained thread/process/handle churn of that kind is how a
//! user-mode program pushes kernel drivers into `PAGE_FAULT_IN_NONPAGED_AREA`.
//! The types here make that impossible by construction:
//!
//! * [`PollGate`] admits at most one poll at a time.
//! * [`reset_poll_delay_ms`] bounds the fast "reset watch" polling to a short,
//!   backing-off burst instead of an endless 5-second loop.
//! * [`mono_mask_bytes`] / [`pixel_count`] compute buffer sizes handed to GDI so
//!   the kernel never receives an undersized or overflowed buffer.

use std::sync::atomic::{AtomicBool, Ordering};

/// Admission gate that allows at most one poll to be in flight at a time.
///
/// Timer ticks that arrive while a poll is running are simply dropped. User
/// initiated requests can ask to be *coalesced* instead: the running poll will
/// run one more time after it finishes, so the UI stays responsive without
/// ever running two polls concurrently.
pub struct PollGate {
    in_flight: AtomicBool,
    rerun_requested: AtomicBool,
}

/// RAII token proving the holder owns the single poll slot. Dropping it
/// releases the gate.
#[must_use = "dropping the guard immediately releases the gate"]
pub struct PollGuard<'a> {
    gate: &'a PollGate,
}

impl PollGate {
    pub const fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            rerun_requested: AtomicBool::new(false),
        }
    }

    /// Try to take the poll slot.
    ///
    /// Returns `None` if a poll is already running. In that case, when
    /// `coalesce` is true, one follow-up poll is queued and will be reported by
    /// [`PollGuard::take_rerun_request`] to the current holder.
    pub fn try_acquire(&self, coalesce: bool) -> Option<PollGuard<'_>> {
        match self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                // A fresh holder starts with a clean rerun flag.
                self.rerun_requested.store(false, Ordering::Release);
                Some(PollGuard { gate: self })
            }
            Err(_) => {
                if coalesce {
                    self.rerun_requested.store(true, Ordering::Release);
                }
                None
            }
        }
    }

    /// True while a poll is in flight.
    #[allow(dead_code)]
    pub fn is_busy(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }
}

impl Default for PollGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PollGuard<'_> {
    /// Returns true exactly once per coalesced request that arrived while this
    /// guard was held. The holder should run one more poll when it sees `true`.
    pub fn take_rerun_request(&self) -> bool {
        self.gate.rerun_requested.swap(false, Ordering::AcqRel)
    }
}

impl Drop for PollGuard<'_> {
    fn drop(&mut self) {
        self.gate.in_flight.store(false, Ordering::Release);
    }
}

/// Why a poll was requested. Determines whether a request that arrives while a
/// poll is already running is dropped (timer-driven) or coalesced (user-driven).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollReason {
    Startup,
    Interval,
    ResetWatch,
    UserRefresh,
    ModelChanged,
}

impl PollReason {
    /// User-initiated requests should never look "dead" just because a
    /// background poll happens to be running, so they queue one follow-up poll.
    /// Timer-driven requests are dropped: the next tick will try again.
    pub fn coalesces(self) -> bool {
        matches!(self, Self::UserRefresh | Self::ModelChanged)
    }
}

/// Shortest interval ever used for the fast "reset watch" poll.
pub const RESET_POLL_MIN_INTERVAL_MS: u32 = 5_000;

/// Longest interval used for the fast "reset watch" poll before giving up.
pub const RESET_POLL_MAX_INTERVAL_MS: u32 = 160_000;

/// Number of fast polls attempted after a usage window has reset before the
/// app falls back to its regular poll interval.
pub const RESET_POLL_MAX_ATTEMPTS: u32 = 6;

/// Delay before the next fast "has the new usage window started yet?" poll.
///
/// `attempts` counts fast polls already made since the reset was detected.
/// The sequence is 5 s, 10 s, 20 s, 40 s, 80 s, 160 s and then `None`, meaning
/// stop fast-polling and rely on the normal interval timer. Total budget is
/// about five minutes and at most [`RESET_POLL_MAX_ATTEMPTS`] polls, versus the
/// previous behaviour of one poll every 5 seconds indefinitely.
pub fn reset_poll_delay_ms(attempts: u32) -> Option<u32> {
    if attempts >= RESET_POLL_MAX_ATTEMPTS {
        return None;
    }
    let delay = RESET_POLL_MIN_INTERVAL_MS
        .checked_shl(attempts)
        .unwrap_or(RESET_POLL_MAX_INTERVAL_MS);
    Some(delay.clamp(RESET_POLL_MIN_INTERVAL_MS, RESET_POLL_MAX_INTERVAL_MS))
}

/// Size in bytes of the pixel buffer for a 1 bit-per-pixel device-dependent
/// bitmap as required by `CreateBitmap`: every scanline is padded to a WORD
/// (16-bit) boundary. Returns 0 for non-positive dimensions.
///
/// The old formula `(w * h + 7) / 8` only matched this for widths that are a
/// multiple of 16; for any other width GDI would read past the end of the
/// buffer.
pub fn mono_mask_bytes(width: i32, height: i32) -> usize {
    if width <= 0 || height <= 0 {
        return 0;
    }
    let width = width as usize;
    let height = height as usize;
    let stride = width.div_ceil(16) * 2;
    stride * height
}

/// Number of 32-bit pixels in a `width` x `height` top-down DIB, or `None` if
/// either dimension is non-positive or the product overflows. Callers must not
/// touch DIB memory when this returns `None`.
pub fn pixel_count(width: i32, height: i32) -> Option<usize> {
    if width <= 0 || height <= 0 {
        return None;
    }
    (width as usize).checked_mul(height as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    #[test]
    fn gate_allows_one_acquire_at_a_time() {
        let gate = PollGate::new();
        let first = gate.try_acquire(false);
        assert!(first.is_some());
        assert!(gate.is_busy());
        assert!(gate.try_acquire(false).is_none());
        drop(first);
        assert!(!gate.is_busy());
    }

    #[test]
    fn gate_releases_on_drop() {
        let gate = PollGate::new();
        {
            let _guard = gate.try_acquire(false).expect("first acquire");
        }
        assert!(gate.try_acquire(false).is_some());
    }

    #[test]
    fn gate_coalesces_only_when_requested() {
        let gate = PollGate::new();
        let guard = gate.try_acquire(false).expect("first acquire");

        assert!(gate.try_acquire(false).is_none());
        assert!(
            !guard.take_rerun_request(),
            "plain skip must not queue a rerun"
        );

        assert!(gate.try_acquire(true).is_none());
        assert!(gate.try_acquire(true).is_none());
        assert!(
            guard.take_rerun_request(),
            "coalesced request is reported once"
        );
        assert!(!guard.take_rerun_request(), "...and only once");
    }

    #[test]
    fn gate_rerun_flag_is_cleared_for_a_new_holder() {
        let gate = PollGate::new();
        let guard = gate.try_acquire(false).expect("first acquire");
        assert!(gate.try_acquire(true).is_none());
        // Holder exits without draining the flag.
        drop(guard);
        let next = gate.try_acquire(false).expect("second acquire");
        assert!(!next.take_rerun_request());
    }

    #[test]
    fn gate_admits_exactly_one_of_many_racing_threads() {
        const THREADS: usize = 32;
        const ROUNDS: usize = 200;

        let gate = Arc::new(PollGate::new());
        let concurrent = Arc::new(AtomicU32::new(0));
        let high_water = Arc::new(AtomicU32::new(0));
        let admitted = Arc::new(AtomicU32::new(0));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let concurrent = Arc::clone(&concurrent);
                let high_water = Arc::clone(&high_water);
                let admitted = Arc::clone(&admitted);
                std::thread::spawn(move || {
                    for _ in 0..ROUNDS {
                        if let Some(guard) = gate.try_acquire(false) {
                            let now = concurrent.fetch_add(1, Ordering::AcqRel) + 1;
                            high_water.fetch_max(now, Ordering::AcqRel);
                            admitted.fetch_add(1, Ordering::AcqRel);
                            std::thread::yield_now();
                            concurrent.fetch_sub(1, Ordering::AcqRel);
                            drop(guard);
                        }
                        std::thread::yield_now();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread panicked");
        }

        assert_eq!(
            high_water.load(Ordering::Acquire),
            1,
            "two polls were in flight at the same time"
        );
        assert!(admitted.load(Ordering::Acquire) >= 1);
        assert!(!gate.is_busy());
    }

    #[test]
    fn poll_reason_coalescing_matrix() {
        assert!(PollReason::UserRefresh.coalesces());
        assert!(PollReason::ModelChanged.coalesces());
        assert!(!PollReason::Startup.coalesces());
        assert!(!PollReason::Interval.coalesces());
        assert!(!PollReason::ResetWatch.coalesces());
    }

    #[test]
    fn reset_poll_delay_backs_off_then_gives_up() {
        let observed: Vec<Option<u32>> = (0..8).map(reset_poll_delay_ms).collect();
        assert_eq!(
            observed,
            vec![
                Some(5_000),
                Some(10_000),
                Some(20_000),
                Some(40_000),
                Some(80_000),
                Some(160_000),
                None,
                None,
            ]
        );
    }

    #[test]
    fn reset_poll_delay_never_below_floor_or_above_ceiling() {
        for attempts in 0..1_000u32 {
            if let Some(ms) = reset_poll_delay_ms(attempts) {
                assert!(ms >= RESET_POLL_MIN_INTERVAL_MS, "attempt {attempts}: {ms}");
                assert!(ms <= RESET_POLL_MAX_INTERVAL_MS, "attempt {attempts}: {ms}");
            }
        }
        assert!(reset_poll_delay_ms(u32::MAX).is_none());
    }

    /// Regression test for the unbounded 5-second reset-poll loop.
    ///
    /// Before the fix, the reset watch fired every 5 s for as long as the API
    /// kept reporting a past `resets_at`, i.e. 720 polls per hour with no upper
    /// bound. Simulate an hour of "still past reset" responses and assert the
    /// policy stops after a small, fixed number of polls.
    #[test]
    fn reset_poll_storm_is_bounded() {
        let one_hour_ms: u64 = 60 * 60 * 1_000;
        let mut simulated_ms: u64 = 0;
        let mut polls = 0u32;
        let mut attempts = 0u32;

        while simulated_ms < one_hour_ms {
            match reset_poll_delay_ms(attempts) {
                Some(delay) => {
                    simulated_ms += u64::from(delay);
                    attempts += 1;
                    polls += 1;
                }
                None => break,
            }
        }

        assert_eq!(polls, RESET_POLL_MAX_ATTEMPTS);
        assert!(
            simulated_ms < 400_000,
            "fast-poll phase must end within a few minutes, took {simulated_ms} ms"
        );
        assert!(
            polls < 720,
            "must be far below the pre-fix rate of one poll per 5 seconds"
        );
    }

    #[test]
    fn mono_mask_bytes_matches_word_aligned_stride() {
        assert_eq!(mono_mask_bytes(64, 64), 512);
        assert_eq!(mono_mask_bytes(20, 20), 80);
        assert_eq!(mono_mask_bytes(1, 1), 2);
        assert_eq!(mono_mask_bytes(17, 3), 12);
        assert_eq!(mono_mask_bytes(16, 1), 2);
        assert_eq!(mono_mask_bytes(32, 32), 128);
        assert_eq!(mono_mask_bytes(0, 0), 0);
        assert_eq!(mono_mask_bytes(-1, 10), 0);
        assert_eq!(mono_mask_bytes(10, -1), 0);
    }

    #[test]
    fn mono_mask_bytes_is_never_smaller_than_naive_bit_count() {
        // The naive `(w*h+7)/8` formula was the old (wrong) size. The correct
        // WORD-aligned size must always cover at least that many bytes.
        for width in 1..=70 {
            for height in 1..=70 {
                let naive = ((width * height) as usize).div_ceil(8);
                assert!(
                    mono_mask_bytes(width, height) >= naive,
                    "{width}x{height}: aligned {} < naive {naive}",
                    mono_mask_bytes(width, height)
                );
            }
        }
    }

    #[test]
    fn pixel_count_rejects_non_positive_dimensions() {
        assert_eq!(pixel_count(0, 46), None);
        assert_eq!(pixel_count(-1, 46), None);
        assert_eq!(pixel_count(300, 0), None);
        assert_eq!(pixel_count(300, -46), None);
    }

    #[test]
    fn pixel_count_matches_dib_size_for_realistic_widgets() {
        for (width, height) in [(200, 46), (300, 69), (400, 92), (1, 1)] {
            let count = pixel_count(width, height).expect("positive dims");
            assert_eq!(count * 4, (width as usize) * 4 * (height as usize));
        }
    }

    #[test]
    fn pixel_count_is_overflow_safe() {
        let result = pixel_count(i32::MAX, i32::MAX);
        // On 64-bit targets the product fits; on 32-bit it must be None, never a
        // wrapped value.
        if let Some(n) = result {
            assert_eq!(n, (i32::MAX as usize) * (i32::MAX as usize));
        }
    }
}
