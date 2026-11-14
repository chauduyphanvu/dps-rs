//! Drives the built `dps` binary end to end.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use dps::{Processor, Profile};
use hound::{SampleFormat, WavSpec};

/// Mirrors `BLOCK_FRAMES` in src/main.rs.
const BLOCK_FRAMES: usize = 4096;

const PRESETS: [&str; 4] = ["headphones", "laptop-speakers", "music", "voice"];

/// Both compressors at 1:1 with every gain at 0 dB, stereo and the limiter
/// off, which makes the chain an identity within float rounding.
const IDENTITY: &str = "\
input_gain_db = 0.0
output_gain_db = 0.0
low_shelf_db = 0.0
high_shelf_db = 0.0

[compressor1]
ratio = 1.0

[compressor2]
ratio = 1.0

[eq]
gains_db = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]

[stereo]
enabled = false

[limiter]
enabled = false
";

/// Stands in for launchctl: logs each call to `launchctl.calls` beside it
/// and answers from the files there. `print` shows `launchctl.print`, or
/// reports no such service; `bootstrap` copies `on-bootstrap.print` to
/// `launchctl.print` and `on-bootstrap.state` to the service's state file,
/// and a second later `later.state` too; `bootout` ends the process
/// `launchctl.print` names and removes it. `print.mode` and `bootout.mode`
/// make them fail in the ways launchctl does.
const LAUNCHCTL_STUB: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
dir=$(dirname "$0")
state="$HOME/Library/Application Support/dps/live.state"
echo "$*" >> "$dir/launchctl.calls"
mode=$(cat "$dir/$1.mode" 2>/dev/null)
missing='Could not find service "dev.dps.live" in domain for user gui: 501'
case "$1" in
print)
    [ "$mode" = broken ] && { echo "Bad request." >&2; exit 5; }
    if [ -f "$dir/launchctl.print" ]; then cat "$dir/launchctl.print"; exit 0; fi
    case "$mode" in
    code-only) exit 113 ;;
    message-only) echo "$missing" >&2; exit 1 ;;
    esac
    echo "$missing" >&2
    exit 113 ;;
bootstrap)
    [ -f "$dir/on-bootstrap.print" ] && cp "$dir/on-bootstrap.print" "$dir/launchctl.print"
    mkdir -p "$(dirname "$state")"
    [ -f "$dir/on-bootstrap.state" ] && cp "$dir/on-bootstrap.state" "$state"
    if [ -f "$dir/later.state" ]; then
        (sleep 1; cp "$dir/later.state" "$state") </dev/null >/dev/null 2>&1 &
    fi
    exit 0 ;;
bootout)
    case "$mode" in
    fail-stuck) echo "Boot-out failed: 5: Input/output error" >&2; exit 5 ;;
    fail-gone) rm -f "$dir/launchctl.print"; echo "Boot-out failed: 3: No such process" >&2; exit 3 ;;
    leave-process) rm -f "$dir/launchctl.print"; exit 0 ;;
    esac
    pid=$(sed -n 's/^	pid = //p' "$dir/launchctl.print")
    [ -n "$pid" ] && kill "$pid"
    rm -f "$dir/launchctl.print"
    exit 0 ;;
esac
exit 64
"#;

/// A per-test directory: test files go in `work`, and the binary runs with
/// its settings file, home and temp directory beside it, so no test touches
/// the real `~/.config/dps`, and with a stand-in for launchctl, so none
/// touches the real launchd.
struct TempDir {
    root: PathBuf,
    work: PathBuf,
}

impl TempDir {
    fn new(test: &str) -> Self {
        let root = std::env::temp_dir().join(format!("dps-cli-{}-{test}", std::process::id()));
        for sub in ["work", "config", "home", "tmp", "empty"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let stub = root.join("launchctl");
            fs::write(&stub, LAUNCHCTL_STUB).unwrap();
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let work = root.join("work");
        Self { root, work }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.work.join(name)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    /// Sorted names of the files in the work directory.
    fn names(&self) -> Vec<String> {
        names_in(&self.work)
    }

    /// Sorted names of what dps left in its temp directory.
    fn temp_names(&self) -> Vec<String> {
        names_in(&self.root.join("tmp"))
    }

    /// Where `dps` looks for its settings file in this directory.
    fn settings(&self) -> PathBuf {
        self.root.join("config").join("dps").join("profile.toml")
    }

    fn write_settings(&self, contents: &str) {
        fs::create_dir_all(self.settings().parent().unwrap()).unwrap();
        fs::write(self.settings(), contents).unwrap();
    }

    /// A directory with no ffmpeg in it, to use as PATH.
    fn empty(&self) -> PathBuf {
        self.root.join("empty")
    }

    fn dps(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dps"));
        // No test reaches the real audio system: the doctor's output device
        // and whether other apps play come from these.
        cmd.env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("DPS_LAUNCHCTL", self.root.join("launchctl"))
            .env("DPS_TEST_OUTPUT", "Test Out@48000")
            .env("DPS_TEST_OTHERS_PLAYING", "0");
        cmd
    }

    fn process(&self, input: &Path, output: &Path, profile: Option<&Path>) -> Output {
        let mut cmd = self.dps();
        cmd.arg("process").arg(input).arg(output);
        if let Some(profile) = profile {
            cmd.arg("--profile").arg(profile);
        }
        cmd.output().unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn assert_success(out: &Output) {
    assert!(out.status.success(), "dps failed: {}", stderr(out));
    assert!(
        out.stdout.is_empty(),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Success with nothing at all on stdout or stderr.
fn assert_quiet_success(out: &Output) {
    assert_success(out);
    assert!(out.stderr.is_empty(), "unexpected stderr: {}", stderr(out));
}

/// Exit code 1 with a single stderr line that mentions `needle`.
fn assert_user_error(out: &Output, needle: &str) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(1), "stderr: {err}");
    assert_eq!(
        err.lines().count(),
        1,
        "expected one stderr line, got: {err}"
    );
    assert!(err.contains(needle), "{needle:?} not in stderr: {err}");
    assert!(!err.contains("panicked"), "{err}");
    assert!(out.stdout.is_empty());
}

/// Exit code 2, clap's usage error, mentioning every one of `needles`.
fn assert_usage_error(out: &Output, needles: &[&str]) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(2), "stderr: {err}");
    for needle in needles {
        assert!(err.contains(needle), "{needle:?} not in stderr: {err}");
    }
    assert!(out.stdout.is_empty());
}

fn int_spec(channels: u16, sample_rate: u32, bits: u16) -> WavSpec {
    WavSpec {
        channels,
        sample_rate,
        bits_per_sample: bits,
        sample_format: SampleFormat::Int,
    }
}

fn float_spec(channels: u16, sample_rate: u32) -> WavSpec {
    WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    }
}

/// A deterministic interleaved signal whose first two samples sit at full
/// scale, positive and negative.
fn signal(frames: usize, channels: usize) -> Vec<f32> {
    let mut samples: Vec<f32> = (0..frames * channels)
        .map(|i| {
            let t = (i / channels) as f32;
            let c = (i % channels) as f32;
            0.6 * (t * (0.013 + 0.011 * c)).sin() + 0.3 * (t * 0.31).sin()
        })
        .collect();
    samples[0] = 1.0;
    samples[1] = -1.0;
    samples
}

fn to_int(v: f32, bits: u16) -> i32 {
    let scale = f64::from(1u32 << (bits - 1));
    (f64::from(v) * scale).round().clamp(-scale, scale - 1.0) as i32
}

/// Writes `samples` (nominal [-1, 1] floats, quantized for integer specs) and
/// returns the values actually stored, as integers or floats widened to f64.
fn write_wav(path: &Path, spec: WavSpec, samples: &[f32]) -> Vec<f64> {
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    let mut stored = Vec::with_capacity(samples.len());
    for &v in samples {
        match spec.sample_format {
            SampleFormat::Float => {
                writer.write_sample(v).unwrap();
                stored.push(f64::from(v));
            }
            SampleFormat::Int => {
                let x = to_int(v, spec.bits_per_sample);
                writer.write_sample(x).unwrap();
                stored.push(f64::from(x));
            }
        }
    }
    writer.finalize().unwrap();
    stored
}

fn read_wav(path: &Path) -> (WavSpec, Vec<f64>) {
    let mut reader = hound::WavReader::open(path).unwrap();
    let spec = reader.spec();
    let samples = match spec.sample_format {
        SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| f64::from(s.unwrap()))
            .collect(),
        SampleFormat::Int => reader
            .samples::<i32>()
            .map(|s| f64::from(s.unwrap()))
            .collect(),
    };
    (spec, samples)
}

/// How far an identity pass may move a sample, in the file's own units.
fn tolerance(spec: WavSpec) -> f64 {
    match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, _) => 1e-6,
        // f32 processing cannot carry 32-bit integer precision, so allow the
        // same 1e-6 of full scale as for float files.
        (SampleFormat::Int, 32) => 1e-6 * 2f64.powi(31),
        // Up to 24 bits every sample is exact in f32, so nothing may move.
        (SampleFormat::Int, _) => 0.0,
    }
}

fn assert_close(actual: &[f64], expected: &[f64], tolerance: f64, what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: sample count");
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (a - e).abs() <= tolerance,
            "{what}: sample {i} is {a}, expected {e}"
        );
    }
}

/// What the chain makes of `samples`, as `process` would write them to a
/// float WAV file: with its latency cut from the start and made up with
/// silence at the end.
fn processed(profile: &Profile, sample_rate: u32, channels: usize, samples: &[f32]) -> Vec<f64> {
    let mut chain = Processor::new(profile, sample_rate, channels);
    let latency = chain.latency_frames() * channels;
    let mut x = samples.to_vec();
    x.resize(samples.len() + latency, 0.0);
    chain.process(&mut x);
    x[latency..].iter().copied().map(f64::from).collect()
}

fn preset_profile(dir: &TempDir, name: &str) -> Profile {
    let out = dir
        .dps()
        .args(["profile", "--preset", name])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    toml::from_str(&stdout(&out)).unwrap()
}

/// Runs the system ffmpeg, which these tests need.
fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg must be installed to run these tests");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", stderr(&out));
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// ffprobe's `key=value` lines for `entries` of `path`.
fn ffprobe(path: &Path, entries: &str) -> HashMap<String, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries"])
        .arg(entries)
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .output()
        .expect("ffprobe must be installed to run these tests");
    assert!(out.status.success(), "ffprobe: {}", stderr(&out));
    stdout(&out)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim_start_matches("TAG:").to_owned(), v.to_owned()))
        .collect()
}

/// Decodes any audio file to a 16-bit or float WAV beside it and reads it.
fn decode(path: &Path, codec: &str) -> (WavSpec, Vec<f64>) {
    let wav = path.with_extension("decoded.wav");
    ffmpeg(&["-i", path_str(path), "-c:a", codec, path_str(&wav)]);
    let result = read_wav(&wav);
    fs::remove_file(&wav).unwrap();
    result
}

#[test]
fn identity_round_trip_preserves_spec_and_samples() {
    let dir = TempDir::new("round-trip");
    let profile = dir.write("identity.toml", IDENTITY);
    let formats = [
        (SampleFormat::Int, 8),
        (SampleFormat::Int, 16),
        (SampleFormat::Int, 24),
        (SampleFormat::Int, 32),
        (SampleFormat::Float, 32),
    ];
    for (channels, sample_rate) in [(1, 44_100), (2, 96_000)] {
        for (sample_format, bits_per_sample) in formats {
            let spec = WavSpec {
                channels,
                sample_rate,
                bits_per_sample,
                sample_format,
            };
            let what = format!("{channels} ch {bits_per_sample}-bit {sample_format:?}");
            let input = dir.join(&format!(
                "in-{channels}-{bits_per_sample}-{sample_format:?}.wav"
            ));
            let output = dir.join(&format!(
                "out-{channels}-{bits_per_sample}-{sample_format:?}.wav"
            ));
            let mut samples = signal(1000, usize::from(channels));
            if sample_format == SampleFormat::Float {
                // Float output must come back unclamped.
                samples[2] = 1.5;
                samples[3] = -2.0;
            }
            let stored = write_wav(&input, spec, &samples);

            assert_success(&dir.process(&input, &output, Some(&profile)));
            let (out_spec, out_samples) = read_wav(&output);
            assert_eq!(out_spec, spec, "{what}");
            assert_close(&out_samples, &stored, tolerance(spec), &what);
        }
    }
}

#[test]
fn stream_longer_than_one_block_keeps_its_length() {
    let dir = TempDir::new("long");
    let profile = dir.write("identity.toml", IDENTITY);
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    let frames = 2 * BLOCK_FRAMES + 1781;
    let spec = int_spec(2, 48_000, 16);
    let stored = write_wav(&input, spec, &signal(frames, 2));

    assert_success(&dir.process(&input, &output, Some(&profile)));
    let (out_spec, out_samples) = read_wav(&output);
    assert_eq!(out_spec, spec);
    assert_eq!(out_samples.len(), frames * 2);
    assert_close(&out_samples, &stored, 0.0, "long stereo file");
}

#[test]
fn the_limiters_lookahead_is_taken_back_out_of_the_output() {
    // The identity chain with the limiter on, with its lookahead and true
    // peak, on audio it never turns down: the output is the input, sample for
    // sample and as long, also for a file shorter than the lookahead.
    let dir = TempDir::new("lookahead");
    let profile = dir.write(
        "limited.toml",
        &IDENTITY.replace("[limiter]\nenabled = false", "[limiter]\nenabled = true"),
    );
    let output = dir.join("out.wav");
    for frames in [10, 2 * BLOCK_FRAMES + 1781] {
        let input = dir.join(&format!("in-{frames}.wav"));
        let spec = int_spec(2, 48_000, 16);
        let quiet: Vec<f32> = signal(frames, 2).iter().map(|v| v * 0.3).collect();
        let stored = write_wav(&input, spec, &quiet);
        assert_quiet_success(&dir.process(&input, &output, Some(&profile)));
        let (out_spec, out_samples) = read_wav(&output);
        assert_eq!(out_spec, spec);
        assert_close(&out_samples, &stored, 0.0, &format!("{frames} frames"));
    }
}

#[test]
fn stateful_run_equals_one_processor_over_the_whole_stream() {
    // Both compressors and the stereo AGC are active, so state that did not
    // carry across the CLI's blocks would change the output.
    let dir = TempDir::new("stateful");
    let mut profile = Profile::default();
    profile.stereo.enabled = true;
    let profile_path = dir.write("stateful.toml", &toml::to_string(&profile).unwrap());
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    let frames = 3 * BLOCK_FRAMES + 517;
    let spec = WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let samples = signal(frames, 2);
    write_wav(&input, spec, &samples);

    assert_success(&dir.process(&input, &output, Some(&profile_path)));
    let want = processed(&profile, 48_000, 2, &samples);
    let (_, got) = read_wav(&output);
    assert_close(&got, &want, 0.0, "stateful stereo file");
}

#[test]
fn non_finite_output_is_an_error_and_writes_nothing() {
    let dir = TempDir::new("non-finite");
    let input = dir.join("in.wav");
    let output = dir.write("out.wav", "an older file in the way");
    let spec = WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    // f32::MAX is a legal float sample; +20 dB of input gain overflows it.
    let mut samples = signal(BLOCK_FRAMES + 100, 1);
    samples[BLOCK_FRAMES + 50] = f32::MAX;
    write_wav(&input, spec, &samples);
    let profile = dir.write(
        "hot.toml",
        &IDENTITY.replace("input_gain_db = 0.0", "input_gain_db = 20.0"),
    );

    let out = dir.process(&input, &output, Some(&profile));
    assert_user_error(&out, "non-finite");
    assert!(
        stderr(&out).contains("check the profile"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fs::read(&output).unwrap(), b"an older file in the way");
    assert_eq!(dir.names(), ["hot.toml", "in.wav", "out.wav"]);
}

#[test]
fn read_error_keeps_existing_output() {
    let dir = TempDir::new("truncated");
    // Without --profile, process now reads the settings file, and creating it
    // would add a line to stderr.
    dir.write_settings(&toml::to_string(&Profile::default()).unwrap());
    let input = dir.join("in.wav");
    let output = dir.write("out.wav", "an older file in the way");
    // The data header still declares three blocks after the cut, so the
    // read fails partway through the second block.
    write_wav(
        &input,
        int_spec(1, 44_100, 16),
        &signal(3 * BLOCK_FRAMES, 1),
    );
    let bytes = fs::read(&input).unwrap();
    fs::write(&input, &bytes[..bytes.len() - 3 * BLOCK_FRAMES]).unwrap();

    let out = dir.process(&input, &output, None);
    assert_user_error(&out, "cannot read");
    assert_eq!(fs::read(&output).unwrap(), b"an older file in the way");
    assert_eq!(dir.names(), ["in.wav", "out.wav"]);
}

#[test]
fn stereo_on_non_stereo_input_warns_once() {
    let dir = TempDir::new("stereo-warning");
    let profile = dir.write(
        "stereo.toml",
        &IDENTITY.replace("[stereo]\nenabled = false", "[stereo]\nenabled = true"),
    );
    let output = dir.join("out.wav");
    for channels in [1u16, 3] {
        let input = dir.join(&format!("in-{channels}.wav"));
        let stored = write_wav(
            &input,
            int_spec(channels, 44_100, 16),
            &signal(500, usize::from(channels)),
        );
        let out = dir.process(&input, &output, Some(&profile));
        assert_success(&out);
        let warning = stderr(&out);
        assert_eq!(warning.lines().count(), 1, "{warning}");
        assert!(warning.starts_with("warning: "), "{warning}");
        assert!(warning.contains("stereo"), "{warning}");
        assert!(warning.contains("skipped"), "{warning}");
        assert!(warning.contains(&format!("in-{channels}.wav")), "{warning}");
        // Skipped means skipped: the identity chain still leaves it untouched.
        assert_close(&read_wav(&output).1, &stored, 0.0, "non-stereo file");
    }

    // Quiet enough that the 8 dB side boost cannot clip.
    let quiet: Vec<f32> = signal(500, 2).iter().map(|v| v * 0.25).collect();
    let input = dir.join("in-2.wav");
    write_wav(&input, int_spec(2, 44_100, 16), &quiet);
    let out = dir.process(&input, &output, Some(&profile));
    assert_success(&out);
    assert!(out.stderr.is_empty(), "{}", stderr(&out));
}

#[test]
fn printed_profile_is_the_default_and_loads_back() {
    let dir = TempDir::new("print-profile");
    let out = dir.dps().arg("profile").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out.stderr.is_empty());
    let text = String::from_utf8(out.stdout).unwrap();
    let parsed: Profile = toml::from_str(&text).unwrap();
    assert_eq!(parsed, Profile::default());
    // Every key is written out, not left to its default.
    fn count_keys(table: &toml::Table) -> usize {
        table
            .values()
            .map(|v| v.as_table().map_or(1, count_keys))
            .sum()
    }
    assert_eq!(count_keys(&text.parse().unwrap()), 50, "{text}");
    for key in [
        "output_gain_db",
        "[leveler]",
        "[compressor2]",
        "gains_db",
        "[bass]",
        "[contour]",
        "crossover_hz",
        "[crossfeed]",
        "[correction]",
        "[limiter]",
        "lookahead_ms",
    ] {
        assert!(text.contains(key), "{key} not in\n{text}");
    }

    let profile = dir.write("default.toml", &text);
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    let spec = int_spec(2, 48_000, 16);
    write_wav(&input, spec, &signal(500, 2));
    assert_success(&dir.process(&input, &output, Some(&profile)));
    let (out_spec, out_samples) = read_wav(&output);
    assert_eq!(out_spec, spec);
    assert_eq!(out_samples.len(), 1000);
}

#[test]
fn partial_profile_is_accepted_and_existing_output_overwritten() {
    let dir = TempDir::new("partial");
    let profile = dir.write("partial.toml", "[compressor1]\nratio = 2.0\n");
    let input = dir.join("in.wav");
    let output = dir.write("out.wav", "an older file in the way");
    let spec = int_spec(1, 44_100, 24);
    write_wav(&input, spec, &signal(700, 1));

    assert_success(&dir.process(&input, &output, Some(&profile)));
    let (out_spec, out_samples) = read_wav(&output);
    assert_eq!(out_spec, spec);
    assert_eq!(out_samples.len(), 700);
}

#[test]
fn unknown_profile_key_is_rejected() {
    let dir = TempDir::new("unknown-key");
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));

    let top = dir.write("top.toml", "output_gain_db = -1.0\nloudness = 3.0\n");
    assert_user_error(&dir.process(&input, &output, Some(&top)), "loudness");
    let nested = dir.write("nested.toml", "[compressor1]\nratoi = 2.0\n");
    let out = dir.process(&input, &output, Some(&nested));
    assert_user_error(&out, "ratoi");
    assert!(stderr(&out).contains("nested.toml"));
    assert!(!output.exists());
}

#[test]
fn out_of_range_profile_value_is_rejected() {
    let dir = TempDir::new("out-of-range");
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));

    let profile = dir.write("bad.toml", "[compressor2]\nratio = 0.5\n");
    let out = dir.process(&input, &output, Some(&profile));
    assert_user_error(&out, "compressor2.ratio");
    assert!(stderr(&out).contains("bad.toml"));
    assert!(!output.exists());

    // A shelf corner must stay below a quarter of the rate.
    let low_rate = dir.join("low-rate.wav");
    write_wav(&low_rate, int_spec(1, 3_000, 16), &signal(100, 1));
    let out = dir.process(&low_rate, &output, Some(&dir.write("i.toml", IDENTITY)));
    assert_user_error(&out, "shelf_corner_hz");
    assert!(!output.exists());
}

#[test]
fn eq_bands_too_high_for_the_rate_are_skipped_with_a_warning() {
    let dir = TempDir::new("low-rate");
    // The 16 kHz band, boosted 12 dB, does not fit under 11.025 kHz.
    let boosted = IDENTITY.replace(
        "gains_db = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]",
        "gains_db = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 12.0]",
    );
    let profile = dir.write("boosted.toml", &boosted);
    let input = dir.join("speech.wav");
    let output = dir.join("out.wav");
    let samples: Vec<f32> = signal(22_050, 1).iter().map(|v| v * 0.5).collect();
    let stored = write_wav(&input, int_spec(1, 22_050, 16), &samples);

    let out = dir.process(&input, &output, Some(&profile));
    assert_success(&out);
    let warning = stderr(&out);
    assert_eq!(
        warning,
        format!(
            "warning: the EQ skipped eq.freqs_hz[10] (16000 Hz): at the 22050 Hz sample rate of \
             {}, a band's centre and bandwidth (freqs_hz / q) must be below 11025 Hz\n",
            input.display()
        )
    );
    // The rest of the chain is the identity, so the skipped band passed the
    // audio through untouched.
    assert_close(&read_wav(&output).1, &stored, 0.0, "skipped band");

    // The voice preset, at a rate that ordinary speech recordings use.
    let input = dir.join("call.wav");
    write_wav(&input, int_spec(1, 16_000, 16), &signal(16_000, 1));
    let out = dir
        .dps()
        .args(["process", path_str(&input), path_str(&output)])
        .args(["--preset", "voice"])
        .output()
        .unwrap();
    assert_success(&out);
    let warning = stderr(&out);
    assert_eq!(warning.lines().count(), 1, "{warning}");
    assert!(
        warning.contains("eq.freqs_hz[9] (8839 Hz) and eq.freqs_hz[10] (16000 Hz)"),
        "{warning}"
    );
    let voice = preset_profile(&dir, "voice");
    let (spec, got) = read_wav(&output);
    assert_eq!(spec, int_spec(1, 16_000, 16));
    assert!(got.iter().all(|v| v.is_finite()));
    let want: Vec<f64> = processed(
        &voice,
        16_000,
        1,
        &read_wav(&input)
            .1
            .iter()
            .map(|v| (v / 32_768.0) as f32)
            .collect::<Vec<_>>(),
    )
    .iter()
    .map(|v| (v * 32_768.0).round().clamp(-32_768.0, 32_767.0))
    .collect();
    assert_close(&got, &want, 0.0, "voice at 16 kHz");
}

#[test]
fn missing_files_are_reported() {
    let dir = TempDir::new("missing");
    let output = dir.join("out.wav");
    let out = dir.process(&dir.join("absent.wav"), &output, None);
    assert_user_error(&out, "absent.wav");
    assert!(!output.exists());

    let input = dir.join("in.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));
    let out = dir.process(&input, &output, Some(&dir.join("absent.toml")));
    assert_user_error(&out, "absent.toml");
    assert!(!output.exists());
}

#[test]
fn non_wav_input_is_reported() {
    let dir = TempDir::new("non-wav");
    let input = dir.write("notes.wav", "these are notes, not audio\n");
    let output = dir.join("out.wav");
    assert_user_error(&dir.process(&input, &output, None), "notes.wav");
    assert!(!output.exists());
}

#[test]
fn same_input_and_output_is_refused() {
    let dir = TempDir::new("same-path");
    let input = dir.join("in.wav");
    write_wav(&input, int_spec(2, 44_100, 16), &signal(300, 2));
    let before = fs::read(&input).unwrap();

    for output in [input.clone(), dir.join(".").join("in.wav")] {
        assert_user_error(&dir.process(&input, &output, None), "in.wav");
    }
    assert_eq!(fs::read(&input).unwrap(), before);
}

#[test]
fn clipping_is_reported_and_clamped() {
    let dir = TempDir::new("clipping");
    let input = dir.join("loud.wav");
    let output = dir.join("out.wav");
    // +20 dB takes every +-30000 sample far past full scale and leaves every
    // +-1000 sample well inside it.
    let pattern = [30_000, -30_000, 1_000, -1_000];
    let input_samples: Vec<i32> = pattern.iter().copied().cycle().take(4000).collect();
    let mut writer = hound::WavWriter::create(&input, int_spec(1, 44_100, 16)).unwrap();
    for &s in &input_samples {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();

    let identity = dir.write("identity.toml", IDENTITY);
    let out = dir.process(&input, &output, Some(&identity));
    assert_success(&out);
    assert!(
        out.stderr.is_empty(),
        "warning without clipping: {}",
        stderr(&out)
    );

    let loud = dir.write(
        "loud.toml",
        &IDENTITY.replace("output_gain_db = 0.0", "output_gain_db = 20.0"),
    );
    let out = dir.process(&input, &output, Some(&loud));
    assert_success(&out);
    let warning = stderr(&out);
    assert_eq!(warning.lines().count(), 1, "{warning}");
    assert!(warning.contains("2000 samples clipped"), "{warning}");
    assert!(warning.contains("output_gain_db"), "{warning}");

    let (_, out_samples) = read_wav(&output);
    assert_eq!(out_samples.len(), input_samples.len());
    for (i, (&o, &x)) in out_samples.iter().zip(&input_samples).enumerate() {
        let expected = match x {
            30_000 => 32_767.0,
            -30_000 => -32_768.0,
            _ => f64::from(x) * 10.0,
        };
        assert!(
            (o - expected).abs() <= 2.0,
            "sample {i}: {o}, expected {expected}"
        );
    }

    // With the limiter on, the same 20 dB clips nothing: every sample stays
    // at or below its -1 dBFS ceiling, and nothing is reported.
    let limited = dir.write(
        "limited.toml",
        &IDENTITY
            .replace("output_gain_db = 0.0", "output_gain_db = 20.0")
            .replace("[limiter]\nenabled = false", "[limiter]\nenabled = true"),
    );
    let out = dir.process(&input, &output, Some(&limited));
    assert_quiet_success(&out);
    let ceiling = (10f64.powf(-1.0 / 20.0) * 32_768.0).round();
    let (_, out_samples) = read_wav(&output);
    let peak = out_samples.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert!(peak <= ceiling && peak > ceiling - 2.0, "peak {peak}");
}

// Settings file and presets.

#[test]
fn settings_file_is_created_on_first_use_and_then_reused() {
    let dir = TempDir::new("settings-first-use");
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    let samples = signal(2000, 2);
    let stored = write_wav(&input, float_spec(2, 48_000), &samples);
    assert!(!dir.settings().exists());

    let out = dir.process(&input, &output, None);
    assert_success(&out);
    let note = stderr(&out);
    assert_eq!(note.lines().count(), 1, "{note}");
    assert!(note.contains("created settings file"), "{note}");
    assert!(
        note.contains(&dir.settings().display().to_string()),
        "{note}"
    );
    let printed = dir.dps().arg("profile").output().unwrap();
    assert_eq!(fs::read(dir.settings()).unwrap(), printed.stdout);
    let want = processed(&Profile::default(), 48_000, 2, &samples);
    assert_close(&read_wav(&output).1, &want, 0.0, "default profile");

    // The second run says nothing and uses the file as it now stands.
    fs::write(dir.settings(), IDENTITY).unwrap();
    assert_quiet_success(&dir.process(&input, &output, None));
    assert_close(&read_wav(&output).1, &stored, 0.0, "edited settings");
    assert_eq!(fs::read_to_string(dir.settings()).unwrap(), IDENTITY);
}

#[test]
fn settings_file_falls_back_to_home_config() {
    let dir = TempDir::new("settings-home");
    let input = dir.join("in.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));
    let in_home = dir.root.join("home/.config/dps/profile.toml");

    let out = dir
        .dps()
        .env("XDG_CONFIG_HOME", "")
        .args(["process", path_str(&input), path_str(&dir.join("a.wav"))])
        .output()
        .unwrap();
    assert_success(&out);
    assert!(stderr(&out).contains(&in_home.display().to_string()));
    assert!(in_home.exists());
    assert!(!dir.settings().exists());

    let out = dir
        .dps()
        .env_remove("XDG_CONFIG_HOME")
        .args(["process", path_str(&input), path_str(&dir.join("b.wav"))])
        .output()
        .unwrap();
    assert_quiet_success(&out);
    assert!(!dir.settings().exists());
}

#[test]
fn profile_flag_beats_the_settings_file() {
    let dir = TempDir::new("profile-beats-settings");
    let identity = dir.write("identity.toml", IDENTITY);
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    let stored = write_wav(&input, int_spec(2, 44_100, 16), &signal(1000, 2));

    // With no settings file yet, --profile leaves it uncreated.
    assert_quiet_success(&dir.process(&input, &output, Some(&identity)));
    assert!(!dir.settings().exists());
    assert_close(&read_wav(&output).1, &stored, 0.0, "fresh");

    let quieter = IDENTITY.replace("output_gain_db = 0.0", "output_gain_db = -6.0");
    dir.write_settings(&quieter);
    assert_quiet_success(&dir.process(&input, &output, Some(&identity)));
    assert_close(&read_wav(&output).1, &stored, 0.0, "with settings");
    assert_eq!(fs::read_to_string(dir.settings()).unwrap(), quieter);
}

#[test]
fn preset_flag_beats_the_settings_file() {
    let dir = TempDir::new("preset-beats-settings");
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    let samples = signal(3000, 2);
    let stored = write_wav(&input, float_spec(2, 44_100), &samples);
    let music = preset_profile(&dir, "music");
    let want = processed(&music, 44_100, 2, &samples);

    let run = || {
        dir.dps()
            .args(["process", path_str(&input), path_str(&output)])
            .args(["--preset", "music"])
            .output()
            .unwrap()
    };
    assert_quiet_success(&run());
    assert!(!dir.settings().exists());
    assert_close(&read_wav(&output).1, &want, 0.0, "fresh");

    dir.write_settings(IDENTITY);
    assert_quiet_success(&run());
    let got = read_wav(&output).1;
    assert_close(&got, &want, 0.0, "with settings");
    let moved = got.iter().zip(&stored).any(|(a, b)| (a - b).abs() > 1e-3);
    assert!(moved, "the preset left the signal untouched");
}

#[test]
fn profile_and_preset_together_are_rejected() {
    let dir = TempDir::new("profile-and-preset");
    let identity = dir.write("identity.toml", IDENTITY);
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));

    let out = dir
        .dps()
        .args(["process", path_str(&input), path_str(&output)])
        .args(["--profile", path_str(&identity), "--preset", "music"])
        .output()
        .unwrap();
    assert_usage_error(&out, &["--preset", "--profile", "cannot be used with"]);
    assert!(!output.exists());
    assert!(!dir.settings().exists());
}

#[test]
fn unknown_preset_is_rejected_with_the_valid_names() {
    let dir = TempDir::new("unknown-preset");
    let input = dir.join("in.wav");
    let output = dir.join("out.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));

    let process = dir
        .dps()
        .args(["process", path_str(&input), path_str(&output)])
        .args(["--preset", "rock"])
        .output()
        .unwrap();
    let profile = dir
        .dps()
        .args(["profile", "--preset", "rock"])
        .output()
        .unwrap();
    for out in [process, profile] {
        let mut needles = vec!["'rock'"];
        needles.extend(PRESETS);
        assert_usage_error(&out, &needles);
    }
    assert!(!output.exists());
}

#[test]
fn presets_lists_each_preset_with_a_description() {
    let dir = TempDir::new("presets-list");
    let out = dir.dps().arg("presets").output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out.stderr.is_empty());
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    for (line, name) in lines.iter().zip(PRESETS) {
        let (first, rest) = line.split_once(' ').unwrap();
        assert_eq!(first, name);
        assert!(rest.trim().len() > 20, "short description: {line}");
    }
    assert_eq!(lines[PRESETS.len()], "");
    let note = lines[PRESETS.len() + 1..].join(" ");
    assert!(note.contains("not Bongiovi's"), "{note}");
    assert!(note.contains("dps profile --preset NAME"), "{note}");
}

#[test]
fn each_preset_prints_as_a_valid_profile_unlike_the_default() {
    let dir = TempDir::new("preset-profiles");
    let mut seen = Vec::new();
    for name in PRESETS {
        let profile = preset_profile(&dir, name);
        for rate in [44_100, 48_000, 96_000, 192_000] {
            if let Err(e) = profile.validate(rate) {
                panic!("{name} at {rate} Hz: {e}");
            }
        }
        assert_ne!(profile, Profile::default(), "{name}");
        assert!(!seen.contains(&profile), "{name} repeats another preset");
        seen.push(profile);
    }
}

/// Deterministic white noise in [-1, 1).
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }

    /// Pink noise from Paul Kellet's filter.
    fn pink(&mut self, n: usize) -> Vec<f64> {
        let mut b = [0.0f64; 7];
        (0..n)
            .map(|_| {
                let w = self.next();
                b[0] = 0.99886 * b[0] + w * 0.0555179;
                b[1] = 0.99332 * b[1] + w * 0.0750759;
                b[2] = 0.96900 * b[2] + w * 0.1538520;
                b[3] = 0.86650 * b[3] + w * 0.3104856;
                b[4] = 0.55000 * b[4] + w * 0.5329522;
                b[5] = -0.7616 * b[5] - w * 0.0168980;
                let out = b.iter().sum::<f64>() + w * 0.5362;
                b[6] = w * 0.115926;
                out
            })
            .collect()
    }
}

fn saw(phase: f64) -> f64 {
    2.0 * (phase % 1.0) - 1.0
}

/// Loud stereo material, each part normalized to a 0 dBFS peak and starting
/// abruptly after half a second of silence, so the compressors and the
/// stereo block's detector start at rest: white and pink noise with
/// uncorrelated channels (whose side signal is as loud as the mid, which the
/// stereo block boosts), two mixes driven into tanh as loud masters are (a
/// bright one of saws and a drum-and-bass one), and a 20 Hz - 20 kHz sweep.
fn loud_material(rate: u32) -> Vec<f32> {
    let fs = f64::from(rate);
    let secs = |s: f64| (s * fs) as usize;
    let mut noise = Noise(7);
    let mut parts: Vec<(Vec<f64>, Vec<f64>)> = Vec::new();

    let n = secs(1.5);
    parts.push((0..n).map(|_| (noise.next(), noise.next())).unzip());
    parts.push((noise.pink(n), noise.pink(n)));

    let n = secs(2.0);
    let bass_notes = [55.0, 55.0, 82.4, 73.4];
    let chords = [
        [220.0, 277.2, 329.6],
        [220.0, 261.6, 329.6],
        [246.9, 293.7, 370.0],
        [196.0, 246.9, 293.7],
    ];
    parts.push(
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let tk = t % 0.5;
                let kick = (2.0 * PI * (45.0 * tk + 2.25 * (1.0 - (-tk / 0.03).exp()))).sin()
                    * (-tk / 0.15).exp();
                let bar = (t * 2.0) as usize % 4;
                let fb = bass_notes[bar];
                let bass = (0.55 * saw(fb * t) + 0.25 * (PI * fb * t).sin())
                    * (0.6 + 0.4 * (-(t % 0.25) / 0.1).exp());
                let pad = |detune: f64| {
                    chords[bar].iter().map(|f| saw(f * t * detune)).sum::<f64>() * 0.12
                };
                let hat = noise.next() * (-(t % 0.125) / 0.02).exp() * 0.35;
                let ts = (t + 0.5) % 1.0;
                let snare = (noise.next() * 0.7 + 0.5 * (2.0 * PI * 185.0 * ts).sin())
                    * (-ts / 0.08).exp()
                    * 0.6;
                let mono = 1.2 * kick + bass + snare;
                (
                    (2.2 * (mono + pad(1.002) + 0.6 * hat)).tanh(),
                    (2.2 * (mono + pad(0.998) + hat)).tanh(),
                )
            })
            .unzip(),
    );

    let n = secs(1.5);
    parts.push(
        (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let tk = t % 0.5;
                let kick = (2.0 * PI * (45.0 * tk + 2.25 * (1.0 - (-tk / 0.03).exp()))).sin()
                    * (-tk / 0.15).exp();
                let fb = if (t as usize).is_multiple_of(2) {
                    55.0
                } else {
                    82.4
                };
                let bass: f64 = (1..=4)
                    .map(|h| (2.0 * PI * fb * f64::from(h) * t).sin() / f64::from(h))
                    .sum::<f64>()
                    * 0.35;
                let chord = |weights: [f64; 3]| {
                    [220.0, 277.2, 329.6]
                        .iter()
                        .zip(weights)
                        .map(|(f, g)| g * (2.0 * PI * f * t).sin())
                        .sum::<f64>()
                        * 0.12
                };
                let hat = noise.next() * (-(t % 0.125) / 0.02).exp() * 0.25;
                let ts = (t + 0.5) % 1.0;
                let snare = (noise.next() * 0.6 + 0.4 * (2.0 * PI * 185.0 * ts).sin())
                    * (-ts / 0.08).exp()
                    * 0.5;
                let mono = kick + bass + snare;
                (
                    (4.0 * (mono + chord([1.0, 0.6, 0.6]) + 0.6 * hat)).tanh(),
                    (4.0 * (mono + chord([0.6, 0.6, 1.0]) + hat)).tanh(),
                )
            })
            .unzip(),
    );

    let n = secs(2.0);
    let (f0, f1) = (20.0f64, 20_000.0f64);
    let k = (f1 / f0).ln();
    let sweep: Vec<f64> = (0..n)
        .map(|i| (2.0 * PI * f0 * 2.0 / k * ((i as f64 / fs / 2.0 * k).exp() - 1.0)).sin())
        .collect();
    parts.push((sweep.clone(), sweep));

    let mut out = Vec::new();
    for (l, r) in parts {
        let peak = l.iter().chain(&r).fold(0.0f64, |m, v| m.max(v.abs()));
        out.extend(std::iter::repeat_n(0.0, 2 * secs(0.5)));
        out.extend(
            l.iter()
                .zip(&r)
                .flat_map(|(l, r)| [(l / peak) as f32, (r / peak) as f32]),
        );
    }
    out
}

#[test]
fn presets_do_not_clip_loud_full_scale_material() {
    let dir = TempDir::new("preset-clipping");
    for rate in [44_100, 48_000, 96_000, 192_000] {
        let input = dir.join(&format!("loud-{rate}.wav"));
        let stored = write_wav(&input, int_spec(2, rate, 16), &loud_material(rate));
        let input_peak = stored.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(input_peak >= 32_767.0, "the material is not full scale");
        for name in PRESETS {
            let output = dir.join(&format!("{name}-{rate}.wav"));
            let out = dir
                .dps()
                .args(["process", path_str(&input), path_str(&output)])
                .args(["--preset", name])
                .output()
                .unwrap();
            // A clipped sample would have printed a warning.
            assert_quiet_success(&out);
            let (_, samples) = read_wav(&output);
            let peak = samples.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            let peak_db = 20.0 * (peak / 32_768.0).log10();
            // The limiter's -1 dBFS ceiling, give or take the 16-bit rounding.
            assert!(
                peak <= (10f64.powf(-1.0 / 20.0) * 32_768.0).round(),
                "{name} at {rate} Hz peaks at {peak_db:.2} dBFS"
            );
            fs::remove_file(&output).unwrap();
        }
        fs::remove_file(&input).unwrap();
    }
}

/// `x` through the biquad b / a (a[0] = 1).
fn biquad(x: &[f64], b: [f64; 3], a: [f64; 3]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter()
        .map(|&v| {
            let y = b[0] * v + b[1] * x1 + b[2] * x2 - a[1] * y1 - a[2] * y2;
            (x2, x1, y2, y1) = (x1, v, y1, y);
            y
        })
        .collect()
}

/// BS.1770-4 integrated loudness of interleaved stereo, in LUFS: K-weighted
/// (a +4 dB high shelf at 1.5 kHz, then a 38 Hz high-pass), 400 ms blocks
/// every 100 ms, gated at -70 LUFS and at 10 LU under the mean of the rest.
/// Within 0.1 LU of ffmpeg's ebur128 on the material the presets were tuned
/// on.
fn integrated_lufs(x: &[f64], rate: u32) -> f64 {
    let fs = f64::from(rate);
    let k_weight = |x: Vec<f64>| {
        let a = 10f64.powf(4.0 / 40.0);
        let w = 2.0 * PI * 1500.0 / fs;
        let (alpha, c) = (w.sin() / 2f64.sqrt(), w.cos());
        let sa = 2.0 * a.sqrt() * alpha;
        let a0 = (a + 1.0) - (a - 1.0) * c + sa;
        let shelf = biquad(
            &x,
            [
                a * ((a + 1.0) + (a - 1.0) * c + sa) / a0,
                -2.0 * a * ((a - 1.0) + (a + 1.0) * c) / a0,
                a * ((a + 1.0) + (a - 1.0) * c - sa) / a0,
            ],
            [
                1.0,
                2.0 * ((a - 1.0) - (a + 1.0) * c) / a0,
                ((a + 1.0) - (a - 1.0) * c - sa) / a0,
            ],
        );
        let w = 2.0 * PI * 38.0 / fs;
        let (alpha, c) = (w.sin(), w.cos());
        let a0 = 1.0 + alpha;
        biquad(
            &shelf,
            [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0],
            [1.0, -2.0 * c / a0, (1.0 - alpha) / a0],
        )
    };
    let channel = |c: usize| k_weight(x.iter().skip(c).step_by(2).copied().collect());
    let (l, r) = (channel(0), channel(1));
    let block = (0.4 * fs) as usize;
    let powers: Vec<f64> = (0..=(l.len() - block) / (block / 4))
        .map(|k| {
            let span = k * block / 4..k * block / 4 + block;
            l[span.clone()]
                .iter()
                .chain(&r[span])
                .map(|v| v * v)
                .sum::<f64>()
                / block as f64
        })
        .collect();
    let lufs = |p: f64| -0.691 + 10.0 * p.log10();
    let mean = |p: &[f64]| p.iter().sum::<f64>() / p.len() as f64;
    let audible: Vec<f64> = powers.into_iter().filter(|&p| lufs(p) > -70.0).collect();
    let gate = lufs(mean(&audible)) - 10.0;
    let gated: Vec<f64> = audible.into_iter().filter(|&p| lufs(p) > gate).collect();
    lufs(mean(&gated))
}

/// Eight seconds of a band at 120 bpm, a quieter verse then a chorus: kick,
/// snare, hats, bass and a detuned pad spread across the sides, saturated a
/// little as mixes are, then set to `lufs` LUFS.
fn band_mix(rate: u32, lufs: f64) -> Vec<f64> {
    let fs = f64::from(rate);
    let mut noise = Noise(11);
    let roots = [55.0, 43.65, 65.41, 49.0];
    let chords = [
        [220.0, 261.6, 329.6],
        [174.6, 220.0, 261.6],
        [261.6, 329.6, 392.0],
        [196.0, 246.9, 293.7],
    ];
    let mut mix = Vec::new();
    let mut last = 0.0;
    for i in 0..(8.0 * fs) as usize {
        let t = i as f64 / fs;
        let chorus = t >= 4.0;
        let bar = (t / 2.0) as usize % 4;
        let tk = t % 0.5;
        let kick =
            (2.0 * PI * (45.0 * tk + 2.5 * (1.0 - (-tk / 0.025).exp()))).sin() * (-tk / 0.18).exp();
        let ts = (t + 0.5) % 1.0;
        let snare =
            (0.6 * noise.next() + 0.4 * (2.0 * PI * 190.0 * ts).sin()) * (-ts / 0.09).exp() * 0.7;
        let white = noise.next();
        let hat = (white - last) * (-(t % 0.125) / 0.025).exp() * 0.18;
        last = white;
        let bass = saw(roots[bar] * t) * (0.4 + 0.3 * (-(t % 0.25) / 0.12).exp());
        let pad = |detune: f64| {
            chords[bar].iter().map(|f| saw(f * t * detune)).sum::<f64>()
                * if chorus { 0.07 } else { 0.04 }
        };
        let level = if chorus { 1.0 } else { 0.55 };
        let mono = 1.1 * kick + snare + bass;
        mix.push((level * (mono + pad(1.003) + 0.4 * hat)).tanh());
        mix.push((level * (mono + pad(0.997) + hat)).tanh());
    }
    let gain = 10f64.powf((lufs - integrated_lufs(&mix, rate)) / 20.0);
    mix.iter().map(|v| v * gain).collect()
}

#[test]
fn presets_keep_typical_material_at_least_about_as_loud() {
    // Each preset adds back the gain its levelling takes, so a -14 or -20
    // LUFS mix comes out no more than 1 LU quieter than it went in (they were
    // tuned to about 0 and +3 to +5 LU on similar material), and the limiter
    // leaves the -20 LUFS mix alone.
    let dir = TempDir::new("preset-loudness");
    let rate = 48_000;
    for target in [-14.0, -20.0] {
        let input = band_mix(rate, target);
        let peak = input.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(
            20.0 * peak.log10() <= -1.0,
            "a {target} LUFS mix peaks at {:.2} dBFS",
            20.0 * peak.log10()
        );
        let samples: Vec<f32> = input.iter().map(|&v| v as f32).collect();
        for name in PRESETS {
            let profile = preset_profile(&dir, name);
            let out = processed(&profile, rate, 2, &samples);
            let change = integrated_lufs(&out, rate) - target;
            let unlimited = Profile {
                limiter: dps::profile::LimiterParams {
                    enabled: false,
                    ..profile.limiter
                },
                ..profile.clone()
            };
            let raw = processed(&unlimited, rate, 2, &samples);
            let reduction = raw
                .iter()
                .zip(&out)
                .filter(|(r, _)| r.abs() > 1e-6)
                .fold(0.0f64, |m, (r, o)| m.max(20.0 * (r / o).abs().log10()));
            eprintln!(
                "{name} on a {target} LUFS mix: {change:+.2} LU, limiter at most {reduction:.2} dB"
            );
            assert!(
                change > -1.0,
                "{name} on a {target} LUFS mix: {change:+.2} LU"
            );
            if target == -20.0 {
                assert!(
                    reduction < 0.5,
                    "{name} limits a -20 LUFS mix by {reduction:.2} dB"
                );
            }
        }
    }
}

// Output naming and formats other than WAV.

#[test]
fn default_output_is_beside_the_input_with_dps_before_the_extension() {
    let dir = TempDir::new("default-output");
    let identity = dir.write("identity.toml", IDENTITY);
    let spec = int_spec(2, 44_100, 16);
    let stored = write_wav(&dir.join("song.wav"), spec, &signal(800, 2));
    fs::copy(dir.join("song.wav"), dir.join("take")).unwrap();

    for (input, output) in [("song.wav", "song.dps.wav"), ("take", "take.dps.wav")] {
        // Without ffmpeg on PATH: WAV to WAV never needs it.
        let out = dir
            .dps()
            .env("PATH", dir.empty())
            .args(["process", path_str(&dir.join(input)), "-p"])
            .arg(&identity)
            .output()
            .unwrap();
        assert_quiet_success(&out);
        let (out_spec, samples) = read_wav(&dir.join(output));
        assert_eq!(out_spec, spec, "{input}");
        assert_close(&samples, &stored, 0.0, input);
    }

    // A relative input names its output relative to the same directory.
    let out = dir
        .dps()
        .current_dir(&dir.work)
        .args(["process", "song.wav", "--preset", "voice"])
        .output()
        .unwrap();
    assert_quiet_success(&out);
    let (_, samples) = read_wav(&dir.join("song.dps.wav"));
    assert_eq!(samples.len(), stored.len());
    assert!(
        samples.iter().zip(&stored).any(|(a, b)| a != b),
        "the voice preset changed nothing"
    );
}

#[cfg(unix)]
#[test]
fn default_output_that_is_the_input_is_refused() {
    let dir = TempDir::new("default-output-same");
    let input = dir.join("in.wav");
    write_wav(&input, int_spec(2, 44_100, 16), &signal(300, 2));
    let before = fs::read(&input).unwrap();
    std::os::unix::fs::symlink(&input, dir.join("in.dps.wav")).unwrap();

    let out = dir
        .dps()
        .args(["process", path_str(&input), "--preset", "music"])
        .output()
        .unwrap();
    assert_user_error(&out, "is the input file");
    assert!(stderr(&out).contains("in.dps.wav"), "{}", stderr(&out));
    assert_eq!(fs::read(&input).unwrap(), before);
    assert_eq!(dir.names(), ["in.dps.wav", "in.wav"]);
}

#[test]
fn flac_round_trip_is_sample_exact_and_keeps_format_and_tags() {
    let dir = TempDir::new("flac");
    let identity = dir.write("identity.toml", IDENTITY);
    for (bits, sample_fmt) in [(16, "s16"), (24, "s32")] {
        let frames = 3 * BLOCK_FRAMES + 123;
        let wav = dir.join(&format!("src-{bits}.wav"));
        let stored = write_wav(&wav, int_spec(2, 44_100, bits), &signal(frames, 2));
        let flac = dir.join(&format!("song-{bits}.flac"));
        ffmpeg(&[
            "-i",
            path_str(&wav),
            "-metadata",
            "title=Test Tone",
            "-metadata",
            "artist=dps",
            path_str(&flac),
        ]);

        assert_quiet_success(
            &dir.dps()
                .args(["process", path_str(&flac), "-p", path_str(&identity)])
                .output()
                .unwrap(),
        );
        let output = dir.join(&format!("song-{bits}.dps.flac"));
        let info = ffprobe(
            &output,
            "stream=codec_name,sample_fmt,sample_rate,channels,bits_per_raw_sample:format_tags",
        );
        assert_eq!(info["codec_name"], "flac", "{bits}-bit");
        assert_eq!(info["sample_fmt"], sample_fmt, "{bits}-bit");
        assert_eq!(info["bits_per_raw_sample"], bits.to_string(), "{bits}-bit");
        assert_eq!(info["sample_rate"], "44100");
        assert_eq!(info["channels"], "2");
        assert_eq!(info["title"], "Test Tone");
        assert_eq!(info["artist"], "dps");

        let codec = if bits == 16 { "pcm_s16le" } else { "pcm_s24le" };
        let (spec, samples) = decode(&output, codec);
        assert_eq!(spec, int_spec(2, 44_100, bits));
        assert_close(&samples, &stored, 0.0, &format!("{bits}-bit FLAC"));
    }
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

#[test]
fn lossy_round_trips_keep_duration_rate_channels_bit_rate_and_tags() {
    let dir = TempDir::new("lossy");
    let identity = dir.write("identity.toml", IDENTITY);
    let cases = [
        ("mp3", 48_000, 2, "libmp3lame", "192k"),
        ("m4a", 44_100, 1, "aac", "128k"),
    ];
    for (extension, rate, channels, encoder, bit_rate) in cases {
        let seconds = 2.0;
        let frames = (seconds * f64::from(rate)) as usize;
        let samples: Vec<f32> = signal(frames, usize::from(channels))
            .iter()
            .map(|v| v * 0.5)
            .collect();
        let wav = dir.join(&format!("src-{extension}.wav"));
        write_wav(&wav, int_spec(channels, rate, 16), &samples);
        let input = dir.join(&format!("song.{extension}"));
        ffmpeg(&[
            "-i",
            path_str(&wav),
            "-c:a",
            encoder,
            "-b:a",
            bit_rate,
            "-metadata",
            "title=Lossy Tone",
            path_str(&input),
        ]);

        assert_quiet_success(
            &dir.dps()
                .args(["process", path_str(&input), "-p", path_str(&identity)])
                .output()
                .unwrap(),
        );
        let output = dir.join(&format!("song.dps.{extension}"));
        let entries = "stream=codec_name,sample_rate,channels,bit_rate:format=duration:format_tags";
        let before = ffprobe(&input, entries);
        let after = ffprobe(&output, entries);
        for key in ["codec_name", "sample_rate", "channels", "title"] {
            assert_eq!(after[key], before[key], "{extension} {key}");
        }
        assert_eq!(after["sample_rate"], rate.to_string());
        assert_eq!(after["channels"], channels.to_string());
        assert_eq!(after["title"], "Lossy Tone");
        let duration = |info: &HashMap<String, String>| info["duration"].parse::<f64>().unwrap();
        let drift = (duration(&after) - seconds).abs();
        assert!(drift < 0.05, "{extension}: {} s", duration(&after));
        let kbps = |info: &HashMap<String, String>| info["bit_rate"].parse::<f64>().unwrap() / 1e3;
        let ratio = kbps(&after) / kbps(&before);
        assert!(
            (0.85..1.15).contains(&ratio),
            "{extension}: {} kb/s became {} kb/s",
            kbps(&before),
            kbps(&after)
        );

        // The identity chain only adds a second generation of coding.
        let rms = |path: &Path| {
            let (_, x) = decode(path, "pcm_f32le");
            (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
        };
        let level_db = 20.0 * (rms(&output) / rms(&input)).log10();
        assert!(
            level_db.abs() < 0.5,
            "{extension}: level moved {level_db} dB"
        );
    }
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

#[test]
fn channel_layouts_survive_ffmpeg() {
    let dir = TempDir::new("layouts");
    let identity = dir.write("identity.toml", IDENTITY);
    let cases = [
        (
            "quad",
            "pan=quad|c0=c0|c1=0.5*c0|c2=0.25*c0|c3=0.1*c0",
            "flac",
        ),
        (
            "5.1(side)",
            "pan=5.1(side)|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0",
            "flac",
        ),
        ("mono", "pan=mono|c0=c0", "opus"),
    ];
    for (layout, pan, extension) in cases {
        let input = dir.join(&format!("in.{extension}"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.5:sample_rate=48000",
            "-af",
            pan,
            path_str(&input),
        ]);
        assert_eq!(
            ffprobe(&input, "stream=channel_layout")["channel_layout"],
            layout
        );
        assert_quiet_success(
            &dir.dps()
                .args(["process", path_str(&input), "-p", path_str(&identity)])
                .output()
                .unwrap(),
        );
        let output = dir.join(&format!("in.dps.{extension}"));
        assert_eq!(
            ffprobe(&output, "stream=channel_layout")["channel_layout"],
            layout
        );
    }
}

#[test]
fn wav_input_can_be_written_as_another_format() {
    let dir = TempDir::new("wav-to-flac");
    let identity = dir.write("identity.toml", IDENTITY);
    let input = dir.join("in.wav");
    let stored = write_wav(&input, int_spec(2, 48_000, 24), &signal(5000, 2));
    let output = dir.join("out.flac");

    assert_quiet_success(&dir.process(&input, &output, Some(&identity)));
    let (spec, samples) = decode(&output, "pcm_s24le");
    assert_eq!(spec, int_spec(2, 48_000, 24));
    assert_close(&samples, &stored, 0.0, "WAV to FLAC");
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

#[test]
fn clipping_is_reported_through_ffmpeg() {
    let dir = TempDir::new("clipping-ffmpeg");
    let wav = dir.join("loud.wav");
    let pattern = [30_000, -30_000, 1_000, -1_000];
    let mut writer = hound::WavWriter::create(&wav, int_spec(1, 44_100, 16)).unwrap();
    for &s in pattern.iter().cycle().take(4000) {
        writer.write_sample(s).unwrap();
    }
    writer.finalize().unwrap();
    let flac = dir.join("loud.flac");
    let mp3 = dir.join("loud.mp3");
    ffmpeg(&["-i", path_str(&wav), path_str(&flac)]);
    ffmpeg(&["-i", path_str(&wav), "-b:a", "192k", path_str(&mp3)]);
    let loud = dir.write(
        "loud.toml",
        &IDENTITY.replace("output_gain_db = 0.0", "output_gain_db = 20.0"),
    );

    // A 16-bit source is processed at 16 bits, so the count matches WAV's.
    let out = dir.process(&flac, &dir.join("out.flac"), Some(&loud));
    assert_success(&out);
    let warning = stderr(&out);
    assert_eq!(warning.lines().count(), 1, "{warning}");
    assert!(warning.contains("2000 samples clipped in"), "{warning}");
    assert!(warning.contains("out.flac"), "{warning}");

    // An MP3 decodes to float, and samples past full scale still count.
    let out = dir.process(&mp3, &dir.join("out.mp3"), Some(&loud));
    assert_success(&out);
    let warning = stderr(&out);
    assert_eq!(warning.lines().count(), 1, "{warning}");
    assert!(warning.contains("samples clipped in"), "{warning}");
}

#[test]
fn missing_ffmpeg_gives_the_install_hint() {
    let dir = TempDir::new("no-ffmpeg");
    let identity = dir.write("identity.toml", IDENTITY);
    let wav = dir.join("in.wav");
    write_wav(&wav, int_spec(2, 44_100, 16), &signal(500, 2));
    let flac = dir.join("in.flac");
    ffmpeg(&["-i", path_str(&wav), path_str(&flac)]);

    for (input, output) in [(&flac, dir.join("out.flac")), (&wav, dir.join("out.mp3"))] {
        let out = dir
            .dps()
            .env("PATH", dir.empty())
            .args(["process", path_str(input), path_str(&output)])
            .args(["-p", path_str(&identity)])
            .output()
            .unwrap();
        assert_user_error(&out, "brew install ffmpeg");
        assert!(stderr(&out).contains("not installed"), "{}", stderr(&out));
        assert!(!output.exists());
    }
    assert_eq!(dir.names(), ["identity.toml", "in.flac", "in.wav"]);
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

#[test]
fn failed_encode_keeps_existing_output_and_leaves_no_temp_files() {
    let dir = TempDir::new("failed-encode");
    let identity = dir.write("identity.toml", IDENTITY);
    let wav = dir.join("src.wav");
    write_wav(&wav, int_spec(2, 44_100, 16), &signal(5000, 2));
    let flac = dir.join("in.flac");
    ffmpeg(&["-i", path_str(&wav), path_str(&flac)]);
    fs::remove_file(&wav).unwrap();
    // ffmpeg has no format for this extension, so only the final encode
    // fails, after decoding and processing.
    let output = dir.write("out.xyz", "an older file in the way");

    let out = dir.process(&flac, &output, Some(&identity));
    assert_user_error(&out, "ffmpeg failed to write");
    let err = stderr(&out);
    assert!(err.contains("Unable to choose an output format"), "{err}");
    // The message names the output, not the hidden file ffmpeg wrote to.
    assert!(err.contains(&format!("'{}'", output.display())), "{err}");
    assert!(!err.contains(".tmp"), "{err}");
    assert_eq!(fs::read(&output).unwrap(), b"an older file in the way");
    assert_eq!(dir.names(), ["identity.toml", "in.flac", "out.xyz"]);
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

#[test]
fn non_finite_output_through_ffmpeg_writes_nothing() {
    let dir = TempDir::new("non-finite-ffmpeg");
    let input = dir.join("in.wav");
    let mut samples = signal(BLOCK_FRAMES + 100, 1);
    samples[BLOCK_FRAMES + 50] = f32::MAX;
    write_wav(&input, float_spec(1, 44_100), &samples);
    let hot = dir.write(
        "hot.toml",
        &IDENTITY.replace("input_gain_db = 0.0", "input_gain_db = 20.0"),
    );
    let output = dir.write("out.flac", "an older file in the way");

    assert_user_error(&dir.process(&input, &output, Some(&hot)), "non-finite");
    assert_eq!(fs::read(&output).unwrap(), b"an older file in the way");
    assert_eq!(dir.names(), ["hot.toml", "in.wav", "out.flac"]);
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

#[test]
fn unreadable_input_is_reported() {
    let dir = TempDir::new("unreadable");
    for name in ["notes.flac", "notes.mp3"] {
        let input = dir.write(name, "these are notes, not audio\n");
        let out = dir
            .dps()
            .args(["process", path_str(&input), "--preset", "music"])
            .output()
            .unwrap();
        assert_user_error(&out, "cannot read");
        assert!(stderr(&out).contains(name), "{}", stderr(&out));
    }
    assert_eq!(dir.names(), ["notes.flac", "notes.mp3"]);
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

#[cfg(target_os = "macos")]
impl TempDir {
    fn run(&self, args: &[&str]) -> Output {
        self.dps().args(args).output().unwrap()
    }

    fn home(&self, path: &str) -> PathBuf {
        self.root.join("home").join(path)
    }

    /// The service's own folder under the test's home.
    fn support(&self, name: &str) -> PathBuf {
        self.home("Library/Application Support/dps").join(name)
    }

    /// Sets what the launchctl stand-in answers or does; see LAUNCHCTL_STUB.
    fn stub(&self, name: &str, contents: &str) {
        fs::write(self.root.join(name), contents).unwrap();
    }

    fn launchctl_calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("launchctl.calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Holds the lock a live run holds, as `pid mode` would.
    fn hold_live_lock(&self, mode: &str) -> fs::File {
        use std::io::Write;
        let path = self.support("live.lock");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(&path).unwrap();
        file.lock().unwrap();
        writeln!(file, "{} {mode}", std::process::id()).unwrap();
        file
    }
}

#[cfg(target_os = "macos")]
fn service_target() -> String {
    let out = Command::new("id").arg("-u").output().unwrap();
    format!(
        "gui/{}/dev.dps.live",
        String::from_utf8(out.stdout).unwrap().trim()
    )
}

#[cfg(target_os = "macos")]
fn this_dps() -> String {
    fs::canonicalize(env!("CARGO_BIN_EXE_dps"))
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

/// `launchctl print` for the job, as launchd words it.
#[cfg(target_os = "macos")]
fn launchd_print(pid: Option<u32>, last_exit: &str, args: &[&str]) -> String {
    let state = if pid.is_some() {
        "running"
    } else {
        "not running"
    };
    let mut text = format!(
        "gui/501/dev.dps.live = {{\n\tactive count = 1\n\ttype = LaunchAgent\n\tstate = {state}\n\n\
         \tprogram = {}\n\targuments = {{\n",
        args[0]
    );
    for arg in args {
        text += &format!("\t\t{arg}\n");
    }
    text += "\t}\n\n\texit timeout = 5\n";
    if let Some(pid) = pid {
        text += &format!("\tpid = {pid}\n");
    }
    text += &format!(
        "\tlast exit code = {last_exit}\n\n\tresource coalition = {{\n\t\tstate = active\n\t}}\n}}\n"
    );
    text
}

/// What a running service writes to its state file.
#[cfg(target_os = "macos")]
fn running_state(pid: u32, settings: &str) -> String {
    format!(
        "pid = {pid}\nstarted = 1763000000\nsettings = \"{settings}\"\nphase = \"running\"\n\
         device = \"Test Out\"\nrate = 48000\nheard = true\n"
    )
}

/// A process that is not this test's child, so it is reaped when killed,
/// to stand in for the service's.
#[cfg(target_os = "macos")]
fn orphan_sleeper() -> u32 {
    sleeper_for(60)
}

/// The same, sleeping `secs` seconds.
#[cfg(target_os = "macos")]
fn sleeper_for(secs: u32) -> u32 {
    let out = Command::new("/bin/sh")
        .args(["-c", &format!("sleep {secs} >/dev/null 2>&1 & echo $!")])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[cfg(target_os = "macos")]
fn stdout_line(out: &Output) -> String {
    assert!(out.status.success(), "dps failed: {}", stderr(out));
    assert!(out.stderr.is_empty(), "unexpected stderr: {}", stderr(out));
    stdout(out)
}

#[cfg(target_os = "macos")]
#[test]
fn background_commands_with_nothing_running_change_nothing() {
    let dir = TempDir::new("bg-idle");
    let target = service_target();
    assert_eq!(
        stdout_line(&dir.run(&["status"])),
        "dps is not running in the background; start at login is off\n"
    );
    assert_eq!(
        stdout_line(&dir.run(&["stop"])),
        "dps is not running in the background\n"
    );
    assert_eq!(
        stdout_line(&dir.run(&["stop", "--login"])),
        "dps is not running in the background; start at login was already off\n"
    );
    assert_eq!(dir.launchctl_calls(), vec![format!("print {target}"); 3]);
    assert!(names_in(&dir.home("")).is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn start_refuses_settings_that_do_not_load_before_touching_launchd() {
    let dir = TempDir::new("bg-bad-settings");
    let bad = dir.write("bad.toml", "nonsense = 1\n");
    assert_user_error(
        &dir.run(&["start", "--profile", path_str(&bad)]),
        "nonsense",
    );
    assert_user_error(
        &dir.run(&["start", "--profile", "missing.toml"]),
        "missing.toml",
    );
    assert!(dir.launchctl_calls().is_empty());
    assert!(names_in(&dir.home("")).is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn start_loads_the_canonical_binary_and_reports_a_service_that_fails() {
    let dir = TempDir::new("bg-fails");
    let target = service_target();
    let exe = this_dps();
    let args = [&*exe, "live", "--service", "--preset", "voice"];
    dir.stub("on-bootstrap.print", &launchd_print(None, "1", &args));
    let log = dir.home("Library/Logs/dps.log");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(
        &log,
        "error: preset voice does not fit Test Out at 8000 Hz\n",
    )
    .unwrap();

    let out = dir.run(&["start", "--preset", "voice"]);
    assert_user_error(
        &out,
        &format!(
            "dps could not start in the background: it ended with exit code 1: error: preset \
             voice does not fit Test Out at 8000 Hz (log: {})",
            log.display()
        ),
    );
    let plist = dir.support("dev.dps.live.plist");
    let calls = dir.launchctl_calls();
    assert_eq!(
        calls[..2],
        [
            format!("print {target}"),
            format!(
                "bootstrap {} {}",
                &target[..target.len() - 13],
                plist.display()
            )
        ]
    );

    let text = fs::read_to_string(&plist).unwrap();
    let strings: Vec<&str> = text
        .split("<string>")
        .skip(1)
        .map(|s| s.split_once("</string>").unwrap().0)
        .collect();
    let home = dir.home("");
    let config = dir.root.join("config");
    let log_str = log.to_str().unwrap();
    assert_eq!(
        strings,
        [
            "dev.dps.live",
            &*exe,
            "live",
            "--service",
            "--preset",
            "voice",
            home.to_str().unwrap().trim_end_matches('/'),
            config.to_str().unwrap(),
            "Interactive",
            log_str,
            log_str,
        ]
    );
    assert!(text.contains("<key>RunAtLoad</key>\n\t<true/>"));
    assert!(text.contains("<key>KeepAlive</key>\n\t<false/>"));
    assert!(!dir.home("Library/LaunchAgents").exists());

    assert_eq!(
        stdout_line(&dir.run(&["status"])),
        format!(
            "dps is not running in the background: it ended with exit code 1; see {}; start at \
             login is off\n",
            log.display()
        )
    );
    assert_eq!(
        stdout_line(&dir.run(&["stop"])),
        "dps was not running in the background\n"
    );
    assert!(dir.launchctl_calls().contains(&format!("bootout {target}")));
    assert!(!plist.exists());

    // Start at login is not left behind by a start that failed.
    let out = dir.run(&["start", "--login", "--preset", "voice"]);
    assert_user_error(
        &out,
        "dps could not start in the background: it ended with exit code 1",
    );
    assert!(!dir.home("Library/LaunchAgents/dev.dps.live.plist").exists());
    assert!(stdout_line(&dir.run(&["status"])).ends_with("; start at login is off\n"));
}

#[cfg(target_os = "macos")]
#[test]
fn start_reports_the_running_service_and_stop_ends_it() {
    let dir = TempDir::new("bg-runs");
    let exe = this_dps();
    let pid = orphan_sleeper();
    let args = [&*exe, "live", "--service", "--preset", "music"];
    dir.stub(
        "on-bootstrap.print",
        &launchd_print(Some(pid), "(never exited)", &args),
    );
    dir.stub("on-bootstrap.state", &running_state(pid, "preset music"));

    assert_eq!(
        stdout_line(&dir.run(&["start", "--preset", "music"])),
        "dps is running in the background on Test Out at 48000 Hz with preset music; `dps stop` \
         stops it\n"
    );
    assert_eq!(
        stdout_line(&dir.run(&["start", "--preset", "voice"])),
        "dps is already running in the background with preset music; `dps stop` stops it\n"
    );
    let bootstraps = |dir: &TempDir| {
        dir.launchctl_calls()
            .iter()
            .filter(|c| c.starts_with("bootstrap"))
            .count()
    };
    assert_eq!(bootstraps(&dir), 1);

    let status = stdout_line(&dir.run(&["status"]));
    assert!(
        status.starts_with("dps is running in the background since 20"),
        "{status}"
    );
    assert!(
        status.ends_with(&format!(
            " (pid {pid}) with preset music on Test Out at 48000 Hz; start at login is off\n"
        )),
        "{status}"
    );

    let stopped = stdout_line(&dir.run(&["stop"]));
    assert!(
        stopped.starts_with("stopped dps, which ran in the background since 20"),
        "{stopped}"
    );
    assert!(stopped.ends_with(" with preset music\n"), "{stopped}");
    assert!(!alive(pid));
    assert!(!dir.support("live.state").exists());
    assert!(!dir.support("dev.dps.live.plist").exists());
    assert_eq!(
        stdout_line(&dir.run(&["status"])),
        "dps is not running in the background; start at login is off\n"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn start_at_login_is_installed_and_removed() {
    let dir = TempDir::new("bg-login");
    let target = service_target();
    let exe = this_dps();
    let pid = orphan_sleeper();
    let args = [&*exe, "live", "--service", "--preset", "voice"];
    dir.stub(
        "on-bootstrap.print",
        &launchd_print(Some(pid), "(never exited)", &args),
    );
    dir.stub("on-bootstrap.state", &running_state(pid, "preset voice"));
    let agent = dir.home("Library/LaunchAgents/dev.dps.live.plist");

    assert_eq!(
        stdout_line(&dir.run(&["start", "--login", "--preset", "voice"])),
        "dps is running in the background on Test Out at 48000 Hz with preset voice; `dps stop` \
         stops it; it also starts at every login with preset voice\n"
    );
    let domain = &target[..target.len() - "/dev.dps.live".len()];
    assert!(
        dir.launchctl_calls()
            .contains(&format!("bootstrap {domain} {}", agent.display()))
    );
    assert!(
        fs::read_to_string(&agent)
            .unwrap()
            .contains("<string>voice</string>")
    );
    assert!(!dir.support("dev.dps.live.plist").exists());
    let status = stdout_line(&dir.run(&["status"]));
    assert!(
        status.ends_with("; start at login is on, with preset voice\n"),
        "{status}"
    );

    let stopped = stdout_line(&dir.run(&["stop"]));
    assert!(
        stopped.ends_with("; start at login is still on (`dps stop --login` turns it off)\n"),
        "{stopped}"
    );
    assert!(agent.exists());
    assert!(!alive(pid));
    assert_eq!(
        stdout_line(&dir.run(&["status"])),
        "dps is not running in the background; start at login is on, with preset voice\n"
    );
    assert_eq!(
        stdout_line(&dir.run(&["stop", "--login"])),
        "dps is not running in the background; start at login is now off\n"
    );
    assert!(!agent.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn live_refuses_to_run_beside_another_live_run() {
    let dir = TempDir::new("bg-lock");
    // Were the lock ignored, this profile would fail to load before any audio.
    let bad = dir.write("bad.toml", "nonsense = 1\n");
    let pid = std::process::id();
    {
        let _lock = dir.hold_live_lock("background");
        let want =
            format!("dps is already running in the background (pid {pid}); run `dps stop` first");
        assert_user_error(&dir.run(&["live", "--profile", path_str(&bad)]), &want);
        assert_user_error(&dir.run(&["--profile", path_str(&bad)]), &want);
    }
    {
        let _lock = dir.hold_live_lock("foreground");
        let want =
            format!("dps live is already running in the foreground (pid {pid}); quit it first");
        assert_user_error(&dir.run(&["live", "--profile", path_str(&bad)]), &want);
        assert_user_error(&dir.run(&["start", "--preset", "music"]), &want);
    }
    assert!(
        !dir.launchctl_calls()
            .iter()
            .any(|c| c.starts_with("bootstrap")),
        "{:?}",
        dir.launchctl_calls()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_login_agent_whose_binary_is_gone_is_reported() {
    let dir = TempDir::new("bg-stale");
    let gone = dir.root.join("moved/dps");
    let agent = dir.home("Library/LaunchAgents/dev.dps.live.plist");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    fs::write(
        &agent,
        format!(
            "<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>dev.dps.live</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\
             \t\t<string>live</string>\n\t\t<string>--service</string>\n\t</array>\n</dict>\n</plist>\n",
            gone.display()
        ),
    )
    .unwrap();
    let want = format!(
        "start at login is on, but it runs {}, which no longer exists; fix: `dps start --login` \
         with this dps, or `dps stop --login`",
        gone.display()
    );
    let status = stdout_line(&dir.run(&["status"]));
    assert!(status.contains(&want), "{status}");

    // A held lock stands in for a running dps, so the doctor makes no tap.
    let _lock = dir.hold_live_lock("foreground");
    let out = dir.run(&["doctor", "--preset", "music"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let report = stdout(&out);
    assert!(
        report.contains(&format!(
            "\nproblem  start at login: {}",
            &want["start at login is ".len()..]
        )),
        "{report}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn doctor_reports_each_check_and_fails_only_on_what_live_mode_needs() {
    let dir = TempDir::new("doctor");
    let bad = dir.write("bad.toml", "nonsense = 1\n");
    let _lock = dir.hold_live_lock("foreground");
    let pid = std::process::id();

    let out = dir
        .dps()
        .args(["doctor", "--profile", path_str(&bad)])
        .env("PATH", dir.empty())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stderr.is_empty(), "{}", stderr(&out));
    let report = stdout(&out);
    let lines: Vec<&str> = report.lines().collect();
    let names: Vec<&str> = lines
        .iter()
        .map(|l| l[9..].split_once(':').unwrap().0)
        .collect();
    assert_eq!(
        names,
        [
            "macOS",
            "output",
            "settings",
            "capture",
            "ffmpeg",
            "service",
            "start at login"
        ]
    );
    assert!(lines[0].starts_with("ok       macOS: "), "{report}");
    assert!(lines[1].starts_with("ok       output: "), "{report}");
    assert!(lines[2].starts_with("problem  settings: "), "{report}");
    assert!(lines[2].contains("nonsense"), "{report}");
    assert!(
        lines[2].ends_with("(`dps profile` prints the default with every key)"),
        "{report}"
    );
    assert_eq!(
        lines[3],
        format!(
            "ok       capture: not checked: dps live is already running in the foreground (pid \
             {pid}), and a second tap would hear its output"
        )
    );
    assert_eq!(
        lines[4],
        "problem  ffmpeg: ffmpeg and ffprobe not on PATH (needed only for files other than WAV); \
         fix: brew install ffmpeg"
    );
    assert_eq!(lines[5], "ok       service: not running in the background");
    assert_eq!(lines[6], "ok       start at login: off");

    // Without ffmpeg, which live mode does not need, all is well.
    let out = dir
        .dps()
        .args(["doctor", "--preset", "music"])
        .env("PATH", dir.empty())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(stdout(&out).contains("\nok       settings: preset music is valid at "));
}

#[cfg(target_os = "macos")]
#[test]
fn help_lists_the_background_commands() {
    let dir = TempDir::new("help-bg");
    let help = stdout(&dir.run(&["--help"]));
    for want in [
        "start",
        "stop",
        "status",
        "doctor",
        "dps start ",
        "dps doctor ",
    ] {
        assert!(help.contains(want), "{want} not in\n{help}");
    }
    assert!(!help.contains("--service"));
    let live = stdout(&dir.run(&["live", "--help"]));
    assert!(!live.contains("--service"), "{live}");
}

// Damaged input, signals, and what an output keeps of its input.

/// Waits up to `limit` for `child` to exit and returns its output.
fn wait_for_exit(mut child: std::process::Child, limit: Duration) -> Output {
    use std::io::Read;
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("dps did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

/// Polls `f` for up to ten seconds until it gives a value.
fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn damaged_lossless_input_is_an_error_not_a_shorter_file() {
    let dir = TempDir::new("damaged");
    let identity = dir.write("identity.toml", IDENTITY);
    let wav = dir.join("src.wav");
    write_wav(&wav, int_spec(2, 44_100, 16), &signal(10 * 44_100, 2));
    let full = dir.join("full.flac");
    ffmpeg(&["-i", path_str(&wav), path_str(&full)]);
    fs::remove_file(&wav).unwrap();
    let bytes = fs::read(&full).unwrap();
    let middle = bytes.len() / 2;
    fs::write(dir.join("cut.flac"), &bytes[..middle]).unwrap();
    let mut corrupt = bytes.clone();
    for b in &mut corrupt[middle..middle + 2000] {
        *b ^= 0x5a;
    }
    fs::write(dir.join("corrupt.flac"), &corrupt).unwrap();

    assert_quiet_success(&dir.process(&full, &dir.join("full.out.flac"), Some(&identity)));
    for name in ["cut", "corrupt"] {
        let input = dir.join(&format!("{name}.flac"));
        let output = dir.join(&format!("{name}.out.flac"));
        let out = dir.process(&input, &output, Some(&identity));
        assert_user_error(
            &out,
            &format!("ffmpeg could not decode all of {}: ", input.display()),
        );
        assert!(!output.exists(), "{name}");
    }
    assert_eq!(
        dir.names(),
        [
            "corrupt.flac",
            "cut.flac",
            "full.flac",
            "full.out.flac",
            "identity.toml"
        ]
    );
    assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
}

/// A directory with an `ffmpeg` that runs the real one, except that a run
/// with `$STALL_ON` among its arguments writes its pid to `$STALL_PID` and
/// sleeps instead.
#[cfg(unix)]
fn stalling_ffmpeg(dir: &TempDir) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let real = Command::new("/bin/sh")
        .args(["-c", "command -v ffmpeg"])
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap().trim().to_owned();
    assert!(
        !real.is_empty(),
        "ffmpeg must be installed to run these tests"
    );
    let bin = dir.root.join("stall-bin");
    fs::create_dir_all(&bin).unwrap();
    let stub = bin.join("ffmpeg");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"$STALL_ON\" ]; then\n    echo $$ > \
             \"$STALL_PID.tmp\"; mv \"$STALL_PID.tmp\" \"$STALL_PID\"; exec sleep 30\n  fi\n\
             done\nexec '{real}' \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[cfg(unix)]
#[test]
fn a_signal_stops_process_and_removes_its_files() {
    let dir = TempDir::new("signals");
    let identity = dir.write("identity.toml", IDENTITY);
    let wav = dir.join("src.wav");
    write_wav(&wav, int_spec(2, 44_100, 16), &signal(44_100, 2));
    let flac = dir.join("in.flac");
    ffmpeg(&["-i", path_str(&wav), path_str(&flac)]);
    fs::remove_file(&wav).unwrap();
    let output = dir.write("in.dps.flac", "an older file in the way");
    let path = format!(
        "{}:{}",
        stalling_ffmpeg(&dir).display(),
        std::env::var("PATH").unwrap()
    );
    let pid_file = dir.root.join("stalled.pid");

    // Decoding, then encoding, each stalled until the signal arrives.
    for (stage, signal, name) in [
        ("-xerror", "INT", "Ctrl-C (SIGINT)"),
        ("-map_metadata", "TERM", "SIGTERM"),
        ("-map_metadata", "HUP", "SIGHUP"),
    ] {
        let _ = fs::remove_file(&pid_file);
        let child = dir
            .dps()
            .env("PATH", &path)
            .env("STALL_ON", stage)
            .env("STALL_PID", &pid_file)
            .args(["process", path_str(&flac), "-p", path_str(&identity)])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let ffmpeg_pid: u32 = eventually("the stalled ffmpeg", || {
            fs::read_to_string(&pid_file).ok()?.trim().parse().ok()
        });
        // Its scratch directory and hidden output are there now.
        assert_eq!(dir.temp_names().len(), 1, "{stage}");
        assert!(dir.names().iter().any(|n| n.contains(".tmp.")), "{stage}");

        let status = Command::new("/bin/kill")
            .args([&format!("-{signal}"), &child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let out = wait_for_exit(child, Duration::from_secs(3));
        assert_user_error(&out, &format!("stopped by {name}; nothing was written"));
        assert!(!alive(ffmpeg_pid), "{stage}: ffmpeg is still running");
        assert!(dir.temp_names().is_empty(), "{:?}", dir.temp_names());
        assert_eq!(dir.names(), ["identity.toml", "in.dps.flac", "in.flac"]);
        assert_eq!(fs::read(&output).unwrap(), b"an older file in the way");
    }
}

#[test]
fn wav_to_wav_keeps_the_channel_layout_and_the_other_chunks() {
    let dir = TempDir::new("wav-chunks");
    let identity = dir.write("identity.toml", IDENTITY);
    for (layout, pan, codec) in [
        (
            "quad",
            "pan=quad|c0=c0|c1=0.5*c0|c2=0.25*c0|c3=0.1*c0",
            "pcm_s24le",
        ),
        (
            "5.1(side)",
            "pan=5.1(side)|c0=c0|c1=c0|c2=c0|c3=c0|c4=c0|c5=c0",
            "pcm_s16le",
        ),
        ("mono", "pan=mono|c0=c0", "pcm_s24le"),
    ] {
        let input = dir.join("in.wav");
        let output = dir.join("out.wav");
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.5:sample_rate=48000",
            "-af",
            pan,
            "-c:a",
            codec,
            "-metadata",
            "title=Chunky",
            "-metadata",
            "description=Broadcast text",
            "-write_bext",
            "1",
            path_str(&input),
        ]);
        let before = ffprobe(&input, "stream=channel_layout:format_tags");
        assert_eq!(before["channel_layout"], layout);
        assert_eq!(before["title"], "Chunky");

        assert_quiet_success(&dir.process(&input, &output, Some(&identity)));
        // The identity chain leaves the samples as they are, so the whole
        // file is: its fmt chunk with the channel mask, bext and LIST.
        assert!(
            fs::read(&output).unwrap() == fs::read(&input).unwrap(),
            "{layout}"
        );
        let after = ffprobe(&output, "stream=channel_layout:format_tags");
        assert_eq!(after, before, "{layout}");
    }
}

#[test]
fn pcm_formats_keep_the_sample_format_and_a_lower_one_is_reported() {
    let dir = TempDir::new("depth");
    let identity = dir.write("identity.toml", IDENTITY);
    let float = dir.join("float.wav");
    write_wav(&float, float_spec(2, 48_000), &signal(4800, 2));
    let s24 = dir.join("s24.wav");
    write_wav(&s24, int_spec(2, 48_000, 24), &signal(4800, 2));
    for (input, extension, sample_fmt, bits) in [
        (&float, "caf", "flt", "N/A"),
        (&float, "w64", "flt", "N/A"),
        (&float, "au", "flt", "N/A"),
        (&float, "aiff", "flt", "N/A"),
        (&s24, "caf", "s32", "24"),
        (&s24, "au", "s32", "24"),
        (&s24, "w64", "s32", "24"),
    ] {
        let output = dir.join(&format!("out.{extension}"));
        assert_quiet_success(&dir.process(input, &output, Some(&identity)));
        let info = ffprobe(&output, "stream=sample_fmt,bits_per_raw_sample");
        assert_eq!(info["sample_fmt"], sample_fmt, "{extension}");
        assert_eq!(info["bits_per_raw_sample"], bits, "{extension}");
    }

    // FLAC as ffmpeg writes it holds 24 bits, and dps works in 32-bit float.
    let s32 = dir.join("s32.wav");
    write_wav(&s32, int_spec(2, 48_000, 32), &signal(4800, 2));
    let flac = dir.join("deep.flac");
    ffmpeg(&[
        "-i",
        path_str(&s32),
        "-c:a",
        "flac",
        "-strict",
        "experimental",
        path_str(&flac),
    ]);
    let f64_wav = dir.join("double.wav");
    ffmpeg(&[
        "-i",
        path_str(&s32),
        "-c:a",
        "pcm_f64le",
        path_str(&f64_wav),
    ]);
    for (input, output, want) in [
        (
            &flac,
            dir.join("deep.dps.flac"),
            "has 24-bit integer samples, less than the 32-bit integer samples of",
        ),
        (
            &f64_wav,
            dir.join("double.dps.wav"),
            "has 32-bit float samples, less than the 64-bit float samples of",
        ),
    ] {
        let out = dir.process(input, &output, Some(&identity));
        assert_success(&out);
        assert_eq!(
            stderr(&out),
            format!("warning: {} {want} {}\n", output.display(), input.display())
        );
        assert!(output.exists());
    }
}

#[test]
fn cover_art_is_kept_where_the_format_holds_it_and_drops_are_reported() {
    let dir = TempDir::new("cover");
    let identity = dir.write("identity.toml", IDENTITY);
    let wav = dir.join("src.wav");
    // Below full scale, so that AAC does not take it over.
    let quiet: Vec<f32> = signal(22_050, 2).iter().map(|v| v * 0.5).collect();
    write_wav(&wav, int_spec(2, 44_100, 16), &quiet);
    let cover = dir.join("cover.png");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=red:s=16x16",
        "-frames:v",
        "1",
        path_str(&cover),
    ]);
    let pictures = |path: &Path| {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v"])
            .args([
                "-show_entries",
                "stream=codec_name:stream_disposition=attached_pic",
            ])
            .args(["-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        stdout(&out)
    };
    for extension in ["flac", "mp3", "m4a", "aiff"] {
        let input = dir.join(&format!("song.{extension}"));
        let mut args = vec!["-i", path_str(&wav), "-i", path_str(&cover)];
        args.extend(["-map", "0:a", "-map", "1:v", "-c:v", "copy"]);
        args.extend(["-disposition:v", "attached_pic", "-write_id3v2", "1"]);
        if extension == "aiff" {
            args.extend(["-c:a", "pcm_s16be"]);
        }
        args.push(path_str(&input));
        ffmpeg(&args);
        assert_eq!(pictures(&input), "png,1\n", "{extension}");

        let output = dir.join(&format!("song.dps.{extension}"));
        assert_quiet_success(&dir.process(&input, &output, Some(&identity)));
        assert_eq!(pictures(&output), "png,1\n", "{extension}");
    }

    let input = dir.join("song.flac");
    let output = dir.join("song.wav");
    let out = dir.process(&input, &output, Some(&identity));
    assert_success(&out);
    assert_eq!(
        stderr(&out),
        format!(
            "warning: {} leaves out its cover art (1 picture that .wav files cannot hold) of {}\n",
            output.display(),
            input.display()
        )
    );
}

#[test]
fn only_the_first_audio_stream_is_processed_and_the_rest_reported() {
    let dir = TempDir::new("streams");
    let identity = dir.write("identity.toml", IDENTITY);
    let first = dir.join("first.wav");
    // Below full scale, so that AAC does not take it over.
    let quiet: Vec<f32> = signal(9600, 2).iter().map(|v| v * 0.5).collect();
    let stored = write_wav(&first, int_spec(2, 48_000, 16), &quiet);
    let second = dir.join("second.wav");
    write_wav(&second, int_spec(1, 22_050, 16), &signal(4410, 1));
    // Two audio streams, the first with its own tags, in Matroska.
    let input = dir.join("two.mka");
    ffmpeg(&[
        "-i",
        path_str(&first),
        "-i",
        path_str(&second),
        "-map",
        "0:a",
        "-map",
        "1:a",
        "-c:a",
        "flac",
        "-metadata:s:a:0",
        "language=fin",
        "-metadata",
        "title=Two",
        path_str(&input),
    ]);
    let out = dir
        .dps()
        .args(["process", path_str(&input), "-p", path_str(&identity)])
        .output()
        .unwrap();
    assert_success(&out);
    let output = dir.join("two.dps.mka");
    assert_eq!(
        stderr(&out),
        format!(
            "warning: {} leaves out 1 more audio stream of {}\n",
            output.display(),
            input.display()
        )
    );
    let info = ffprobe(
        &output,
        "stream=codec_name,channels,sample_rate:stream_tags=language:format_tags=title",
    );
    assert_eq!(info["codec_name"], "flac");
    assert_eq!(info["channels"], "2");
    assert_eq!(info["sample_rate"], "48000");
    assert_eq!(info["language"], "fin");
    assert_eq!(info["title"], "Two");
    let (spec, samples) = decode(&output, "pcm_s16le");
    assert_eq!(spec, int_spec(2, 48_000, 16));
    assert_close(&samples, &stored, 0.0, "first stream");

    // A video is left out too.
    let video = dir.join("clip.mp4");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=blue:s=32x32:d=0.5",
        "-i",
        path_str(&first),
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-c:a",
        "aac",
        "-shortest",
        path_str(&video),
    ]);
    let output = dir.join("clip.m4a");
    let out = dir.process(&video, &output, Some(&identity));
    assert_success(&out);
    assert_eq!(
        stderr(&out),
        format!(
            "warning: {} leaves out 1 video stream of {}\n",
            output.display(),
            video.display()
        )
    );
}

#[test]
fn the_same_extension_keeps_the_codec_and_its_bit_rate() {
    let dir = TempDir::new("codecs");
    let identity = dir.write("identity.toml", IDENTITY);
    let wav = dir.join("src.wav");
    // Noise, so that the encoder uses all of the bit rate it is given.
    let mut noise = Noise(3);
    let samples: Vec<f32> = (0..2 * 4 * 48_000)
        .map(|_| (0.3 * noise.next()) as f32)
        .collect();
    write_wav(&wav, int_spec(2, 48_000, 16), &samples);
    let opus = dir.join("in.opus");
    ffmpeg(&[
        "-i",
        path_str(&wav),
        "-c:a",
        "libopus",
        "-b:a",
        "140k",
        "-metadata",
        "title=Stream Tag",
        path_str(&opus),
    ]);
    let mka = dir.join("opus.mka");
    let ogg = dir.join("opus.ogg");
    let flac_mka = dir.join("flac.mka");
    ffmpeg(&["-i", path_str(&opus), "-c:a", "copy", path_str(&mka)]);
    ffmpeg(&["-i", path_str(&opus), "-c:a", "copy", path_str(&ogg)]);
    ffmpeg(&["-i", path_str(&wav), "-c:a", "flac", path_str(&flac_mka)]);

    let kbps = |path: &Path| {
        ffprobe(path, "format=bit_rate")["bit_rate"]
            .parse::<f64>()
            .unwrap()
            / 1e3
    };
    for (input, codec) in [
        (&opus, "opus"),
        (&mka, "opus"),
        (&ogg, "opus"),
        (&flac_mka, "flac"),
    ] {
        let out = dir
            .dps()
            .args(["process", path_str(input), "-p", path_str(&identity)])
            .output()
            .unwrap();
        assert_quiet_success(&out);
        let name = input.file_name().unwrap().to_str().unwrap();
        let (stem, extension) = name.split_once('.').unwrap();
        let output = dir.join(&format!("{stem}.dps.{extension}"));
        assert_eq!(
            ffprobe(&output, "stream=codec_name")["codec_name"],
            codec,
            "{name}"
        );
        if codec == "opus" {
            // Ogg and Matroska store no per-stream rate for Opus.
            let ratio = kbps(&output) / kbps(input);
            assert!(
                (0.85..1.15).contains(&ratio),
                "{name}: {} kb/s became {} kb/s",
                kbps(input),
                kbps(&output)
            );
        }
    }
    // Ogg keeps its tags on the stream.
    let output = dir.join("in.dps.opus");
    let tags = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream_tags=title",
            "-of",
            "csv=p=0",
        ])
        .arg(&output)
        .output()
        .unwrap();
    assert_eq!(stdout(&tags), "Stream Tag\n");
}

#[test]
fn awkward_file_names_reach_ffmpeg_as_files() {
    let dir = TempDir::new("names");
    let identity = dir.write("identity.toml", IDENTITY);
    let wav = dir.join("src.wav");
    let stored = write_wav(&wav, int_spec(2, 44_100, 16), &signal(4410, 2));
    // Relative names, as typed: a colon would otherwise name a protocol.
    for name in [
        "a:b.flac",
        "-dash.flac",
        "100%d.flac",
        "two  spaces.flac",
        "naïve ünïcode.flac",
    ] {
        let input = dir.join(name);
        ffmpeg(&[
            "-i",
            path_str(&wav),
            "-metadata",
            "title=Odd",
            path_str(&input),
        ]);
        let out = dir
            .dps()
            .current_dir(&dir.work)
            .args(["process", "-p", path_str(&identity), "--", name])
            .output()
            .unwrap();
        assert_quiet_success(&out);
        let output = dir.join(&name.replace(".flac", ".dps.flac"));
        assert_eq!(
            ffprobe(&output, "format_tags=title")["title"],
            "Odd",
            "{name}"
        );
        let (_, samples) = decode(&output, "pcm_s16le");
        assert_close(&samples, &stored, 0.0, name);
    }
}

#[test]
fn non_finite_input_is_reported_as_the_inputs_problem() {
    let dir = TempDir::new("non-finite-input");
    let identity = dir.write("identity.toml", IDENTITY);
    let input = dir.join("in.wav");
    let mut samples = signal(BLOCK_FRAMES + 100, 2);
    samples[2 * (BLOCK_FRAMES + 50) + 1] = f32::NAN;
    write_wav(&input, float_spec(2, 48_000), &samples);
    let output = dir.join("out.wav");
    let out = dir.process(&input, &output, Some(&identity));
    let at = (BLOCK_FRAMES + 50) as f64 / 48_000.0;
    assert_user_error(
        &out,
        &format!(
            "{} has a NaN or infinite sample at {at:.3} s, which cannot be processed; the input \
             file is damaged",
            input.display()
        ),
    );
    assert!(!stderr(&out).contains("profile"), "{}", stderr(&out));
    assert!(!output.exists());
}

#[test]
fn an_input_named_like_dps_output_needs_an_output_name() {
    let dir = TempDir::new("dps-dps");
    let identity = dir.write("identity.toml", IDENTITY);
    let input = dir.join("song.dps.wav");
    write_wav(&input, int_spec(2, 44_100, 16), &signal(441, 2));
    let out = dir
        .dps()
        .args(["process", path_str(&input), "-p", path_str(&identity)])
        .output()
        .unwrap();
    assert_user_error(
        &out,
        "looks like dps output already; to process it again, name the output",
    );
    assert_eq!(dir.names(), ["identity.toml", "song.dps.wav"]);
    let again = dir.join("song.again.wav");
    assert_quiet_success(&dir.process(&input, &again, Some(&identity)));
}

#[test]
fn process_help_states_what_is_kept() {
    let dir = TempDir::new("help-process");
    let help = stdout(&dir.dps().args(["process", "--help"]).output().unwrap());
    for want in [
        "channel layout",
        "decoding error stops the run",
        "PCM formats keep the sample format",
        "keeps its codec and bit rate",
        "cover art",
        "Only the first audio stream is processed",
        "a warning names whatever the output leaves out or stores with fewer bits",
    ] {
        assert!(help.contains(want), "{want} not in\n{help}");
    }
}

#[test]
fn a_settings_file_that_cannot_be_created_is_reported() {
    let dir = TempDir::new("settings-unwritable");
    let input = dir.join("in.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));
    let config = dir.root.join("config/dps");
    fs::create_dir_all(&config).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o555)).unwrap();
    }
    let out = dir.process(&input, &dir.join("out.wav"), None);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_user_error(
        &out,
        &format!("cannot create settings file {}: ", dir.settings().display()),
    );
}

#[test]
fn a_relative_config_home_is_ignored_and_a_relative_home_refused() {
    let dir = TempDir::new("relative-dirs");
    let input = dir.join("in.wav");
    write_wav(&input, int_spec(1, 44_100, 16), &signal(100, 1));
    let in_home = dir.root.join("home/.config/dps/profile.toml");
    let out = dir
        .dps()
        .current_dir(&dir.work)
        .env("XDG_CONFIG_HOME", "config")
        .args(["process", "in.wav", "a.wav"])
        .output()
        .unwrap();
    assert_success(&out);
    assert!(
        stderr(&out).contains(&in_home.display().to_string()),
        "{}",
        stderr(&out)
    );
    assert!(in_home.exists());
    assert!(!dir.work.join("config").exists());

    let out = dir
        .dps()
        .current_dir(&dir.root)
        .env("HOME", "home")
        .env_remove("XDG_CONFIG_HOME")
        .args(["process", path_str(&input), path_str(&dir.join("b.wav"))])
        .output()
        .unwrap();
    assert_user_error(
        &out,
        "cannot locate the settings file: HOME is home, which is not an absolute path; set HOME \
         to your home directory",
    );
}

// The background service, beyond the happy paths.

/// A state file in `phase` (as the service words it), with `extra` lines.
#[cfg(target_os = "macos")]
fn state_in(pid: u32, phase: &str, extra: &str) -> String {
    format!(
        "pid = {pid}\nstarted = 1763000000\nsettings = \"preset music\"\nphase = \"{phase}\"\n\
         device = \"Test Out\"\nrate = 48000\n{extra}"
    )
}

/// `launchctl print` for a running job with the music preset.
#[cfg(target_os = "macos")]
fn running_print(pid: u32) -> String {
    let exe = this_dps();
    launchd_print(
        Some(pid),
        "(never exited)",
        &[&*exe, "live", "--service", "--preset", "music"],
    )
}

#[cfg(target_os = "macos")]
#[test]
fn doctor_reports_the_output_it_is_told_about_not_this_machines() {
    let dir = TempDir::new("doctor-output");
    let _lock = dir.hold_live_lock("foreground");
    let out = dir
        .dps()
        .env("DPS_TEST_OUTPUT", "there is no default output device")
        .args(["doctor", "--preset", "voice"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let report = stdout(&out);
    assert!(
        report.contains(
            "\nproblem  output: there is no default output device; fix: choose an output in \
             System Settings > Sound\n"
        ),
        "{report}"
    );
    assert!(
        report.contains("\nok       settings: preset voice is valid at 48000 Hz\n"),
        "{report}"
    );

    // A low rate skips EQ bands, which is not a problem.
    let out = dir
        .dps()
        .env("DPS_TEST_OUTPUT", "Phone Line@8000")
        .args(["doctor", "--preset", "voice"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let report = stdout(&out);
    assert!(
        report.contains("\nok       output: Phone Line at 8000 Hz\n"),
        "{report}"
    );
    assert!(
        report.contains(
            "\nok       settings: preset voice is valid at 8000 Hz, though at this rate the EQ \
             skips eq.freqs_hz[8] (4698 Hz), eq.freqs_hz[9] (8839 Hz) and eq.freqs_hz[10] (16000 \
             Hz)\n"
        ),
        "{report}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn doctor_reports_an_ffmpeg_that_does_not_run() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("doctor-broken-ffmpeg");
    let _lock = dir.hold_live_lock("foreground");
    let bin = dir.root.join("broken-bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ["ffmpeg", "ffprobe"] {
        fs::write(bin.join(name), "#!/bin/sh\nexit 134\n").unwrap();
        fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = dir
        .dps()
        .env("PATH", &bin)
        .args(["doctor", "--preset", "music"])
        .output()
        .unwrap();
    // Live mode does not need ffmpeg.
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(
        stdout(&out).contains(
            "\nproblem  ffmpeg: `ffmpeg -version` failed with exit code 134; `ffprobe -version` \
             failed with exit code 134 (needed only for files other than WAV); fix: brew \
             reinstall ffmpeg\n"
        ),
        "{}",
        stdout(&out)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn doctor_without_home_says_how_to_fix_it() {
    let dir = TempDir::new("doctor-no-home");
    let fix = "HOME is not set; set HOME to your home directory";
    let out = dir
        .dps()
        .env_remove("HOME")
        .args(["doctor", "--preset", "music"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let report = stdout(&out);
    assert!(
        report.contains("\nok       settings: preset music is valid at 48000 Hz\n"),
        "{report}"
    );
    assert!(
        report.contains(&format!("\nproblem  capture: {fix}\n")),
        "{report}"
    );
    assert!(
        report.contains(&format!("\nproblem  service: {fix}\n")),
        "{report}"
    );
    assert!(dir.launchctl_calls().is_empty());

    let out = dir
        .dps()
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .arg("doctor")
        .output()
        .unwrap();
    assert!(out.stderr.is_empty(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains(&format!(
            "\nproblem  settings: cannot locate the settings file: {fix}\n"
        )),
        "{}",
        stdout(&out)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn the_service_run_refuses_to_start_beside_another_live_run() {
    let dir = TempDir::new("bg-serve");
    // Should the lock not hold, this profile fails before any audio starts.
    let bad = dir.write("bad.toml", "nonsense = 1\n");
    let pid = std::process::id();
    let state = dir.support("live.state");
    {
        let _lock = dir.hold_live_lock("background");
        let out = dir.run(&["live", "--service", "--profile", path_str(&bad)]);
        assert_user_error(
            &out,
            &format!("dps is already running in the background (pid {pid})"),
        );
        // That run owns the state file.
        assert!(!state.exists());
    }
    let _lock = dir.hold_live_lock("foreground");
    let out = dir.run(&["live", "--service", "--profile", path_str(&bad)]);
    let want = format!("dps live is already running in the foreground (pid {pid}); quit it first");
    assert_user_error(&out, &want);
    let text = fs::read_to_string(&state).unwrap();
    assert!(text.contains("phase = \"failed\""), "{text}");
    assert!(text.contains(&want), "{text}");
    assert!(!text.contains("nonsense"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
fn start_waits_for_the_service_to_start_audio() {
    let dir = TempDir::new("bg-start-wait");
    let pid = orphan_sleeper();
    dir.stub("on-bootstrap.print", &running_print(pid));
    dir.stub("on-bootstrap.state", &state_in(pid, "starting", ""));
    dir.stub("later.state", &state_in(pid, "running", "heard = true\n"));
    let started = Instant::now();
    assert_eq!(
        stdout_line(&dir.run(&["start", "--preset", "music"])),
        "dps is running in the background on Test Out at 48000 Hz with preset music; `dps stop` \
         stops it\n"
    );
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert_eq!(
        stdout_line(&dir.run(&["stop"])).split_once(',').unwrap().0,
        "stopped dps"
    );
    assert!(!alive(pid));
}

#[cfg(target_os = "macos")]
#[test]
fn start_gives_up_on_a_service_that_never_starts_audio() {
    let dir = TempDir::new("bg-start-timeout");
    let pid = orphan_sleeper();
    dir.stub("on-bootstrap.print", &running_print(pid));
    dir.stub("on-bootstrap.state", &state_in(pid, "starting", ""));
    let started = Instant::now();
    let out = dir.run(&["start", "--preset", "music"]);
    assert_user_error(
        &out,
        &format!(
            "dps started in the background but has not started processing audio after 10 s; see \
             `dps status` and {}",
            dir.home("Library/Logs/dps.log").display()
        ),
    );
    assert!(started.elapsed() >= Duration::from_secs(10));
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
}

#[cfg(target_os = "macos")]
#[test]
fn start_waits_to_hear_sound_while_other_apps_play() {
    let dir = TempDir::new("bg-hear");
    let agent = dir.home("Library/LaunchAgents/dev.dps.live.plist");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let earlier = "<plist>an earlier start at login that worked</plist>\n";
    fs::write(&agent, earlier).unwrap();

    // The service gives up on a tap that hears only silence.
    let pid = orphan_sleeper();
    dir.stub("on-bootstrap.print", &running_print(pid));
    dir.stub(
        "on-bootstrap.state",
        &state_in(pid, "running", "heard = false\n"),
    );
    dir.stub(
        "later.state",
        &state_in(pid, "failed", "problem = \"the tap heard only silence\"\n"),
    );
    let out = dir
        .dps()
        .env("DPS_TEST_OTHERS_PLAYING", "1")
        .args(["start", "--login", "--preset", "music"])
        .output()
        .unwrap();
    assert_user_error(
        &out,
        "dps could not start in the background: the tap heard only silence; the earlier start at \
         login was kept",
    );
    assert_eq!(fs::read_to_string(&agent).unwrap(), earlier);
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();

    // Or hears sound.
    fs::remove_file(dir.root.join("launchctl.print")).unwrap();
    let pid = orphan_sleeper();
    dir.stub("on-bootstrap.print", &running_print(pid));
    dir.stub(
        "on-bootstrap.state",
        &state_in(pid, "running", "heard = false\n"),
    );
    dir.stub("later.state", &state_in(pid, "running", "heard = true\n"));
    let started = Instant::now();
    let out = dir
        .dps()
        .env("DPS_TEST_OTHERS_PLAYING", "1")
        .args(["start", "--preset", "music"])
        .output()
        .unwrap();
    assert!(stdout_line(&out).starts_with("dps is running in the background on Test Out"));
    assert!(started.elapsed() >= Duration::from_millis(900));
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
}

#[cfg(target_os = "macos")]
#[test]
fn a_failed_start_at_login_keeps_the_earlier_one() {
    let dir = TempDir::new("bg-login-kept");
    let exe = this_dps();
    let args = [&*exe, "live", "--service", "--preset", "voice"];
    dir.stub("on-bootstrap.print", &launchd_print(None, "1", &args));
    let agent = dir.home("Library/LaunchAgents/dev.dps.live.plist");
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let earlier = launchd_print(
        None,
        "0",
        &[&*exe, "live", "--service", "--preset", "music"],
    );
    fs::write(&agent, &earlier).unwrap();

    let out = dir.run(&["start", "--login", "--preset", "voice"]);
    let err = stderr(&out);
    assert_user_error(
        &out,
        "dps could not start in the background: it ended with exit code 1",
    );
    assert!(
        err.ends_with("; the earlier start at login was kept\n"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(&agent).unwrap(), earlier);
}

#[cfg(target_os = "macos")]
#[test]
fn start_unloads_a_job_that_has_ended_before_loading_it_again() {
    let dir = TempDir::new("bg-ended");
    let target = service_target();
    let exe = this_dps();
    let args = [&*exe, "live", "--service", "--preset", "music"];
    dir.stub("launchctl.print", &launchd_print(None, "0", &args));
    let pid = orphan_sleeper();
    dir.stub("on-bootstrap.print", &running_print(pid));
    dir.stub("on-bootstrap.state", &running_state(pid, "preset music"));

    assert!(
        stdout_line(&dir.run(&["start", "--preset", "music"]))
            .starts_with("dps is running in the background on Test Out at 48000 Hz")
    );
    let calls = dir.launchctl_calls();
    let bootout = calls.iter().position(|c| *c == format!("bootout {target}"));
    let bootstrap = calls.iter().position(|c| c.starts_with("bootstrap "));
    assert!(bootout.is_some() && bootout < bootstrap, "{calls:?}");
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
}

#[cfg(target_os = "macos")]
#[test]
fn start_says_when_the_running_service_waits_for_permission() {
    let dir = TempDir::new("bg-permission");
    let pid = orphan_sleeper();
    dir.stub("launchctl.print", &running_print(pid));
    let state = dir.support("live.state");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    fs::write(&state, state_in(pid, "waiting-for-permission", "")).unwrap();
    let out = dir.run(&["start", "--preset", "music"]);
    assert_user_error(
        &out,
        "dps is already running in the background but is waiting for permission to record \
         system audio: click Allow in the macOS prompt",
    );
    assert!(
        !dir.launchctl_calls()
            .iter()
            .any(|c| c.starts_with("bootstrap"))
    );
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
}

#[cfg(target_os = "macos")]
#[test]
fn status_tells_a_starting_service_from_an_unreadable_state_file() {
    let dir = TempDir::new("bg-unreadable");
    let pid = std::process::id();
    dir.stub("launchctl.print", &running_print(pid));
    let state = dir.support("live.state");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    let unreadable = format!(
        "dps is running in the background (pid {pid}) with preset music, but its state file {} \
         is missing or unreadable, so how it is doing is unknown; fix: `dps stop`, then `dps \
         start`; start at login is off\n",
        state.display()
    );

    // Not yet holding the lock, it has not got as far as its state file.
    assert_eq!(
        stdout_line(&dir.run(&["status"])),
        format!(
            "dps is starting in the background (pid {pid}) with preset music; start at login is off\n"
        )
    );
    {
        // Holding it, it would have written one.
        let _lock = dir.hold_live_lock("background");
        assert_eq!(stdout_line(&dir.run(&["status"])), unreadable);
    }
    fs::write(&state, "phase = [not a state\n").unwrap();
    assert_eq!(stdout_line(&dir.run(&["status"])), unreadable);
}

#[cfg(target_os = "macos")]
#[test]
fn stop_waits_for_the_process_to_exit() {
    let dir = TempDir::new("bg-stop-wait");
    let pid = sleeper_for(1);
    dir.stub("launchctl.print", &running_print(pid));
    // launchd unloads the job at once, but its process takes a second.
    dir.stub("bootout.mode", "leave-process");
    assert_eq!(
        stdout_line(&dir.run(&["stop"])),
        "stopped dps in the background\n"
    );
    assert!(!alive(pid));
}

#[cfg(target_os = "macos")]
#[test]
fn a_stop_that_times_out_keeps_the_files_until_the_run_exits() {
    let dir = TempDir::new("bg-stop-timeout");
    let pid = orphan_sleeper();
    let me = std::process::id();
    dir.stub("launchctl.print", &running_print(pid));
    dir.stub("bootout.mode", "leave-process");
    let state = dir.support("live.state");
    let plist = dir.support("dev.dps.live.plist");
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    fs::write(&state, running_state(pid, "preset music")).unwrap();
    fs::write(&plist, "<plist/>").unwrap();
    let lock = dir.hold_live_lock("background");

    assert_user_error(
        &dir.run(&["stop"]),
        &format!("dps in the background (pid {pid}) did not stop within 10 s"),
    );
    assert!(state.exists() && plist.exists());
    // launchd has let go of it, but it still runs.
    assert_user_error(
        &dir.run(&["stop"]),
        &format!(
            "dps in the background (pid {me}) is still running although launchd has unloaded \
             it; run `dps stop` again once it has stopped, or end it with `kill {me}`"
        ),
    );
    assert!(state.exists() && plist.exists());

    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
    drop(lock);
    assert_eq!(
        stdout_line(&dir.run(&["stop"])),
        "dps is not running in the background\n"
    );
    assert!(!state.exists() && !plist.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn launchctl_failures_are_told_apart() {
    let dir = TempDir::new("bg-launchctl");
    let idle = "dps is not running in the background; start at login is off\n";
    // launchctl says there is no such job by its exit code, its message, or
    // both.
    for mode in ["code-only", "message-only"] {
        dir.stub("print.mode", mode);
        assert_eq!(stdout_line(&dir.run(&["status"])), idle, "{mode}");
    }
    dir.stub("print.mode", "broken");
    assert_user_error(
        &dir.run(&["status"]),
        "launchctl print failed: Bad request.",
    );
    fs::remove_file(dir.root.join("print.mode")).unwrap();

    // A bootout that fails although the job has gone is a stop.
    let pid = orphan_sleeper();
    dir.stub("launchctl.print", &running_print(pid));
    dir.stub("bootout.mode", "fail-gone");
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
    assert_eq!(
        stdout_line(&dir.run(&["stop"])),
        "stopped dps in the background\n"
    );

    // One that fails with the job still there is not.
    let pid = orphan_sleeper();
    dir.stub("launchctl.print", &running_print(pid));
    dir.stub("bootout.mode", "fail-stuck");
    let plist = dir.support("dev.dps.live.plist");
    fs::create_dir_all(plist.parent().unwrap()).unwrap();
    fs::write(&plist, "<plist/>").unwrap();
    assert_user_error(
        &dir.run(&["stop"]),
        "launchctl could not stop dev.dps.live: Boot-out failed: 5: Input/output error",
    );
    assert!(plist.exists());
    let _ = Command::new("/bin/kill").arg(pid.to_string()).status();
}
