//! Device context — resolves model profile, schema, offsets, and predicted layout.
//!
//! Consolidates the repeated resolution pattern used by CLI commands and the
//! tray app: detect profile → extract schema → compute offsets → predict layout.
// Modified for the Scarlett Solo build; see README.md.

use crate::device::{DeviceError, ScarlettDevice};
use crate::layout::{self, PredictedLayout};
use crate::models::{self, ModelProfile};
use crate::offsets::DeviceOffsets;
use crate::schema::{self, SchemaConstants};

/// Resolved device context with model profile, schema, offsets, and layout.
#[derive(Debug)]
pub struct DeviceContext {
    pub profile: Option<&'static ModelProfile>,
    pub schema: Option<SchemaConstants>,
    pub offsets: DeviceOffsets,
    pub predicted: Option<PredictedLayout>,
}

impl DeviceContext {
    /// Resolve context from a connected device.
    ///
    /// If `force_schema` is false (the common case), schema extraction is
    /// skipped when a hardcoded profile exists, except on the Solo where the
    /// metering-color gradient is needed for halo mute indication.
    ///
    /// Returns `Err(UnsupportedDevice)` if no profile exists and schema
    /// extraction also failed — the device cannot be operated safely.
    pub fn resolve(device: &impl ScarlettDevice, force_schema: bool) -> crate::error::Result<Self> {
        Self::resolve_at(device, force_schema, None)
    }

    /// Testable variant of [`resolve`](Self::resolve) with an explicit schema
    /// cache path, mirroring [`schema::extract_or_cached_at`]. `None` uses the
    /// real per-user cache.
    pub fn resolve_at(
        device: &impl ScarlettDevice,
        force_schema: bool,
        cache_path: Option<&std::path::Path>,
    ) -> crate::error::Result<Self> {
        let profile = models::detect_model(device.info().model());

        let needs_solo_gradient =
            profile.is_some_and(|p| p.name.eq_ignore_ascii_case("Scarlett Solo 4th Gen"));
        let schema = if force_schema || profile.is_none() || needs_solo_gradient {
            match cache_path {
                Some(path) => schema::extract_or_cached_at(device, path),
                None => schema::extract_or_cached(device),
            }
            .inspect_err(|e| log::warn!("[context] schema extraction failed: {e}"))
            .ok()
        } else {
            log::debug!("[context] skipping schema extraction (hardcoded profile exists)");
            None
        };

        let offsets = if let Some(ref sc) = schema {
            DeviceOffsets::from_schema(sc)
        } else if let Some(p) = profile {
            // The profile carries its own offsets. A shared default here would
            // hand every profiled model the 2i2's, which is wrong the moment a
            // second profile exists.
            p.offsets.clone()
        } else {
            return Err(DeviceError::UnsupportedDevice(device.info().model().to_string()).into());
        };

        // Predict whenever a schema is in hand, profile or not. The profile
        // still wins everywhere both exist (`resolve_mute_strategy`,
        // `input_count`); this is what `map` labels its panel walk from, and
        // what `--output-code` builds its skeleton from, on a model that
        // already has a profile.
        let predicted = schema.as_ref().and_then(|sc| {
            layout::predict_layout(sc)
                .inspect_err(|e| log::debug!("[context] layout prediction failed: {e}"))
                .ok()
        });

        Ok(DeviceContext {
            profile,
            schema,
            offsets,
            predicted,
        })
    }

    /// The effective input count from the best available source.
    pub fn input_count(&self) -> Option<usize> {
        self.profile
            .map(|p| p.input_count)
            .or_else(|| self.predicted.as_ref().map(|pl| pl.input_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::mock::MockDevice;

    /// A mock that serves the 2i2 test schema over the devmap commands, so
    /// `resolve_at` can actually extract one.
    fn mock_with_schema(name: &str) -> MockDevice {
        use crate::protocol::{
            CMD_GET_DEVMAP, CMD_INFO_DEVMAP, DEVMAP_PAGE_SIZE, DEVMAP_RESPONSE_SIZE,
        };
        let dev = mock_with_name(name);
        let raw = crate::schema::tests::encode_schema(&crate::schema::tests::test_schema_json());

        let mut info = vec![0u8; 8];
        info.extend_from_slice(&0u16.to_le_bytes());
        info.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        dev.add_transact_response(CMD_INFO_DEVMAP, info);

        for page in 0..raw.len().div_ceil(DEVMAP_PAGE_SIZE) {
            let start = page * DEVMAP_PAGE_SIZE;
            let end = (start + DEVMAP_PAGE_SIZE).min(raw.len());
            let mut resp = vec![0u8; 8];
            resp.extend_from_slice(&raw[start..end]);
            resp.resize(DEVMAP_RESPONSE_SIZE, 0);
            dev.add_transact_response(CMD_GET_DEVMAP, resp);
        }
        dev
    }

    fn mock_with_name(name: &str) -> MockDevice {
        let mut dev = MockDevice::new();
        dev.info_mut().device_name = name.into();
        dev
    }

    #[test]
    fn known_model_skips_schema() {
        let dev = mock_with_name("Scarlett 2i2 4th Gen-00031337");
        let ctx = DeviceContext::resolve(&dev, false).unwrap();
        assert!(ctx.profile.is_some());
        assert_eq!(ctx.profile.unwrap().name, "Scarlett 2i2 4th Gen");
        assert!(
            ctx.schema.is_none(),
            "schema should be skipped for known model"
        );
        assert!(
            ctx.predicted.is_none(),
            "predicted should be None when profile exists"
        );
    }

    #[test]
    fn known_model_force_schema_still_has_profile() {
        let dev = mock_with_name("Scarlett 2i2 4th Gen-00031337");
        let ctx = DeviceContext::resolve(&dev, true).unwrap();
        assert!(ctx.profile.is_some());
        // Schema extraction will fail on mock but profile is still detected
    }

    /// A Solo profile remains usable when schema extraction fails, but its
    /// schema-only metering gradient stays unavailable in that case.
    #[test]
    fn profile_without_schema_uses_that_profiles_offsets_not_the_2i2_default() {
        let dev = mock_with_name("Scarlett Solo 4th Gen-00031337");
        let ctx = DeviceContext::resolve(&dev, false).unwrap();
        assert!(
            ctx.schema.is_none(),
            "the mock device has no schema response"
        );

        let defaults = DeviceOffsets::default();
        assert_eq!(ctx.offsets.direct_led_colour, 80);
        assert_eq!(ctx.offsets.direct_led_index, 84);
        assert_eq!(ctx.offsets.enable_direct_led, 72);
        assert_eq!(ctx.offsets.selected_input, None);
        assert_eq!(ctx.offsets.metering_gradient, None);
        assert_ne!(ctx.offsets.direct_led_colour, defaults.direct_led_colour);
        assert_ne!(ctx.offsets.direct_led_index, defaults.direct_led_index);
    }

    #[test]
    fn known_verified_model_still_resolves_the_2i2_offsets() {
        let dev = mock_with_name("Scarlett 2i2 4th Gen-00031337");
        let ctx = DeviceContext::resolve(&dev, false).unwrap();
        let defaults = DeviceOffsets::default();
        assert_eq!(ctx.offsets.direct_led_colour, defaults.direct_led_colour);
        assert_eq!(ctx.offsets.direct_led_index, defaults.direct_led_index);
        assert_eq!(ctx.offsets.selected_input, defaults.selected_input);
    }

    /// `map` forces schema extraction so it can label the panel walk and print
    /// a profile skeleton. Gating prediction on the absence of a profile made
    /// both produce nothing for exactly the models most in need of mapping.
    #[test]
    fn forcing_the_schema_predicts_a_layout_even_when_a_profile_exists() {
        let dir = tempfile::tempdir().unwrap();
        let dev = mock_with_schema("Scarlett 2i2 4th Gen-00031337");
        let ctx =
            DeviceContext::resolve_at(&dev, true, Some(&dir.path().join("schema.json"))).unwrap();

        assert!(ctx.profile.is_some(), "the 2i2 profile should be detected");
        assert!(ctx.schema.is_some(), "force_schema should extract");
        let predicted = ctx
            .predicted
            .expect("a profiled device must still get a predicted layout when forced");
        assert_eq!(predicted.total_leds, 40);
        assert_eq!(predicted.input_count, 2);
    }

    /// Without a forced schema a profiled device short-circuits, so there is
    /// nothing to predict from.
    #[test]
    fn a_profiled_device_predicts_nothing_when_the_schema_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let dev = mock_with_schema("Scarlett 2i2 4th Gen-00031337");
        let ctx =
            DeviceContext::resolve_at(&dev, false, Some(&dir.path().join("schema.json"))).unwrap();
        assert!(ctx.schema.is_none());
        assert!(ctx.predicted.is_none());
    }

    #[test]
    fn unknown_model_no_schema_returns_error() {
        let dev = mock_with_name("Scarlett 4i4 4th Gen-00031337");
        let err = DeviceContext::resolve(&dev, false).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported device"),
            "expected UnsupportedDevice error, got: {err}"
        );
    }

    #[test]
    fn input_count_from_profile() {
        let dev = mock_with_name("Scarlett 2i2 4th Gen-00031337");
        let ctx = DeviceContext::resolve(&dev, false).unwrap();
        assert_eq!(ctx.input_count(), Some(2));
    }

    #[test]
    fn input_count_unknown_model_no_schema_is_err() {
        let dev = mock_with_name("Unknown Device-00031337");
        let err = DeviceContext::resolve(&dev, false);
        assert!(err.is_err(), "unknown device with no schema should be Err");
    }

    #[test]
    fn known_model_force_schema_fails_gracefully() {
        // When schema extraction fails on a known model with force_schema,
        // the context should still resolve with the hardcoded profile and
        // default offsets (schema = None).
        let dev = mock_with_name("Scarlett 2i2 4th Gen-00031337");
        let ctx = DeviceContext::resolve(&dev, true).unwrap();
        assert!(ctx.profile.is_some());
        assert!(
            ctx.schema.is_none(),
            "mock device can't provide schema data — should be None"
        );
        assert_eq!(ctx.input_count(), Some(2));
    }

    #[test]
    fn default_offsets_used_when_schema_unavailable() {
        let dev = mock_with_name("Scarlett 2i2 4th Gen-00031337");
        let ctx = DeviceContext::resolve(&dev, false).unwrap();
        // With no schema, offsets should be defaults (compare key fields)
        let defaults = DeviceOffsets::default();
        assert_eq!(
            ctx.offsets.enable_direct_led, defaults.enable_direct_led,
            "enable_direct_led offset should match default"
        );
    }
}
