# dps

[![CI](https://github.com/chauduyphanvu/dps-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/chauduyphanvu/dps-rs/actions/workflows/ci.yml)

A system-wide audio processor for macOS that needs no virtual audio driver:
it levels, shapes and limits everything your Mac plays, with a profile for
each output device. The same chain processes audio files on any platform.

The core of the chain is a reimplementation of the "Digital Power Station"
processing chain. This project is not affiliated with or endorsed by Bongiovi
Acoustics. There is no public source for their product, so that core is
rebuilt from the published patents (the US 8,284,955 family, and US 9,398,394
for the stereo block). The patents publish no tuning values, so the default
profile and the presets are this project's own starting points and will not
sound like the commercial product. The leveler, bass harmonics, contour,
crossfeed, device correction and limiter are this project's own additions.
The MIT license covers this code only; it grants no rights under anyone's
patents.

## What it does

```
leveler → input gain → low shelf cut → high shelf boost → compressor
        → mirrored shelves → 11-band parametric EQ → compressor → output gain
        → bass harmonics → contour → stereo field enhancement → crossfeed
        → device correction → peak limiter
```

| Stage | What it is for | Off by default |
|---|---|---|
| Leveler | Brings every source to one loudness, so the compressors work the same on quiet and loud material | yes |
| Shelves, compressors, EQ, output gain | The patent chain | |
| Bass harmonics | Makes bass audible on speakers too small to play it | yes |
| Contour | Adds bass and treble as the system volume goes down (live only) | yes |
| Stereo field enhancement | The patent's widening | yes |
| Crossfeed | Feeds each ear some of the other channel, for headphones | yes |
| Device correction | A measured correction for the output device, kept apart from your EQ | yes |
| Limiter | No peak above the ceiling, with lookahead and true-peak detection | |

- **Live:** intercepts system audio with a Core Audio process tap, processes it
  and plays it on the default output device. No virtual audio driver. When it
  starts it states the latency Core Audio reports for the session: 41 ms on a
  MacBook Pro's speakers at 48 kHz, of which 17 ms is the speakers' own and
  1.7 ms the limiter's lookahead.
- **Files:** WAV directly; any other format through `ffmpeg`, keeping tags,
  cover art, codec and bit depth where the container allows.

## Requirements

- Rust 1.95 or later to build.
- Live mode: macOS 14.2 or later, and your terminal allowed under System
  Settings > Privacy & Security > Screen & System Audio Recording.
- `ffmpeg` and `ffprobe` on `PATH` for files other than WAV.
- `dps calibrate`: a microphone, and your terminal allowed under Privacy &
  Security > Microphone.

File processing builds on other platforms; live mode is macOS only.

## Install

**Homebrew**, on macOS 14 or later:

```sh
brew install chauduyphanvu/dps/dps
```

**From a release**: each [release](https://github.com/chauduyphanvu/dps-rs/releases)
has `dps-<version>-macos-universal.zip`, holding one `dps` binary for Apple
silicon and Intel Macs, signed and notarised, and a `.sha256` file to check it
against. Set `v` to the release you want:

```sh
v=0.1.0
base=https://github.com/chauduyphanvu/dps-rs/releases/download/v$v
curl -fL -O "$base/dps-$v-macos-universal.zip" -O "$base/dps-$v-macos-universal.zip.sha256"
shasum -a 256 -c "dps-$v-macos-universal.zip.sha256"
unzip "dps-$v-macos-universal.zip"
sudo install -d /usr/local/bin && sudo install dps /usr/local/bin/
```

`/usr/local/bin` is on the `PATH` macOS sets up; any other directory on your
`PATH` will do.

**From source**, on any platform (live mode is macOS only):

```sh
cargo install --git https://github.com/chauduyphanvu/dps-rs --locked
```

or `cargo install --path . --locked` in a clone.

On macOS, the background service (`dps start`) is asked for audio-recording
consent as `dps` itself, and macOS ties that consent to the code signature. The
Homebrew and release binaries are signed with the same Developer ID every
time, so one approval lasts across upgrades. A build from source gets a new
signature on every rebuild, so the prompt returns each time, unless you install
it from a clone with `scripts/install.sh`, which signs it with a Developer ID
certificate from your keychain. Switching between a release and your own build
asks again.

Run `dps doctor` to check that everything live mode needs is in place.

## Use

```sh
dps                       # process system audio live with your settings file
dps --preset music        # the same, starting from a built-in preset
dps process song.flac     # write song.dps.flac
dps start                 # run live in the background (dps status, dps stop)
dps start --login         # also start at every login
dps devices               # list the outputs and the profile each plays with
dps import hd650.txt      # import a headphone correction
dps calibrate             # measure your speakers and correct them
dps set eq.gains_db[3] --by +2   # change one setting from a script
dps bypass                # toggle bypass in the running dps
dps presets               # list the built-in presets
dps profile               # print the default profile as TOML
```

### Tuning while it plays

In a terminal, live mode shows every setting, with meters above them: input
and output loudness, the leveler's gain, and how much each compressor and the
limiter are taking off.

| Key | Action |
|---|---|
| Up / Down, `k` / `j` | select a setting |
| Left / Right, `h` / `l` | change it |
| Shift + Left / Right, `H` / `L` | change it by a fine step |
| Tab, Shift + Tab | on an EQ band: switch between gain, frequency and Q |
| Space | toggle an on/off setting |
| `b` | bypass, matched to the processed level (total of left and right) |
| `g` | show or hide the response curve of the EQ and the device correction |
| `a` | switch between A and B |
| `c` | copy the slot you hear into the other |
| `p` | load the next built-in preset into the slot you hear, not saved |
| `m` | level matching between A and B on or off (on to begin with) |
| `x` | start a blind test; `x` or Esc ends it and shows the tally |
| `s` | save the slot you hear to the settings file |
| `r` | revert it to the last saved settings |
| `q` | quit |

Changes crossfade over 30 ms. Saving rewrites the settings file, so comments
in a hand-edited file are not kept.

**A and B.** The tuner holds two sets of settings. B starts as what was last
saved, so `a` compares your edits with it. With level matching on, the louder
of the two is turned down to the other's loudness, since louder tends to win
a comparison.

**Blind test.** `x` hides the settings and calls the two X and Y, drawn at
random. `a` or Tab switches between them, `1` or `2` votes for X or Y, and
each vote draws the order again. Ending the test shows how often each was
preferred and whether a split that lopsided could be chance.

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

### Scripting

`dps get` and `dps set` read and change one value of the profile; a running
dps picks a change up as it does any save.

```sh
dps get eq.gains_db[3]            # one value; a table or a list prints as TOML
dps set eq.gains_db[3] --by +2    # add to a number (--by -1.5 works too)
dps set leveler.target_lufs -16   # a number, true/false or on/off
```

Keys are paths into the profile's TOML (`dps profile` prints every key), with
`[N]` for a position in a list. A value out of range changes nothing; otherwise
the file is rewritten, so its comments are not kept. A running dps, in a
terminal or from `dps start`, also takes:

```sh
dps bypass            # toggle the level-matched bypass (or: on, off)
dps use night         # play a preset or profile file until that run ends
dps use --settings    # back to the settings file and per-device profiles
```

### A profile for each output

Speakers and headphones want different settings. `dps devices` lists the
outputs; `dps devices assign` gives the default output (or `--device NAME`) a
profile file of its own, a copy of what it plays with now, or `--preset NAME`.
Live mode with the settings file then follows the output: when you plug in
headphones it switches to their profile, and the tuner edits and saves that
file. Unsaved changes are dropped at a switch, and the message says so.

The assignments are kept in `config.toml` beside the settings file, which also
lists apps to leave alone:

```toml
# Bundle IDs whose audio is not processed. An entry also covers IDs that
# continue it after a dot, such as a browser's helper processes.
exclude = ["us.zoom.xos", "com.apple.FaceTime"]

[devices."BuiltInSpeakerDevice"]
name = "MacBook Pro Speakers"
profile = "devices/macbook-pro-speakers.toml"
```

`dps doctor` lists the bundle IDs of the apps playing audio now.

### Correcting the output device

The `[correction]` table holds a measured correction for the device, applied
last before the limiter and kept apart from the EQ.

- **Headphones:** `dps import ParametricEQ.txt` reads the parametric EQ files
  the AutoEQ project publishes for each headphone model (Equalizer APO
  format).
- **Speakers:** `dps calibrate` plays a quiet warble tone for each third of an
  octave from 100 Hz to 8 kHz, records it with the default input device, and
  fits up to 8 filters that flatten what it heard, with at most 6 dB of boost.
  It takes about 35 s. Put the microphone where you listen. `--level` sets how
  loud the tones are (−30 dBFS unless you say otherwise) and `--dry-run` only
  prints the result. It corrects the speakers as that microphone hears them at
  that position.

Both write to the profile the current output device uses, or to `--profile`.

### Presets

`headphones`, `laptop-speakers`, `music`, `night` and `voice`. They were set by
measurement on test signals, not by listening tests. Each starts with a leveler
that steers its input to a target loudness (−14 LUFS; −16 for `voice` and
`night`), so once it has settled, after a few seconds, a mix at −20 to −4 LUFS
comes out within 1 LU of a −14 LUFS one, and one at −30 LUFS within 2 LU.
`laptop-speakers` adds bass harmonics, `headphones` crossfeed, and `headphones`,
`music` and `night` a contour that follows the system volume in live mode.
`night` is for low volume late at night: a programme swinging 20 LU between
quiet and loud passages comes out 3.4 LU apart. A limiter keeps every peak at
or below −1 dBFS (−2 for `night`).

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
- The leveler holds its gain for 5 s after a sudden drop in level, so material
  that alternates between quiet and loud every few seconds comes out with a
  wider gap than it went in with. `night` avoids this by never cutting.
- The contour needs an output device with a volume control; others, and
  files, get nothing from it.

## Development

```sh
cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings
cargo run --release --example bench    # what the chain costs, stage by stage
```

The tests never touch the real settings file, launchd or audio devices.
`docs/` holds the specs extracted from the patents, including the errors in
the printed equations and the corrected forms the code uses.

With every stage on, the chain takes about 1.5 % of one core at 48 kHz on an
Apple silicon Mac; the default profile about 0.5 %.

### Releasing

Pushing a tag `vX.Y.Z` builds, signs, notarises and publishes the release and
updates the Homebrew tap, through `.github/workflows/release.yml`.

One-time setup. The secrets live in 1Password and reach the keychain or GitHub
through `op read`, so none is pasted or left in a file; `<vault>/<item>/<field>`
stands for wherever you keep each one. Run the `gh` commands in a clone.

1. For local runs of `scripts/release.sh`:
   `rustup target add aarch64-apple-darwin x86_64-apple-darwin`.
2. A notary key: an App Store Connect API team key, since Apple documents
   individual keys as unable to notarise. As the Account Holder, request API
   access in App Store Connect under Users and Access > Integrations, then
   generate a team key with the Developer role. Keep the downloaded
   `AuthKey_<key id>.p8`, the key ID and the issuer ID in 1Password, and delete
   the download. For local runs, store them as the profile `scripts/release.sh`
   uses. notarytool cannot read a pipe, so zsh's `=(...)` hands it a temporary
   file that zsh deletes when the command ends:

   ```sh
   xcrun notarytool store-credentials dps-notary \
     --key =(op read 'op://<vault>/<item>/<key file>') \
     --key-id "$(op read 'op://<vault>/<item>/<key id>')" \
     --issuer "$(op read 'op://<vault>/<item>/<issuer id>')"
   ```

3. The signing certificate. In Keychain Access, export your "Developer ID
   Application" certificate with its private key as a `.p12` with a password,
   keep both in 1Password, and delete the export.
4. A `release` environment that only `v*` tags can use, holding the secrets:

   ```sh
   gh api -X PUT repos/chauduyphanvu/dps-rs/environments/release \
     -F 'deployment_branch_policy[protected_branches]=false' \
     -F 'deployment_branch_policy[custom_branch_policies]=true'
   gh api -X POST repos/chauduyphanvu/dps-rs/environments/release/deployment-branch-policies \
     -f name='v*' -f type=tag
   op read -n 'op://<vault>/<item>/<p12 file>' | base64 | gh secret set DEVELOPER_ID_P12_BASE64 --env release
   op read -n 'op://<vault>/<item>/<p12 password>' | gh secret set DEVELOPER_ID_P12_PASSWORD --env release
   op read -n 'op://<vault>/<item>/<key file>' | gh secret set NOTARY_KEY_P8 --env release
   op read -n 'op://<vault>/<item>/<key id>' | gh secret set NOTARY_KEY_ID --env release
   op read -n 'op://<vault>/<item>/<issuer id>' | gh secret set NOTARY_ISSUER_ID --env release
   ```

5. The tap. Create it with
   `gh repo create chauduyphanvu/homebrew-dps --public --add-readme`. Then, under
   GitHub Settings > Developer settings > Personal access tokens > Fine-grained
   tokens, create a token with access to `chauduyphanvu/homebrew-dps` only and
   its Contents permission set to Read and write, keep it in 1Password, and:

   ```sh
   op read -n 'op://<vault>/<item>/<token>' | gh secret set TAP_GITHUB_TOKEN --env release
   ```

   When the token expires, replace it the same way.

To release:

1. Bump `version` in `Cargo.toml` and run `cargo build` to refresh `Cargo.lock`.
2. Add a `## X.Y.Z` section to `CHANGELOG.md`; it becomes the release notes.
3. Commit both, and push to `main`.
4. `git tag vX.Y.Z && git push origin vX.Y.Z`

The workflow runs CI, then checks that the tag matches `version` in
`Cargo.toml`, that the tagged commit is on `main` and that `CHANGELOG.md` has a
non-empty `## X.Y.Z` section, and stops there if not. It puts the certificate
and the notary key in a temporary keychain, runs `scripts/release.sh`, and
publishes the release `vX.Y.Z` with the zip, its `.sha256` and the changelog
section. Last, it downloads the published zip, checks it against the sha256 of
the one it built, and commits `Formula/dps.rb`, rendered from
`packaging/homebrew/dps.rb` with the new `url` and `sha256`, to the tap as
`dps X.Y.Z`.

Nothing is public until the release is published. If a run fails before that
for a reason outside the code (a secret, Apple's service), re-run it; for a fix
in the code, delete the tag (`git push origin :vX.Y.Z && git tag -d vX.Y.Z`)
and tag the fixed commit. If only the tap update fails, fix the cause and
re-run the failed job.

`scripts/release.sh` also runs on a Mac with a clean tree: it builds, signs
and notarises `dist/dps-<version>-macos-universal.zip` and writes its
`.sha256`, using the Developer ID certificate in your keychain and the
`dps-notary` profile. It publishes nothing.

## License

MIT. See [LICENSE](LICENSE).
