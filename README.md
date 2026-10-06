# FocusMute — Scarlett Solo Build

This is a Solo-focused fork of [FocusMute](https://github.com/barnumbirr/focusmute). For general features, configuration, installation, and build instructions, see the [upstream README](https://github.com/barnumbirr/focusmute#readme).

## What this build adds

- **Direct button mute (Windows, Scarlett Solo 4th Gen):** Pressing Direct toggles the computer's default microphone mute. The Direct LEDs follow the mute color and return to their normal color on unmute.
- **Solo mute LEDs:** Both input number indicators follow the mute state. Active halo meter segments use the configured mute color while muted; FocusMute restores the original metering gradient on unmute. Halos remain meter-driven, so idle segments may be dark, and the gradient may also affect the output halo.
- **Confirmed Solo map:** Input number LEDs are 4 and 12; halo segments are 5–11 and 13–19; Direct LED segments are 27 and 31. The map was confirmed on the project owner's device; other firmware/device combinations are unverified.
- **Build defaults:** Sound feedback off, mute/unmute sound volumes 0.185/0.36, and start-on-login on. Existing user configuration is not overwritten.

## Download

On Windows, download the `focusmute-*-windows-x86_64.zip` release and extract it. The archive contains `focusmute.exe`, `focusmute-cli.exe`, this README, and a copy of [`LICENSE`](./LICENSE). Keep the license with the executables when redistributing them. Direct-button integration is Solo-only; other FocusMute behavior follows upstream.

## License and notices

This fork is distributed under the [Apache License 2.0](./LICENSE). Modified source and documentation files carry concise notices identifying the Solo-build changes. The original project and its notices are preserved where applicable.
