//! Per-server health state tracking for Porter.
//!
//! Tracks error rates using a sliding time window and transitions between
//! Starting, Healthy, Degraded, and Unhealthy states based on observed error rates.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Four-state health model for managed MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// Insufficient samples to determine health (fewer than 5)
    Starting,
    /// Error rate below 5%
    Healthy,
    /// Error rate between 5% and 50%
    Degraded,
    /// Error rate above 50% or process dead
    Unhealthy,
}

/// Tracks call success/error events in a sliding time window to compute health state.
pub struct ErrorRateTracker {
    /// Ring buffer of (timestamp, was_error) pairs within the window
    window: VecDeque<(Instant, bool)>,
    /// Duration of the sliding window
    window_duration: Duration,
}

impl ErrorRateTracker {
    /// Create a new tracker with the given sliding window duration.
    pub fn new(window_duration: Duration) -> Self {
        Self {
            window: VecDeque::new(),
            window_duration,
        }
    }

    /// Record a successful call.
    pub fn record_success(&mut self) {
        self.window.push_back((Instant::now(), false));
        self.prune();
    }

    /// Record a failed call.
    pub fn record_error(&mut self) {
        self.window.push_back((Instant::now(), true));
        self.prune();
    }

    /// Compute current health state based on the sliding window.
    pub fn health_state(&self) -> HealthState {
        let total = self.window.len();

        // Need at least 5 samples before making a health determination
        if total < 5 {
            return HealthState::Starting;
        }

        let errors = self.window.iter().filter(|(_, is_err)| *is_err).count();
        let error_rate = errors as f64 / total as f64;

        if error_rate < 0.05 {
            HealthState::Healthy
        } else if error_rate <= 0.50 {
            HealthState::Degraded
        } else {
            HealthState::Unhealthy
        }
    }

    /// Remove entries older than the window duration.
    fn prune(&mut self) {
        let cutoff = Instant::now() - self.window_duration;
        while let Some((ts, _)) = self.window.front() {
            if *ts < cutoff {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    /// Number of entries currently in the window (for testing).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.window.len()
    }
}

/// Rolling buffer for per-server stderr output, for diagnostics.
pub struct StderrBuffer {
    lines: VecDeque<String>,
    capacity: usize,
}

impl StderrBuffer {
    /// Create a new stderr buffer with the given line capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity,
        }
    }

    /// Push a new stderr line, evicting the oldest if at capacity.
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Read access to buffered stderr lines.
    pub fn lines(&self) -> &VecDeque<String> {
        &self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker_with_samples(successes: usize, errors: usize) -> ErrorRateTracker {
        let mut tracker = ErrorRateTracker::new(Duration::from_secs(60));
        for _ in 0..successes {
            tracker.record_success();
        }
        for _ in 0..errors {
            tracker.record_error();
        }
        tracker
    }

    #[test]
    fn test_health_starting_below_threshold() {
        let tracker = tracker_with_samples(4, 0);
        assert_eq!(tracker.health_state(), HealthState::Starting);
    }

    #[test]
    fn test_health_starting_zero_samples() {
        let tracker = ErrorRateTracker::new(Duration::from_secs(60));
        assert_eq!(tracker.health_state(), HealthState::Starting);
    }

    #[test]
    fn test_health_healthy() {
        let tracker = tracker_with_samples(10, 0);
        assert_eq!(tracker.health_state(), HealthState::Healthy);
    }

    #[test]
    fn test_health_degraded() {
        // 10% error rate: 1 error in 10 calls
        let tracker = tracker_with_samples(9, 1);
        assert_eq!(tracker.health_state(), HealthState::Degraded);
    }

    #[test]
    fn test_health_unhealthy() {
        // 60% error rate: 6 errors in 10 calls
        let tracker = tracker_with_samples(4, 6);
        assert_eq!(tracker.health_state(), HealthState::Unhealthy);
    }

    #[test]
    fn test_stderr_buffer_capacity() {
        let mut buf = StderrBuffer::new(3);
        buf.push("line1".to_string());
        buf.push("line2".to_string());
        buf.push("line3".to_string());
        buf.push("line4".to_string()); // should evict "line1"
        assert_eq!(buf.lines().len(), 3);
        assert_eq!(buf.lines().front().unwrap(), "line2");
        assert_eq!(buf.lines().back().unwrap(), "line4");
    }

    #[test]
    fn test_error_rate_window_pruning() {
        // Use a very short window to test pruning
        let mut tracker = ErrorRateTracker::new(Duration::from_millis(50));
        // Add 3 entries
        tracker.record_success();
        tracker.record_success();
        tracker.record_error();
        assert_eq!(tracker.len(), 3);
        // Wait for window to expire
        std::thread::sleep(Duration::from_millis(100));
        // Adding a new entry will trigger prune of old entries
        tracker.record_success();
        // The 3 old entries should have been pruned
        assert_eq!(tracker.len(), 1);
    }
}
