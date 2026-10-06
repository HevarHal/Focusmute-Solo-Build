//! Monitor state machine — testable mute-indicator logic decoupled from I/O.
//!
//! The `MuteIndicator` encapsulates the core state transitions for the mute
//! monitor loop: debouncing input, deciding when to apply/clear mute colors,
//! and executing the LED writes. CLI and tray binaries become thin adapters
//! that wire I/O sources (audio monitor, device handle) to this state machine.
// Modified for the Scarlett Solo build; see README.md.

use crate::audio::MuteDebouncer;
use crate::device::{Result, ScarlettDevice};
use crate::led;
use std::cell::RefCell;

const SOLO_DIRECT_LED_INDICES: [u8; 2] = [27, 31];

/// Action to take after a mute-state update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorAction {
    /// Apply the mute color to the number LEDs.
    ApplyMute,
    /// Clear the mute color and restore the number LEDs.
    ClearMute,
    /// No state change — do nothing.
    NoChange,
}

/// Mute indicator state machine.
///
/// Processes raw mute polls through a debouncer and tracks the confirmed
/// mute state. Call [`update`] each poll cycle and match on the returned
/// [`MonitorAction`] to decide what to do.
pub struct MuteIndicator {
    debouncer: MuteDebouncer,
    mute_color: u32,
    strategy: led::MuteStrategy,
    solo_direct_leds: bool,
    saved_solo_gradient: RefCell<Option<Vec<u8>>>,
}

impl MuteIndicator {
    /// Create a new indicator with the given debounce threshold, initial state,
    /// mute color, and mute strategy.
    pub fn new(
        debounce_threshold: u32,
        initial_muted: bool,
        mute_color: u32,
        strategy: led::MuteStrategy,
    ) -> Self {
        Self {
            debouncer: MuteDebouncer::new(debounce_threshold, initial_muted),
            mute_color,
            strategy,
            solo_direct_leds: false,
            saved_solo_gradient: RefCell::new(None),
        }
    }

    /// Feed a raw mute poll. Returns the action to take, if any.
    pub fn update(&mut self, muted: bool) -> MonitorAction {
        match self.debouncer.update(muted) {
            Some(true) => MonitorAction::ApplyMute,
            Some(false) => MonitorAction::ClearMute,
            None => MonitorAction::NoChange,
        }
    }

    /// Apply the mute indicator to the device.
    pub fn apply_mute(&self, device: &impl ScarlettDevice) -> Result<()> {
        led::apply_mute_indicator(device, &self.strategy, self.mute_color)?;
        if self.solo_direct_leds {
            for index in SOLO_DIRECT_LED_INDICES {
                led::set_single_led(device, &self.strategy.offsets, index, self.mute_color)?;
            }
            self.apply_solo_halo_gradient(device)?;
        }
        Ok(())
    }

    /// Clear the mute indicator and restore normal LED state.
    pub fn clear_mute(&self, device: &impl ScarlettDevice) -> Result<()> {
        led::clear_mute_indicator(device, &self.strategy)?;
        if self.solo_direct_leds {
            self.restore_solo_halo_gradient(device)?;
            for index in SOLO_DIRECT_LED_INDICES {
                led::set_single_led(
                    device,
                    &self.strategy.offsets,
                    index,
                    self.strategy.unselected_color,
                )?;
            }
        }
        Ok(())
    }

    /// Whether the debouncer currently considers the mic muted.
    pub fn is_muted(&self) -> bool {
        self.debouncer.is_muted()
    }

    /// The configured mute color.
    pub fn mute_color(&self) -> u32 {
        self.mute_color
    }

    /// Reference to the mute strategy.
    pub fn strategy(&self) -> &led::MuteStrategy {
        &self.strategy
    }

    /// Update the mute color (e.g. after settings change).
    pub fn set_mute_color(&mut self, color: u32) {
        self.mute_color = color;
    }

    /// Replace the mute strategy (e.g. after mute_inputs setting change).
    pub fn set_strategy(&mut self, strategy: led::MuteStrategy) {
        self.strategy = strategy;
    }

    pub fn set_solo_direct_leds(&mut self, enabled: bool) {
        self.solo_direct_leds = enabled;
        if enabled && self.strategy.offsets.metering_gradient.is_none() {
            log::warn!(
                "[led] Solo halo mute colors are unavailable because the firmware gradient could not be read from schema"
            );
        }
    }

    fn apply_solo_halo_gradient(&self, device: &impl ScarlettDevice) -> Result<()> {
        let Some(gradient) = self.strategy.offsets.metering_gradient else {
            return Ok(());
        };
        let mut saved = self.saved_solo_gradient.borrow_mut();
        if saved.is_none() {
            *saved = Some(led::read_metering_gradient(device, gradient)?);
        }
        let Some(original) = saved.as_ref() else {
            return Err(crate::device::DeviceError::TransactFailed(
                "metering gradient was not saved before applying mute color".into(),
            ));
        };
        let mut colors = original.clone();
        for color in colors.chunks_exact_mut(std::mem::size_of::<u32>()).skip(1) {
            color.copy_from_slice(&self.mute_color.to_le_bytes());
        }
        led::write_metering_gradient(device, gradient, &colors)
    }

    fn restore_solo_halo_gradient(&self, device: &impl ScarlettDevice) -> Result<()> {
        let Some(gradient) = self.strategy.offsets.metering_gradient else {
            return Ok(());
        };
        let mut saved = self.saved_solo_gradient.borrow_mut();
        if let Some(original) = saved.as_ref() {
            led::write_metering_gradient(device, gradient, original)?;
            *saved = None;
        }
        Ok(())
    }

    /// Force the debouncer's confirmed state without triggering a state-change event.
    ///
    /// Use this when the mute state is known from an authoritative source (e.g.
    /// the audio API at startup). After calling this, subsequent polls matching
    /// the forced state will return `NoChange` instead of `ApplyMute`/`ClearMute`.
    pub fn force_state(&mut self, muted: bool) {
        self.debouncer.force_state(muted);
    }

    /// Use a hardware button event as the first debounce sample without
    /// applying LEDs immediately; the next matching audio poll performs the
    /// single normal LED update.
    pub fn prime_external_transition(&mut self, muted: bool) {
        self.debouncer.prime_transition(muted);
    }

    /// Feed a raw mute poll and apply the resulting action to the device.
    ///
    /// Returns the action taken (for callers that need to update UI, play sounds, etc.)
    /// and a device error if the LED write failed.
    pub fn poll_and_apply(
        &mut self,
        muted: bool,
        device: &impl ScarlettDevice,
    ) -> (MonitorAction, Option<crate::device::DeviceError>) {
        let action = self.update(muted);
        let err = match action {
            MonitorAction::ApplyMute => self.apply_mute(device).err(),
            MonitorAction::ClearMute => self.clear_mute(device).err(),
            MonitorAction::NoChange => None,
        };
        (action, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mock::MockDevice;
    use crate::protocol::*;

    fn make_indicator(initial: bool) -> MuteIndicator {
        MuteIndicator::new(
            2,
            initial,
            0xFF00_0000,
            led::MuteStrategy {
                input_indices: vec![0, 1],
                number_leds: vec![0, 8],
                mute_colors: vec![],
                selected_color: 0x20FF_0000,
                unselected_color: 0x88FF_FF00,
                offsets: Default::default(),
            },
        )
    }

    #[test]
    fn initial_state_not_muted() {
        let ind = make_indicator(false);
        assert!(!ind.is_muted());
    }

    #[test]
    fn initial_state_muted() {
        let ind = make_indicator(true);
        assert!(ind.is_muted());
    }

    #[test]
    fn update_returns_no_change_below_threshold() {
        let mut ind = make_indicator(false);
        assert_eq!(ind.update(true), MonitorAction::NoChange);
        // Not yet at threshold of 2
        assert!(!ind.is_muted());
    }

    #[test]
    fn update_returns_apply_mute_at_threshold() {
        let mut ind = make_indicator(false);
        assert_eq!(ind.update(true), MonitorAction::NoChange);
        assert_eq!(ind.update(true), MonitorAction::ApplyMute);
        assert!(ind.is_muted());
    }

    #[test]
    fn update_returns_clear_mute_on_unmute() {
        let mut ind = make_indicator(true);
        assert_eq!(ind.update(false), MonitorAction::NoChange);
        assert_eq!(ind.update(false), MonitorAction::ClearMute);
        assert!(!ind.is_muted());
    }

    #[test]
    fn flicker_resets_debounce() {
        let mut ind = make_indicator(false);
        assert_eq!(ind.update(true), MonitorAction::NoChange);
        // Flicker back
        assert_eq!(ind.update(false), MonitorAction::NoChange);
        // Must restart count
        assert_eq!(ind.update(true), MonitorAction::NoChange);
        assert_eq!(ind.update(true), MonitorAction::ApplyMute);
    }

    #[test]
    fn same_state_always_no_change() {
        let mut ind = make_indicator(false);
        for _ in 0..10 {
            assert_eq!(ind.update(false), MonitorAction::NoChange);
        }
    }

    #[test]
    fn apply_mute_writes_number_led() {
        let ind = make_indicator(false);
        let dev = MockDevice::new();
        ind.apply_mute(&dev).unwrap();

        let descs = dev.descriptors.borrow();
        // Should use single-LED update, NOT direct mode
        assert!(!descs.contains_key(&OFF_ENABLE_DIRECT_LED));
        assert!(!descs.contains_key(&OFF_DIRECT_LED_VALUES));
        // Should have written directLEDColour and directLEDIndex
        assert!(descs.contains_key(&OFF_DIRECT_LED_COLOUR));
        assert!(descs.contains_key(&OFF_DIRECT_LED_INDEX));
    }

    #[test]
    fn solo_direct_led_follows_input_mute_color_and_restore_color() {
        let mut indicator = make_indicator(false);
        indicator.set_solo_direct_leds(true);
        indicator.set_mute_color(0x0000_FF00);
        let device = MockDevice::new();
        let original_gradient: Vec<u8> = (0..44).map(|byte| byte as u8).collect();
        device
            .set_descriptor(384, &original_gradient)
            .expect("seed the original metering gradient");

        indicator.apply_mute(&device).unwrap();
        let descriptors = device.descriptors.borrow();
        assert_eq!(
            descriptors.get(&OFF_DIRECT_LED_COLOUR).unwrap(),
            &0x0000_FF00u32.to_le_bytes(),
            "Direct LED should use the current configured mute color"
        );
        assert_eq!(descriptors.get(&OFF_DIRECT_LED_INDEX).unwrap(), &[31]);
        let muted_gradient = descriptors.get(&384).unwrap();
        assert_eq!(&muted_gradient[..4], &original_gradient[..4]);
        assert!(
            muted_gradient[4..]
                .chunks_exact(4)
                .all(|color| color == [0, 255, 0, 0])
        );
        drop(descriptors);

        indicator.clear_mute(&device).unwrap();
        let descriptors = device.descriptors.borrow();
        assert_eq!(
            descriptors.get(&OFF_DIRECT_LED_COLOUR).unwrap(),
            &0x88FF_FF00u32.to_le_bytes(),
            "Direct LED should use the same restore color as the input LEDs"
        );
        assert_eq!(descriptors.get(&OFF_DIRECT_LED_INDEX).unwrap(), &[31]);
        assert_eq!(descriptors.get(&384).unwrap(), &original_gradient);
        assert_eq!(
            device
                .notifies
                .borrow()
                .iter()
                .filter(|&&event| event == 9)
                .count(),
            2,
            "mute and unmute should each activate the metering gradient"
        );
    }

    #[test]
    fn clear_mute_restores_number_leds() {
        let ind = make_indicator(true);
        let dev = MockDevice::new();

        // Set up selectedInput for restore
        dev.set_descriptor(OFF_SELECTED_INPUT, &[0]).unwrap();

        ind.apply_mute(&dev).unwrap();
        ind.clear_mute(&dev).unwrap();

        let notifies = dev.notifies.borrow();
        assert!(
            !notifies.contains(&NOTIFY_DIRECT_LED_VALUES),
            "should not send DATA_NOTIFY(5)"
        );
        // Should have sent multiple DATA_NOTIFY(8) events (apply + clear)
        assert!(
            notifies
                .iter()
                .filter(|&&n| n == NOTIFY_DIRECT_LED_COLOUR)
                .count()
                >= 2,
            "should send DATA_NOTIFY(8) for apply and clear"
        );
    }

    // ── poll_and_apply ──

    #[test]
    fn poll_and_apply_no_change() {
        let mut ind = make_indicator(false);
        let dev = MockDevice::new();
        let (action, err) = ind.poll_and_apply(false, &dev);
        assert_eq!(action, MonitorAction::NoChange);
        assert!(err.is_none());
        // No writes should have happened
        assert!(dev.descriptors.borrow().is_empty());
    }

    #[test]
    fn poll_and_apply_triggers_mute_at_threshold() {
        let mut ind = make_indicator(false);
        let dev = MockDevice::new();

        // First poll: NoChange
        let (a1, _) = ind.poll_and_apply(true, &dev);
        assert_eq!(a1, MonitorAction::NoChange);

        // Second poll: ApplyMute (threshold=2)
        let (a2, e2) = ind.poll_and_apply(true, &dev);
        assert_eq!(a2, MonitorAction::ApplyMute);
        assert!(e2.is_none());
        assert!(ind.is_muted());

        // Verify LED was written
        let descs = dev.descriptors.borrow();
        assert!(descs.contains_key(&OFF_DIRECT_LED_COLOUR));
    }

    #[test]
    fn primed_external_mute_applies_led_once_on_next_poll() {
        let mut ind = make_indicator(false);
        let dev = MockDevice::new();

        ind.prime_external_transition(true);
        assert!(dev.descriptors.borrow().is_empty());

        let (action, err) = ind.poll_and_apply(true, &dev);
        assert_eq!(action, MonitorAction::ApplyMute);
        assert!(err.is_none());
        assert_eq!(
            dev.notifies.borrow().as_slice(),
            &[NOTIFY_DIRECT_LED_COLOUR, NOTIFY_DIRECT_LED_COLOUR],
            "the input LED should be written only through the normal poll path"
        );
    }

    #[test]
    fn poll_and_apply_triggers_clear_mute() {
        let mut ind = make_indicator(true);
        let dev = MockDevice::new();

        // Set up selectedInput for restore
        dev.set_descriptor(OFF_SELECTED_INPUT, &[0]).unwrap();

        let (a1, _) = ind.poll_and_apply(false, &dev);
        assert_eq!(a1, MonitorAction::NoChange);
        let (a2, e2) = ind.poll_and_apply(false, &dev);
        assert_eq!(a2, MonitorAction::ClearMute);
        assert!(e2.is_none());
        assert!(!ind.is_muted());
    }

    #[test]
    fn poll_and_apply_full_cycle() {
        let mut ind = make_indicator(false);
        let dev = MockDevice::new();

        // Set up selectedInput for restore
        dev.set_descriptor(OFF_SELECTED_INPUT, &[0]).unwrap();

        // Mute (threshold=2)
        for _ in 0..2 {
            ind.poll_and_apply(true, &dev);
        }
        assert!(ind.is_muted());

        // Unmute (threshold=2)
        for _ in 0..2 {
            ind.poll_and_apply(false, &dev);
        }
        assert!(!ind.is_muted());
    }

    #[test]
    fn set_strategy_preserves_mute_state() {
        let mut ind = make_indicator(false);
        // Feed threshold polls to get muted
        assert_eq!(ind.update(true), MonitorAction::NoChange);
        assert_eq!(ind.update(true), MonitorAction::ApplyMute);
        assert!(ind.is_muted());

        // Switch strategy
        let new_strategy = led::MuteStrategy {
            input_indices: vec![0],
            number_leds: vec![0],
            mute_colors: vec![],
            selected_color: 0x20FF_0000,
            unselected_color: 0x88FF_FF00,
            offsets: Default::default(),
        };
        ind.set_strategy(new_strategy);
        assert!(
            ind.is_muted(),
            "mute state should be preserved after strategy switch"
        );
    }

    #[test]
    fn force_state_syncs_debouncer_to_muted() {
        let mut ind = make_indicator(false);
        assert!(!ind.is_muted());

        // Force to muted — debouncer should now consider mic muted
        ind.force_state(true);
        assert!(ind.is_muted());

        // Subsequent muted polls should return NoChange (already muted)
        assert_eq!(ind.update(true), MonitorAction::NoChange);
        assert_eq!(ind.update(true), MonitorAction::NoChange);
    }

    #[test]
    fn force_state_prevents_spurious_apply_mute() {
        let mut ind = make_indicator(false);

        // Simulate: mic is already muted at startup, force debouncer to match
        ind.force_state(true);

        // Now feed muted polls — should NOT trigger ApplyMute
        for _ in 0..5 {
            assert_eq!(ind.update(true), MonitorAction::NoChange);
        }

        // But unmuting should still work after debounce threshold
        assert_eq!(ind.update(false), MonitorAction::NoChange);
        assert_eq!(ind.update(false), MonitorAction::ClearMute);
        assert!(!ind.is_muted());
    }

    #[test]
    fn force_state_to_unmuted_prevents_spurious_clear_mute() {
        let mut ind = make_indicator(true); // start muted
        assert!(ind.is_muted());

        // Force to unmuted
        ind.force_state(false);
        assert!(!ind.is_muted());

        // Subsequent unmuted polls should NOT trigger ClearMute
        for _ in 0..5 {
            assert_eq!(ind.update(false), MonitorAction::NoChange);
        }
    }
}
