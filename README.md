# dps

A command-line tool that runs audio through a reimplementation of the
"Digital Power Station" processing chain: on files, or live on everything your
Mac plays.

This project is not affiliated with or endorsed by Bongiovi Acoustics. There is
no public source for their product, so the chain here is rebuilt from the
published patents (the US 8,284,955 family, and US 9,398,394 for the stereo
block). The patents publish no tuning values, so the default profile and the
presets are this project's own starting points and will not sound like the
commercial product. The MIT license covers this code only; it grants no rights
under anyone's patents.

## What it does

```
input gain → low shelf cut → high shelf boost → compressor
           → mirrored shelves → 11-band parametric EQ → compressor
           → output gain → stereo field enhancement → peak limiter
```

- **Live:** intercepts system audio with a Core Audio process tap, processes it
  and plays it on the default output device. No virtual audio driver. Adds
  about 25 ms of latency.
- **Files:** WAV directly; any other format through `ffmpeg`, keeping tags,
  cover art, codec and bit depth where the container allows.

## Requirements

- Rust 1.95 or later to build.
- Live mode: macOS 14.2 or later, and your terminal allowed under System
  Settings > Privacy & Security > Screen & System Audio Recording.
- `ffmpeg` and `ffprobe` on `PATH` for files other than WAV.

File processing builds on other platforms; live mode is macOS only.

## Install

```sh
cargo install --path .
```

On macOS, the background service (`dps start`) is asked for audio-recording
consent as `dps` itself, and macOS ties that consent to the code signature. An
unsigned build gets a new signature on every rebuild, so the prompt returns
each time. `scripts/install.sh` installs and then signs the binary with a
Developer ID certificate from your keychain, which makes one approval last.

Run `dps doctor` to check that everything live mode needs is in place.

## Use

```sh
dps                       # process system audio live with your settings file
dps --preset music        # the same, starting from a built-in preset
dps process song.flac     # write song.dps.flac
dps start                 # run live in the background (dps status, dps stop)
dps start --login         # also start at every login
dps presets               # list the built-in presets
dps profile               # print the default profile as TOML
```

### Tuning while it plays

In a terminal, live mode shows every setting.

| Key | Action |
|---|---|
| Up / Down, `k` / `j` | select a setting |
| Left / Right, `h` / `l` | change it |
| Shift + Left / Right, `H` / `L` | change it by a fine step |
| Tab, Shift + Tab | on an EQ band: switch between gain, frequency and Q |
| Space | toggle an on/off setting |
| `b` | bypass, matched to the processed level (total of left and right) |
| `s` | save to the settings file |
| `r` | revert to the last saved settings |
| `q` | quit |

Changes crossfade over 30 ms. Saving rewrites the settings file, so comments
in a hand-edited file are not kept.

### Settings

Settings live in `~/.config/dps/profile.toml` (or under `$XDG_CONFIG_HOME`),
created with the default profile on first use. While live mode runs, saving
the file applies it within a moment. `--profile FILE` or `--preset NAME` use
something else for one run.

A profile is plain TOML; `dps profile` prints every key. The EQ is three lists
of eleven values, one position per band:

```toml
[eq]
freqs_hz = [30.0, 56.4, 106.0, 199.0, 375.0, 705.0, 1327.0, 2497.0, 4698.0, 8839.0, 16000.0]
q        = [1.5, 1.5, 1.5, 1.5, 1.5, 1.5, 1.5, 1.5, 1.5, 1.5, 1.5]
gains_db = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]
```

A band at or above half the sample rate is skipped rather than rejected.

### Presets

`headphones`, `laptop-speakers`, `music` and `voice`. They were set by
measurement on test signals, not by listening tests. With the limiter on, a
mix at −14 LUFS comes out about as loud as it went in; loud masters come out a
few LU quieter because the compressors turn loud material down most.

## Safety in live mode

- Every output sample is clamped to ±1.0 after the limiter.
- A non-finite sample silences the output and stops the run.
- Only one live run at a time, foreground or background.
- Ctrl-C, errors and a killed process all leave system audio as it was.

## Known limits

- Output devices that also have inputs (headsets, USB interfaces) are covered
  by unit tests only.
- Start at login was checked through launchd's state, not an actual logout.
- macOS below 14.2 is untested, and the binary may not launch there.
- Decoded audio above 4 GiB is not supported.

## Development

```sh
cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings
```

The tests never touch the real settings file, launchd or audio devices.
`docs/` holds the specs extracted from the patents, including the errors in
the printed equations and the corrected forms the code uses.

## License

MIT. See [LICENSE](LICENSE).
