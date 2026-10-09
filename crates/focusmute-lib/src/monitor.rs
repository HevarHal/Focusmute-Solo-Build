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
use crate::offsets::MeteringGradientOffsets;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

const SOLO_DIRECT_LED_INDICES: [u8; 2] = [27, 31];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SoloLedRecovery {
    version: u8,
    serial: String,
    gradient: Option<MeteringGradientOffsets>,
    gradient_bytes: Option<Vec<u8>>,
    led_colors: Vec<(u8, u32)>,
}

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
    saved_solo_state: RefCell<Option<SoloLedRecovery>>,
    solo_recovery_path: Option<PathBuf>,
    solo_serial: Option<String>,
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
            saved_solo_state: RefCell::new(None),
            solo_recovery_path: None,
            solo_serial: None,
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
        if self.solo_direct_leds {
            self.save_solo_led_state(device)?;
        }
        led::apply_mute_indicator(device, &self.strategy, self.mute_color)?;
        if self.solo_direct_leds {
            for index in SOLO_DIRECT_LED_INDICES {
                led::set_single_led(device, &self.strategy.offsets, index, self.mute_color)?;
            }
            if let Some(state) = self.saved_solo_state.borrow().as_ref()
                && let (Some(gradient), Some(original)) =
                    (state.gradient, state.gradient_bytes.as_ref())
            {
                let mut colors = original.clone();
                for color in colors.chunks_exact_mut(std::mem::size_of::<u32>()).skip(1) {
                    color.copy_from_slice(&self.mute_color.to_le_bytes());
                }
                led::write_metering_gradient(device, gradient, &colors)?;
            }
        }
        Ok(())
    }

    /// Clear the mute indicator and restore normal LED state.
    pub fn clear_mute(&self, device: &impl ScarlettDevice) -> Result<()> {
        if self.solo_direct_leds {
            if self.saved_solo_state.borrow().is_some() {
                return self.restore_saved_solo_led_state(device);
            }
        }
        led::clear_mute_indicator(device, &self.strategy)
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

    /// Configure durable Solo LED recovery for this device instance.
    pub fn set_solo_recovery_dir(&mut self, directory: &Path, serial: Option<&str>) -> Result<()> {
        let serial = serial.filter(|serial| !serial.is_empty()).ok_or_else(|| {
            crate::device::DeviceError::TransactFailed(
                "Solo serial unavailable; durable LED recovery cannot be configured".into(),
            )
        })?;
        let encoded_serial: String = serial.bytes().map(|byte| format!("{byte:02x}")).collect();
        self.solo_serial = Some(serial.to_owned());
        self.solo_recovery_path =
            Some(directory.join(format!("solo-led-recovery-{encoded_serial}.json")));
        Ok(())
    }

    /// Restore a pending persisted Solo LED state left by an interrupted run.
    ///
    /// Call after resolving this device's offsets and before applying the
    /// startup mute state.
    pub fn recover_solo_led_state(&self, device: &impl ScarlettDevice) -> Result<bool> {
        let Some(path) = self.solo_recovery_path.as_deref() else {
            return Ok(false);
        };
        let state = match read_solo_recovery(path)? {
            Some(state) => state,
            None => return Ok(false),
        };
        self.validate_solo_recovery(&state)?;
        restore_solo_state(device, &self.strategy, &state)?;
        remove_solo_recovery(path)?;
        log::info!("[led] recovered and restored Solo LED state from previous run");
        Ok(true)
    }

    /// Restore the saved mute-time Solo state on normal application exit.
    pub fn restore_solo_led_state_on_exit(&self, device: &impl ScarlettDevice) -> Result<()> {
        if self.solo_direct_leds {
            if self.saved_solo_state.borrow().is_some() {
                self.restore_saved_solo_led_state(device)
            } else if self.is_muted() {
                self.clear_mute(device)
            } else {
                Ok(())
            }
        } else {
            self.clear_mute(device)
        }
    }

    fn save_solo_led_state(&self, device: &impl ScarlettDevice) -> Result<()> {
        if self.saved_solo_state.borrow().is_some() {
            return Ok(());
        }
        let path = self.solo_recovery_path.as_deref().ok_or_else(|| {
            crate::device::DeviceError::TransactFailed(
                "Solo LED recovery path unavailable; refusing to change LEDs".into(),
            )
        })?;
        let serial = self.solo_serial.as_ref().ok_or_else(|| {
            crate::device::DeviceError::TransactFailed(
                "Solo serial unavailable; refusing to change LEDs without durable recovery".into(),
            )
        })?;
        let gradient = self.strategy.offsets.metering_gradient;
        let gradient_bytes = gradient
            .map(|gradient| led::read_metering_gradient(device, gradient))
            .transpose()?;
        // Solo number LEDs are firmware-driven and not readable, so these
        // indicators are restored to their known normal color.
        let mut led_colors: Vec<(u8, u32)> = self
            .strategy
            .number_leds
            .iter()
            .copied()
            .map(|index| (index, self.strategy.unselected_color))
            .collect();
        led_colors.extend(
            SOLO_DIRECT_LED_INDICES
                .into_iter()
                .map(|index| (index, self.strategy.unselected_color)),
        );
        let state = SoloLedRecovery {
            version: 1,
            serial: serial.clone(),
            gradient,
            gradient_bytes,
            led_colors,
        };
        write_solo_recovery(path, &state)?;
        *self.saved_solo_state.borrow_mut() = Some(state);
        Ok(())
    }

    fn validate_solo_recovery(&self, state: &SoloLedRecovery) -> Result<()> {
        if state.version != 1 || self.solo_serial.as_deref() != Some(state.serial.as_str()) {
            return Err(crate::device::DeviceError::TransactFailed(
                "Solo LED recovery record version or device serial does not match".into(),
            ));
        }
        if state.gradient != self.strategy.offsets.metering_gradient {
            return Err(crate::device::DeviceError::TransactFailed(
                "Solo LED recovery record gradient layout does not match this device".into(),
            ));
        }
        let mut expected_led_colors: Vec<(u8, u32)> = self
            .strategy
            .number_leds
            .iter()
            .copied()
            .map(|index| (index, self.strategy.unselected_color))
            .collect();
        expected_led_colors.extend(
            SOLO_DIRECT_LED_INDICES
                .into_iter()
                .map(|index| (index, self.strategy.unselected_color)),
        );
        if state.led_colors != expected_led_colors {
            return Err(crate::device::DeviceError::TransactFailed(
                "Solo LED recovery record contains unexpected LED targets".into(),
            ));
        }
        if let (Some(gradient), Some(bytes)) = (state.gradient, state.gradient_bytes.as_ref()) {
            if bytes.len() != gradient.count.saturating_mul(std::mem::size_of::<u32>()) {
                return Err(crate::device::DeviceError::TransactFailed(
                    "Solo LED recovery record has an invalid gradient length".into(),
                ));
            }
        } else if state.gradient.is_some() != state.gradient_bytes.is_some() {
            return Err(crate::device::DeviceError::TransactFailed(
                "Solo LED recovery record is missing gradient data".into(),
            ));
        }
        Ok(())
    }

    fn restore_saved_solo_led_state(&self, device: &impl ScarlettDevice) -> Result<()> {
        let state = self.saved_solo_state.borrow().clone();
        let Some(state) = state else {
            return Ok(());
        };
        restore_solo_state(device, &self.strategy, &state)?;
        let path = self.solo_recovery_path.as_deref().ok_or_else(|| {
            crate::device::DeviceError::TransactFailed(
                "Solo LED recovery path unavailable during restore".into(),
            )
        })?;
        remove_solo_recovery(path)?;
        *self.saved_solo_state.borrow_mut() = None;
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

fn read_solo_recovery(path: &Path) -> Result<Option<SoloLedRecovery>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(crate::device::DeviceError::TransactFailed(format!(
                "could not read Solo LED recovery file: {error}"
            )));
        }
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        crate::device::DeviceError::TransactFailed(format!(
            "could not parse Solo LED recovery file: {error}"
        ))
    })
}

fn write_solo_recovery(path: &Path, state: &SoloLedRecovery) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().ok_or_else(|| {
        crate::device::DeviceError::TransactFailed(
            "Solo LED recovery file has no parent directory".into(),
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        crate::device::DeviceError::TransactFailed(format!(
            "could not create Solo LED recovery directory: {error}"
        ))
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        crate::device::DeviceError::TransactFailed("Solo LED recovery file has no file name".into())
    })?;
    let temp_path = path.with_file_name(format!(
        "{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let bytes = serde_json::to_vec(state).map_err(|error| {
        crate::device::DeviceError::TransactFailed(format!(
            "could not serialize Solo LED recovery state: {error}"
        ))
    })?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|error| {
            crate::device::DeviceError::TransactFailed(format!(
                "could not create temporary Solo LED recovery file: {error}"
            ))
        })?;
    let result = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temp_path, path));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(crate::device::DeviceError::TransactFailed(format!(
            "could not persist Solo LED recovery state: {error}"
        )));
    }
    Ok(())
}

fn remove_solo_recovery(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::device::DeviceError::TransactFailed(format!(
            "could not remove restored Solo LED recovery file: {error}"
        ))),
    }
}

fn restore_solo_state(
    device: &impl ScarlettDevice,
    strategy: &led::MuteStrategy,
    state: &SoloLedRecovery,
) -> Result<()> {
    let mut errors = Vec::new();
    if let (Some(gradient), Some(bytes)) = (state.gradient, state.gradient_bytes.as_ref())
        && let Err(error) = led::write_metering_gradient(device, gradient, bytes)
    {
        errors.push(format!("halo gradient: {error}"));
    }
    for &(index, color) in &state.led_colors {
        if let Err(error) = led::set_single_led(device, &strategy.offsets, index, color) {
            errors.push(format!("LED {index}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(crate::device::DeviceError::TransactFailed(format!(
            "could not restore Solo LED state: {}",
            errors.join("; ")
        )))
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
        let recovery_dir = tempfile::tempdir().unwrap();
        indicator
            .set_solo_recovery_dir(recovery_dir.path(), Some("MOCK123"))
            .unwrap();
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
            std::fs::read_dir(recovery_dir.path()).unwrap().count(),
            0,
            "successful restoration should remove its recovery record"
        );
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
    fn persisted_solo_led_state_is_recovered_after_restart() {
        let recovery_dir = tempfile::tempdir().unwrap();
        let device = MockDevice::new();
        let original_gradient: Vec<u8> = (0..44).map(|byte| byte as u8).collect();
        device
            .set_descriptor(384, &original_gradient)
            .expect("seed original gradient");

        let mut first_run = make_indicator(false);
        first_run.set_solo_direct_leds(true);
        first_run
            .set_solo_recovery_dir(recovery_dir.path(), Some("MOCK123"))
            .unwrap();
        first_run.apply_mute(&device).unwrap();
        assert_eq!(
            std::fs::read_dir(recovery_dir.path()).unwrap().count(),
            1,
            "recovery data must be durable while mute LEDs are applied"
        );
        drop(first_run);

        let mut restarted = make_indicator(false);
        restarted.set_solo_direct_leds(true);
        restarted
            .set_solo_recovery_dir(recovery_dir.path(), Some("MOCK123"))
            .unwrap();
        assert!(restarted.recover_solo_led_state(&device).unwrap());
        assert_eq!(
            device.descriptors.borrow().get(&384),
            Some(&original_gradient)
        );
        assert_eq!(
            std::fs::read_dir(recovery_dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>(),
            Vec::<std::path::PathBuf>::new(),
            "recovery record should be removed only after restoration"
        );
    }

    #[test]
    fn abrupt_restart_restores_solo_gradient_and_indicator_colors() {
        let recovery_dir = tempfile::tempdir().unwrap();
        let device = MockDevice::new();
        let original_gradient: Vec<u8> = (0..44).map(|byte| byte as u8).collect();
        device
            .set_descriptor(384, &original_gradient)
            .expect("seed original Solo gradient");

        let strategy = led::MuteStrategy {
            input_indices: vec![0, 1],
            number_leds: vec![4, 12],
            mute_colors: vec![],
            selected_color: 0xAAFF_DD00,
            unselected_color: 0xAAFF_DD00,
            offsets: crate::offsets::DeviceOffsets {
                enable_direct_led: 72,
                direct_led_values: 88,
                direct_led_count: 32,
                direct_led_notify: 5,
                direct_led_colour: 80,
                direct_led_index: 84,
                direct_led_colour_notify: 8,
                selected_input: None,
                metering_gradient: Some(MeteringGradientOffsets {
                    offset: 384,
                    count: 11,
                    notify: 9,
                }),
            },
        };
        let mut first_run = MuteIndicator::new(2, false, 0xFF00_0000, strategy.clone());
        first_run.set_solo_direct_leds(true);
        first_run
            .set_solo_recovery_dir(recovery_dir.path(), Some("MOCK123"))
            .unwrap();
        first_run.apply_mute(&device).unwrap();
        assert_eq!(
            device.descriptors.borrow().get(&384).unwrap(),
            &[
                original_gradient[..4].to_vec(),
                vec![0, 0, 0, 255].repeat(10)
            ]
            .concat(),
            "first run should apply the mute color to the Solo halos"
        );
        assert_eq!(std::fs::read_dir(recovery_dir.path()).unwrap().count(), 1);
        drop(first_run);

        let mut restarted = MuteIndicator::new(2, false, 0xFF00_0000, strategy);
        restarted.set_solo_direct_leds(true);
        restarted
            .set_solo_recovery_dir(recovery_dir.path(), Some("MOCK123"))
            .unwrap();
        assert!(restarted.recover_solo_led_state(&device).unwrap());

        let descriptors = device.descriptors.borrow();
        assert_eq!(
            descriptors.get(&384),
            Some(&original_gradient),
            "restart recovery should restore the exact original halo gradient"
        );
        assert_eq!(
            descriptors.get(&80),
            Some(&0xAAFF_DD00u32.to_le_bytes().to_vec())
        );
        assert_eq!(descriptors.get(&84), Some(&vec![31]));
        drop(descriptors);
        assert_eq!(
            device
                .notifies
                .borrow()
                .iter()
                .filter(|&&event| event == 8)
                .count(),
            8,
            "mute and recovery should each write both number and Direct indicators"
        );
        assert_eq!(
            device
                .notifies
                .borrow()
                .iter()
                .filter(|&&event| event == 9)
                .count(),
            2,
            "mute and recovery should each activate the halo gradient"
        );
        assert_eq!(std::fs::read_dir(recovery_dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn exit_restores_persisted_state_even_when_mute_tracker_is_live() {
        let recovery_dir = tempfile::tempdir().unwrap();
        let device = MockDevice::new();
        let original_gradient: Vec<u8> = (0..44).map(|byte| byte as u8).collect();
        device
            .set_descriptor(384, &original_gradient)
            .expect("seed original gradient");

        let mut indicator = make_indicator(false);
        indicator.set_solo_direct_leds(true);
        indicator
            .set_solo_recovery_dir(recovery_dir.path(), Some("MOCK123"))
            .unwrap();
        indicator.apply_mute(&device).unwrap();

        assert!(!indicator.is_muted());
        indicator.restore_solo_led_state_on_exit(&device).unwrap();
        assert_eq!(
            device.descriptors.borrow().get(&384),
            Some(&original_gradient)
        );
        assert_eq!(std::fs::read_dir(recovery_dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn refuses_to_mute_solo_leds_without_serial_for_recovery() {
        let mut indicator = make_indicator(false);
        indicator.set_solo_direct_leds(true);
        let device = MockDevice::new();

        assert!(indicator.apply_mute(&device).is_err());
        assert!(device.descriptors.borrow().is_empty());
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
