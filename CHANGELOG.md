# Changelog

## 0.1.0

dps is a system-wide audio processor for macOS that needs no virtual audio driver: it levels, shapes and limits everything your Mac plays, with a profile for each output device, and the same chain processes audio files on any platform. Its core is a reimplementation of the "Digital Power Station" processing chain, rebuilt from the published patents; this project is not affiliated with or endorsed by Bongiovi Acoustics.

### Live mode
- Intercepts system audio with a Core Audio process tap, processes it and plays it on the default output device. Live mode is macOS only, needs macOS 14.2 or later, and needs your terminal allowed under Screen & System Audio Recording.
- States the latency Core Audio reports for the session when it starts, and `dps status` shows it.
- `dps start` runs live in the background (`dps status`, `dps stop`), and `dps start --login` also starts it at every login.
- Settings live in `~/.config/dps/profile.toml` (or under `$XDG_CONFIG_HOME`), and saving the file while live mode runs applies it within a moment. Each output device can have a profile of its own, and live mode switches to it when the output changes, for example when you plug in headphones.
- `exclude` in `config.toml` lists apps, by bundle ID, whose audio is left out of the tap and plays unprocessed.

### Processing chain
- The patent chain: low shelf cut, high shelf boost, compressor, mirrored shelves, 11-band parametric EQ, compressor and output gain, followed by the patent's stereo field enhancement, which is off by default.
- A leveler that brings every source to one loudness, so the compressors work the same on quiet and loud material. Off by default.
- Bass harmonics, to make bass audible on speakers too small to play it; crossfeed, for headphones; and a contour that adds bass and treble as the system volume goes down, in live mode. All off by default.
- Device correction, a measured correction for the output device kept apart from your EQ, applied last before the limiter. Off by default.
- A peak limiter with lookahead and true-peak detection, so no peak goes above the ceiling.
- Five built-in presets: `headphones`, `laptop-speakers`, `music`, `night` and `voice`. Each starts with a leveler aimed at −14 LUFS (−16 for `voice` and `night`). `night` is for low volume late at night: a programme swinging 20 LU between quiet and loud passages comes out 3.4 LU apart.

### Tuner
- In a terminal, live mode shows every setting, with meters for input and output loudness, the leveler's gain, and how much each compressor and the limiter are taking off. `g` shows the response curve of the EQ and the device correction.
- Settings change from the keyboard, with a fine step on Shift. `s` saves to the settings file and `r` reverts to the last saved settings.
- Two sets of settings, A and B, switch with `a`, and `c` copies the one you hear into the other. With level matching on, the louder is turned down to the other's loudness.
- `x` starts a blind test: the two sets play as X and Y in random order, each vote draws the order again, and the end shows whether the split could be chance.

### Commands
- `dps devices` lists the outputs and the profile each plays with; `dps devices assign` gives one a profile of its own, and `unassign` takes it away.
- `dps import` reads a headphone correction in the parametric EQ format the AutoEQ project publishes (Equalizer APO).
- `dps calibrate` plays a quiet warble tone for each third of an octave from 100 Hz to 8 kHz, records it with the default input device, and fits up to 8 filters that flatten what it heard, with at most 6 dB of boost. `--dry-run` only prints the result.
- `dps get` and `dps set` read and change one profile value by its path in the TOML, for scripts. A value out of range changes nothing.
- `dps bypass` and `dps use` control a running dps: bypass matched to the processed level, and a preset or profile file played until that run ends.
- `dps doctor` checks that everything live mode needs is in place and lists the bundle IDs of the apps playing audio now. `dps presets` lists the built-in presets, and `dps profile` prints the default profile as TOML.

### Files
- `dps process song.flac` writes `song.dps.flac`. WAV is read directly; any other format goes through `ffmpeg`, keeping tags, cover art, codec and bit depth where the container allows.
- The output keeps the input's length and alignment, whatever delay the limiter's lookahead adds. File processing builds on other platforms.

### Safety
- Every output sample is clamped to ±1.0 after the limiter.
- A non-finite sample silences the output and stops the run.
- Only one live run at a time, foreground or background.
- Ctrl-C, errors and a killed process all leave system audio as it was.

### Known limits
- The patents publish no tuning values, so the default profile and the presets are this project's own starting points and will not sound like the commercial product.
- Output devices that also have inputs (headsets, USB interfaces) are covered by unit tests only, and start at login was checked through launchd's state, not an actual logout.
- macOS below 14.2 is untested, and the binary may not launch there.
- Decoded audio above 4 GiB is not supported.
- The leveler holds its gain for 5 s after a sudden drop in level, so material that alternates between quiet and loud every few seconds comes out with a wider gap than it went in with.
- The contour needs an output device with a volume control; others, and files, get nothing from it.
