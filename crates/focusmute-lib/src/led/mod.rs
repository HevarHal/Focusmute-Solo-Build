//! LED control — single-LED update, mute indicator apply/clear/restore.
// Modified for the Scarlett Solo build; see README.md.

mod color;
mod ops;
mod strategy;

pub use color::{color_to_rgb, format_color, parse_color, rgb_to_hex};
pub use ops::{
    apply_mute_indicator, clear_mute_indicator, read_metering_gradient, refresh_after_reconnect,
    restore_on_exit, set_single_led, write_metering_gradient,
};
pub use strategy::{MuteStrategy, mute_color_or_default, resolve_strategy_from_config};
