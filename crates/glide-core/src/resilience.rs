//! Cross-platform sleep/wake detection, fast-recovery reconnect backoff, and
//! post-resume connection health probing for Glide.
//!
//! Pure Rust (zero OS-specific C libraries): works identically on Windows 11,
//! Linux X11/Wayland, and macOS.

use std::time::{Duration, Instant, SystemTime};

/// Default jump threshold above which a monotonic or wall-clock leap between
/// periodic ticks is classified as a system sleep/suspend or severe stall.
pub const DEFAULT_SLEEP_JUMP_THRESHOLD: Duration = Duration::from_secs(5);

/// Initial reconnect delay after a clean session drop or wake/roaming event.
pub const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(200);

/// Maximum backoff ceiling while the remote server remains unreachable.
pub const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Default deadline for a post-resume / post-roaming `Ping` health check before
/// declaring a half-open connection dead and forcing fast recovery.
pub const DEFAULT_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Detects system sleep/suspend or long process stalls.
///
/// Platform clock behavior:
/// - On Linux/Unix, `Instant` (`CLOCK_MONOTONIC`) **pauses during system suspend**,
///   so after a 10-minute lid-close sleep `Instant::elapsed()` only reports the
///   normal tick interval (~500ms) while `SystemTime` jumps forward by 600s.
///   Therefore Unix builds monitor both `Instant` and `SystemTime`.
/// - On Windows, `Instant` (`QueryPerformanceCounter`) keeps advancing across
///   sleep/hibernate, so `SleepDetector::check()` monitors **only** monotonic
///   `Instant` jumps — making Windows immune to forward `w32time` NTP wall-clock
///   steps that would otherwise trigger false-positive suspend disconnects.
/// - Backwards NTP wall-clock steps yield `Duration::ZERO` and never cause
///   false positives.
#[derive(Debug, Clone)]
pub struct SleepDetector {
    threshold: Duration,
    last_mono: Instant,
    last_wall: SystemTime,
}

impl Default for SleepDetector {
    fn default() -> Self {
        Self::new(DEFAULT_SLEEP_JUMP_THRESHOLD)
    }
}

impl SleepDetector {
    pub fn new(threshold: Duration) -> Self {
        Self::with_times(threshold, Instant::now(), SystemTime::now())
    }

    pub fn with_times(threshold: Duration, mono: Instant, wall: SystemTime) -> Self {
        Self {
            threshold,
            last_mono: mono,
            last_wall: wall,
        }
    }

    /// Resets the reference clocks to `now` (e.g. right after handling an explicit
    /// OS `SystemResumed` event so the next tick does not double-fire).
    pub fn reset(&mut self) {
        self.reset_at(Instant::now(), SystemTime::now());
    }

    pub fn reset_at(&mut self, now_mono: Instant, now_wall: SystemTime) {
        self.last_mono = now_mono;
        self.last_wall = now_wall;
    }

    /// Checks whether a sleep/resume jump larger than `threshold` occurred since
    /// the last call, advancing internal reference timestamps.
    ///
    /// On Windows, checks only monotonic [`Instant`] (immune to NTP wall-clock steps).
    /// On Unix/Linux, checks both monotonic [`Instant`] and wall-clock [`SystemTime`].
    pub fn check(&mut self) -> Option<Duration> {
        #[cfg(target_os = "windows")]
        {
            self.check_monotonic_at(Instant::now())
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.check_at(Instant::now(), SystemTime::now())
        }
    }

    /// Deterministic testable variant checking only monotonic [`Instant`] time
    /// (used by [`Self::check`] on Windows so NTP wall-clock steps cannot false-trigger).
    pub fn check_monotonic_at(&mut self, now_mono: Instant) -> Option<Duration> {
        let mono_dt = now_mono.saturating_duration_since(self.last_mono);
        self.last_mono = now_mono;
        if mono_dt > self.threshold {
            Some(mono_dt)
        } else {
            None
        }
    }

    /// Deterministic testable variant of [`SleepDetector::check`] comparing both
    /// monotonic and wall-clock deltas.
    pub fn check_at(&mut self, now_mono: Instant, now_wall: SystemTime) -> Option<Duration> {
        let mono_dt = now_mono.saturating_duration_since(self.last_mono);
        let wall_dt = now_wall
            .duration_since(self.last_wall)
            .unwrap_or(Duration::ZERO);
        self.last_mono = now_mono;
        self.last_wall = now_wall;

        let jump = mono_dt.max(wall_dt);
        if jump > self.threshold {
            Some(jump)
        } else {
            None
        }
    }
}

/// Exponential reconnect backoff with instant reset on system wake, network
/// roaming, or healthy session establishment.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    initial: Duration,
    max: Duration,
    current: Duration,
    attempts: u32,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self::new(INITIAL_RECONNECT_DELAY, MAX_RECONNECT_DELAY)
    }
}

impl ReconnectBackoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        let max = max.max(initial);
        Self {
            initial,
            max,
            current: initial,
            attempts: 0,
        }
    }

    /// Returns the delay to wait before the next reconnect attempt and advances
    /// the exponential backoff state (capped at `max`).
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self
            .current
            .checked_mul(2)
            .unwrap_or(self.max)
            .min(self.max);
        self.attempts = self.attempts.saturating_add(1);
        delay
    }

    /// Resets the backoff immediately back to `initial` so waking a laptop or
    /// switching Wi-Fi/Ethernet reconnects in under a second.
    pub fn reset(&mut self) {
        self.current = self.initial;
        self.attempts = 0;
    }

    pub fn current_delay(&self) -> Duration {
        self.current
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// Tracks an in-flight post-resume or post-roaming `Ping` health check so stale
/// half-open QUIC connections fail fast instead of stalling for idle timeout.
#[derive(Debug, Clone)]
pub struct HealthProbe {
    timeout: Duration,
    pending_since: Option<Instant>,
    next_token: u64,
}

impl Default for HealthProbe {
    fn default() -> Self {
        Self::new(DEFAULT_HEALTH_PROBE_TIMEOUT)
    }
}

impl HealthProbe {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            pending_since: None,
            next_token: 1,
        }
    }

    /// Arms a health probe deadline (preserving any earlier still-pending deadline
    /// so repeated wake notifications cannot postpone timeout indefinitely) and
    /// returns a unique `ts_us` token to place in `ControlMessage::Ping`.
    pub fn start_probe(&mut self, now: Instant) -> u64 {
        if self.pending_since.is_none() {
            self.pending_since = Some(now);
        }
        let tok = self.next_token;
        self.next_token = self.next_token.wrapping_add(1).max(1);
        tok
    }

    /// Marks the peer/connection healthy (called on `Pong` or any valid inbound traffic).
    pub fn mark_alive(&mut self) {
        self.pending_since = None;
    }

    /// Returns `true` if a probe has been waiting at least `timeout` with no reply.
    pub fn is_timed_out(&self, now: Instant) -> bool {
        self.pending_since
            .is_some_and(|sent| now.saturating_duration_since(sent) >= self.timeout)
    }

    pub fn is_pending(&self) -> bool {
        self.pending_since.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_detector_catches_linux_wall_clock_jump_when_monotonic_pauses() {
        let mono0 = Instant::now();
        let wall0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut det = SleepDetector::with_times(Duration::from_secs(5), mono0, wall0);

        // Normal 500ms awake tick: no jump
        let mono1 = mono0 + Duration::from_millis(500);
        let wall1 = wall0 + Duration::from_millis(500);
        assert_eq!(det.check_at(mono1, wall1), None);

        // Linux suspend: CLOCK_MONOTONIC only advances 500ms, wall clock jumps 45s!
        let mono2 = mono1 + Duration::from_millis(500);
        let wall2 = wall1 + Duration::from_secs(45);
        let jump = det
            .check_at(mono2, wall2)
            .expect("must detect Linux sleep jump");
        assert_eq!(jump, Duration::from_secs(45));

        // Next normal tick after wake does not re-trigger
        let mono3 = mono2 + Duration::from_millis(500);
        let wall3 = wall2 + Duration::from_millis(500);
        assert_eq!(det.check_at(mono3, wall3), None);
    }

    #[test]
    fn sleep_detector_catches_windows_monotonic_jump_and_ignores_backwards_ntp() {
        let mono0 = Instant::now();
        let wall0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut det = SleepDetector::with_times(Duration::from_secs(5), mono0, wall0);

        // Backwards NTP wall-clock step (-30s) while awake: must NOT false-trigger!
        let mono1 = mono0 + Duration::from_millis(500);
        let wall1 = wall0 - Duration::from_secs(30);
        assert_eq!(det.check_at(mono1, wall1), None);

        // Windows sleep: monotonic jumps 12s even if wall clock did not move forward
        let mono2 = mono1 + Duration::from_secs(12);
        let jump = det
            .check_at(mono2, wall1)
            .expect("must detect monotonic sleep jump");
        assert_eq!(jump, Duration::from_secs(12));

        // Explicit reset prevents double-trigger
        det.reset_at(
            mono2 + Duration::from_secs(10),
            wall1 + Duration::from_secs(10),
        );
        assert_eq!(
            det.check_at(
                mono2 + Duration::from_secs(10) + Duration::from_millis(500),
                wall1 + Duration::from_secs(10) + Duration::from_millis(500),
            ),
            None
        );
    }

    #[test]
    fn sleep_detector_monotonic_only_catches_sleep_and_ignores_wall_clock() {
        let mono0 = Instant::now();
        let wall0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut det = SleepDetector::with_times(Duration::from_secs(5), mono0, wall0);

        // Normal awake tick (even if system wall clock stepped +120s via w32time NTP):
        // monotonic-only path only inspects Instant, so zero false positive!
        let mono1 = mono0 + Duration::from_millis(500);
        assert_eq!(det.check_monotonic_at(mono1), None);

        // Genuine Windows sleep/hibernate advances QPC/Instant by 20s -> detected!
        let mono2 = mono1 + Duration::from_secs(20);
        assert_eq!(det.check_monotonic_at(mono2), Some(Duration::from_secs(20)));

        // Subsequent normal tick does not double-fire
        assert_eq!(
            det.check_monotonic_at(mono2 + Duration::from_millis(500)),
            None
        );
    }

    #[test]
    fn reconnect_backoff_scales_exponentially_and_resets_immediately_on_wake() {
        let mut bo = ReconnectBackoff::new(Duration::from_millis(200), Duration::from_secs(3));
        assert_eq!(bo.next_delay(), Duration::from_millis(200));
        assert_eq!(bo.next_delay(), Duration::from_millis(400));
        assert_eq!(bo.next_delay(), Duration::from_millis(800));
        assert_eq!(bo.next_delay(), Duration::from_millis(1600));
        assert_eq!(bo.next_delay(), Duration::from_secs(3));
        assert_eq!(bo.next_delay(), Duration::from_secs(3));
        assert_eq!(bo.attempts(), 6);

        // System resume / healthy session / network change resets backoff back to <1s immediately!
        bo.reset();
        assert_eq!(bo.attempts(), 0);
        assert_eq!(bo.current_delay(), Duration::from_millis(200));
        assert_eq!(bo.next_delay(), Duration::from_millis(200));
    }

    #[test]
    fn health_probe_times_out_stalled_peer_and_clears_on_pong() {
        let t0 = Instant::now();
        let mut probe = HealthProbe::new(Duration::from_millis(1500));
        assert!(!probe.is_pending());
        assert!(!probe.is_timed_out(t0));

        let tok1 = probe.start_probe(t0);
        assert!(probe.is_pending());
        assert!(!probe.is_timed_out(t0 + Duration::from_millis(1000)));

        // Calling start_probe again at t0+1000ms must NOT extend original deadline!
        let tok2 = probe.start_probe(t0 + Duration::from_millis(1000));
        assert_ne!(tok1, tok2);
        assert!(probe.is_timed_out(t0 + Duration::from_millis(1500)));

        // Receiving Pong clears the probe immediately
        probe.mark_alive();
        assert!(!probe.is_pending());
        assert!(!probe.is_timed_out(t0 + Duration::from_secs(10)));
    }
}
