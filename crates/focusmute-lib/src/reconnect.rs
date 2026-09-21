//! Reconnection with exponential backoff for device communication failures.
//!
//! When the USB device becomes unreachable (unplugged, driver restart, etc.),
//! the reconnection state machine manages retry timing with exponential
//! backoff to avoid hammering the system with reconnect attempts.

use std::time::{Duration, Instant};

/// Configuration for reconnection backoff.
#[derive(Debug, Clone)]
pub struct ReconnectConfig {
    /// Initial delay before the first reconnection attempt.
    pub initial_delay: Duration,
    /// Maximum delay between reconnection attempts.
    pub max_delay: Duration,
    /// Multiplier applied to delay after each failure (typically 2.0).
    pub multiplier: f64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

/// Reconnection state machine with exponential backoff.
#[derive(Debug)]
pub struct ReconnectState {
    config: ReconnectConfig,
    current_delay: Duration,
    last_attempt: Option<Instant>,
    consecutive_failures: u32,
}

impl ReconnectState {
    /// Create a new reconnection state with the given config.
    pub fn new(config: ReconnectConfig) -> Self {
        Self {
            current_delay: config.initial_delay,
            config,
            last_attempt: None,
            consecutive_failures: 0,
        }
    }

    /// Create a new reconnection state with default config.
    pub fn with_defaults() -> Self {
        Self::new(ReconnectConfig::default())
    }

    /// Check if enough time has elapsed to attempt reconnection.
    ///
    /// Returns `true` if no attempt has been made yet, or if the
    /// backoff delay has elapsed since the last attempt.
    pub fn should_attempt(&self) -> bool {
        match self.last_attempt {
            None => true,
            Some(last) => last.elapsed() >= self.current_delay,
        }
    }

    /// Record a failed reconnection attempt and advance the backoff.
    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        self.last_attempt = Some(Instant::now());

        // Advance backoff: current_delay *= multiplier, capped at max_delay
        let next = self.current_delay.as_secs_f64() * self.config.multiplier;
        self.current_delay = Duration::from_secs_f64(next).min(self.config.max_delay);
    }

    /// Record a successful reconnection and reset the backoff.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.current_delay = self.config.initial_delay;
        self.last_attempt = None;
    }

    /// Number of consecutive failed attempts.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Current backoff delay before the next attempt.
    pub fn current_delay(&self) -> Duration {
        self.current_delay
    }
}

/// Testable variant of `try_reopen` that accepts a device factory closure.
///
/// - `device_serial`: preferred serial number (empty = auto-select).
/// - `open_fn`: closure that attempts to open the device by serial.
/// - Returns `None` without attempting if the backoff timer hasn't elapsed.
/// - On success, records success and returns the new device.
/// - On failure, records failure, logs the backoff schedule, and returns `None`.
pub fn try_reopen_with<D, F>(
    state: &mut ReconnectState,
    device_serial: &str,
    open_fn: F,
) -> Option<D>
where
    F: FnOnce(&str) -> crate::device::Result<D>,
{
    if !state.should_attempt() {
        return None;
    }
    match open_fn(device_serial) {
        Ok(dev) => {
            state.record_success();
            Some(dev)
        }
        Err(e) => {
            state.record_failure();
            log::warn!(
                "[device] reconnect failed: {e} (attempt {}, retry in {:.1}s)",
                state.consecutive_failures(),
                state.current_delay().as_secs_f64()
            );
            None
        }
    }
}

/// Attempt to reopen the device, respecting backoff timing.
///
/// - `device_serial`: preferred serial number (empty = auto-select).
/// - Returns `None` without attempting if the backoff timer hasn't elapsed.
/// - On success, records success and returns the new device.
/// - On failure, records failure, logs the backoff schedule, and returns `None`.
pub fn try_reopen(
    state: &mut ReconnectState,
    device_serial: &str,
) -> Option<crate::device::PlatformDevice> {
    try_reopen_with(state, device_serial, crate::device::open_device_by_serial)
}

/// Testable variant of `try_reconnect_and_refresh` that accepts a device factory closure.
pub fn try_reconnect_and_refresh_with<D: crate::device::ScarlettDevice, F>(
    reconnect: &mut ReconnectState,
    strategy: &crate::led::MuteStrategy,
    mute_color: u32,
    is_muted: bool,
    device_serial: &str,
    open_fn: F,
) -> Option<D>
where
    F: FnOnce(&str) -> crate::device::Result<D>,
{
    let dev = try_reopen_with(reconnect, device_serial, open_fn)?;
    if let Err(e) = crate::led::refresh_after_reconnect(&dev, strategy, mute_color, is_muted) {
        log::warn!("[device] could not re-apply mute indicator after reconnect: {e}");
    }
    Some(dev)
}

/// Attempt to reopen the device and re-apply mute indicator after reconnection.
///
/// Combines `try_reopen()` with `led::refresh_after_reconnect()` into a single
/// call. Returns the new device on success.
pub fn try_reconnect_and_refresh(
    reconnect: &mut ReconnectState,
    strategy: &crate::led::MuteStrategy,
    mute_color: u32,
    is_muted: bool,
    device_serial: &str,
) -> Option<crate::device::PlatformDevice> {
    try_reconnect_and_refresh_with(
        reconnect,
        strategy,
        mute_color,
        is_muted,
        device_serial,
        crate::device::open_device_by_serial,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let config = ReconnectConfig::default();
        assert_eq!(config.initial_delay, Duration::from_secs(1));
        assert_eq!(config.max_delay, Duration::from_secs(30));
        assert_eq!(config.multiplier, 2.0);
    }

    #[test]
    fn initial_should_attempt_is_true() {
        let state = ReconnectState::with_defaults();
        assert!(state.should_attempt());
        assert_eq!(state.consecutive_failures(), 0);
    }

    #[test]
    fn backoff_progresses_on_failure() {
        let config = ReconnectConfig {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            multiplier: 2.0,
        };
        let mut state = ReconnectState::new(config);

        // Initial delay is 100ms
        assert_eq!(state.current_delay(), Duration::from_millis(100));

        state.record_failure();
        assert_eq!(state.consecutive_failures(), 1);
        assert_eq!(state.current_delay(), Duration::from_millis(200));

        state.record_failure();
        assert_eq!(state.consecutive_failures(), 2);
        assert_eq!(state.current_delay(), Duration::from_millis(400));

        state.record_failure();
        assert_eq!(state.consecutive_failures(), 3);
        assert_eq!(state.current_delay(), Duration::from_millis(800));
    }

    #[test]
    fn backoff_capped_at_max() {
        let config = ReconnectConfig {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(4),
            multiplier: 2.0,
        };
        let mut state = ReconnectState::new(config);

        state.record_failure(); // 1s → 2s
        assert_eq!(state.current_delay(), Duration::from_secs(2));

        state.record_failure(); // 2s → 4s (= max)
        assert_eq!(state.current_delay(), Duration::from_secs(4));

        state.record_failure(); // 4s → 4s (capped)
        assert_eq!(state.current_delay(), Duration::from_secs(4));
    }

    #[test]
    fn success_resets_backoff() {
        let mut state = ReconnectState::with_defaults();

        state.record_failure();
        state.record_failure();
        assert_eq!(state.consecutive_failures(), 2);
        assert_ne!(state.current_delay(), Duration::from_secs(1));

        state.record_success();
        assert_eq!(state.consecutive_failures(), 0);
        assert_eq!(state.current_delay(), Duration::from_secs(1));
        assert!(state.should_attempt());
    }

    #[test]
    fn should_attempt_false_immediately_after_failure() {
        let config = ReconnectConfig {
            initial_delay: Duration::from_secs(60), // very long delay
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
        };
        let mut state = ReconnectState::new(config);

        state.record_failure();
        // Immediately after failure, should_attempt should be false
        // (unless 60 seconds have somehow elapsed)
        assert!(!state.should_attempt());
    }

    #[test]
    fn should_attempt_true_after_delay_elapses() {
        let config = ReconnectConfig {
            initial_delay: Duration::from_millis(1), // 1ms delay
            max_delay: Duration::from_secs(1),
            multiplier: 2.0,
        };
        let mut state = ReconnectState::new(config);

        state.record_failure();
        // Wait for the delay to elapse
        std::thread::sleep(Duration::from_millis(10));
        assert!(state.should_attempt());
    }

    #[test]
    fn custom_multiplier() {
        let config = ReconnectConfig {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            multiplier: 3.0,
        };
        let mut state = ReconnectState::new(config);

        state.record_failure(); // 100ms → 300ms
        assert_eq!(state.current_delay(), Duration::from_millis(300));

        state.record_failure(); // 300ms → 900ms
        assert_eq!(state.current_delay(), Duration::from_millis(900));
    }

    #[test]
    fn multiple_success_calls_idempotent() {
        let mut state = ReconnectState::with_defaults();
        state.record_failure();
        state.record_failure();

        state.record_success();
        state.record_success(); // second call should be fine

        assert_eq!(state.consecutive_failures(), 0);
        assert_eq!(state.current_delay(), Duration::from_secs(1));
    }

    // ── try_reopen_with / try_reconnect_and_refresh_with ──

    use crate::device::DeviceError;
    use crate::device::mock::MockDevice;

    fn mock_open_ok(_serial: &str) -> crate::device::Result<MockDevice> {
        Ok(MockDevice::new())
    }

    fn mock_open_err(_serial: &str) -> crate::device::Result<MockDevice> {
        Err(DeviceError::TransactFailed("no device".into()))
    }

    fn make_strategy() -> crate::led::MuteStrategy {
        crate::led::MuteStrategy {
            input_indices: vec![0],
            number_leds: vec![0],
            mute_colors: vec![],
            selected_color: 0x20FF_0000,
            unselected_color: 0x88FF_FF00,
            offsets: Default::default(),
        }
    }

    #[test]
    fn try_reopen_with_success() {
        let mut state = ReconnectState::with_defaults();
        let result = try_reopen_with(&mut state, "MOCK123", mock_open_ok);
        assert!(result.is_some());
        assert_eq!(state.consecutive_failures(), 0);
        assert!(state.should_attempt()); // reset after success
    }

    #[test]
    fn try_reopen_with_failure() {
        let mut state = ReconnectState::with_defaults();
        let result: Option<MockDevice> = try_reopen_with(&mut state, "MOCK123", mock_open_err);
        assert!(result.is_none());
        assert_eq!(state.consecutive_failures(), 1);
    }

    #[test]
    fn try_reopen_with_skips_during_backoff() {
        let config = ReconnectConfig {
            initial_delay: Duration::from_secs(60),
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
        };
        let mut state = ReconnectState::new(config);

        // First failure puts us in backoff
        let _: Option<MockDevice> = try_reopen_with(&mut state, "", mock_open_err);
        assert_eq!(state.consecutive_failures(), 1);

        // Track whether the factory was called
        let mut called = false;
        let result: Option<MockDevice> = try_reopen_with(&mut state, "", |_serial| {
            called = true;
            Ok(MockDevice::new())
        });
        assert!(result.is_none(), "should skip during backoff");
        assert!(!called, "open_fn should not be called during backoff");
        assert_eq!(state.consecutive_failures(), 1, "failure count unchanged");
    }

    #[test]
    fn try_reconnect_and_refresh_with_success() {
        let mut state = ReconnectState::with_defaults();
        let strategy = make_strategy();
        let mute_color = 0xFF00_0000;

        // Not muted → refresh is a no-op, device returned
        let result: Option<MockDevice> = try_reconnect_and_refresh_with(
            &mut state,
            &strategy,
            mute_color,
            false,
            "MOCK123",
            mock_open_ok,
        );
        assert!(result.is_some());
        assert_eq!(state.consecutive_failures(), 0);
    }

    #[test]
    fn try_reconnect_and_refresh_with_led_failure() {
        let mut state = ReconnectState::with_defaults();
        let strategy = make_strategy();
        let mute_color = 0xFF00_0000;

        // Mock device that fails set_descriptor → LED refresh will fail,
        // but the device should still be returned.
        let result: Option<MockDevice> = try_reconnect_and_refresh_with(
            &mut state,
            &strategy,
            mute_color,
            true, // muted → triggers LED refresh
            "MOCK123",
            |_serial| {
                let dev = MockDevice::new();
                dev.fail_set_descriptor.set(true);
                Ok(dev)
            },
        );
        assert!(
            result.is_some(),
            "device should be returned despite LED failure"
        );
        // Verify the device has fail_set_descriptor set (confirming our mock was used)
        assert!(result.unwrap().fail_set_descriptor.get());
    }

    #[test]
    fn try_reconnect_and_refresh_with_open_failure() {
        let mut state = ReconnectState::with_defaults();
        let strategy = make_strategy();
        let mute_color = 0xFF00_0000;

        let result: Option<MockDevice> = try_reconnect_and_refresh_with(
            &mut state,
            &strategy,
            mute_color,
            true,
            "MOCK123",
            mock_open_err,
        );
        assert!(result.is_none());
        assert_eq!(state.consecutive_failures(), 1);
    }
}
