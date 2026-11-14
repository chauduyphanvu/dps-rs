//! What the chain costs: for each configuration, the CPU time per frame and
//! the fraction of one core it takes to keep up with real time.
//!
//! cargo run --release --example bench [-- --rate HZ] [--seconds N] [--block FRAMES]

use dps::profile::{BandKind, CorrectionBand, EQ_BANDS};
use dps::{Processor, Profile};
use std::f64::consts::TAU;
use std::hint::black_box;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant};

const USAGE: &str = "\
Usage: cargo run --release --example bench [-- OPTIONS]

Times the dps chain over a synthetic stereo programme, once per configuration.
Numbers from a debug build are meaningless: always use --release.

Options:
  --rate HZ        sample rate (default 48000)
  --seconds N      length of the programme in seconds (default 20)
  --block FRAMES   frames per process() call (default 512)
  -h, --help       print this help
";

const CHANNELS: usize = 2;
const RUNS: usize = 5;
const DEFAULT: &str = "default";
/// System volume given to every configuration whose loudness contour is on.
const CONTOUR_VOLUME_DB: f32 = -40.0;

const PRESETS: [(&str, &str); 4] = [
    ("headphones", include_str!("../presets/headphones.toml")),
    (
        "laptop-speakers",
        include_str!("../presets/laptop-speakers.toml"),
    ),
    ("music", include_str!("../presets/music.toml")),
    ("voice", include_str!("../presets/voice.toml")),
];

type Edit = fn(&mut Profile);

/// Each stage that is off by default, with the edit that turns it on with its
/// default parameters.
const STAGES: [(&str, Edit); 6] = [
    ("leveler", |p| p.leveler.enabled = true),
    ("bass", |p| p.bass.enabled = true),
    ("contour", |p| p.contour.enabled = true),
    ("crossfeed", |p| p.crossfeed.enabled = true),
    ("correction", |p| {
        p.correction.enabled = true;
        p.correction.bands = correction_bands();
    }),
    ("stereo", |p| p.stereo.enabled = true),
];

struct Args {
    rate: u32,
    seconds: f64,
    block: usize,
}

impl Args {
    fn frames(&self) -> usize {
        (f64::from(self.rate) * self.seconds).round() as usize
    }
}

struct Config {
    name: String,
    profile: Profile,
    /// One of the `STAGES` added to the default profile.
    single_stage: bool,
}

struct Row {
    name: String,
    single_stage: bool,
    ns_per_frame: f64,
    /// Half the min-max range of the runs.
    spread_ns: f64,
    realtime: f64,
    finite: bool,
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("bench: {e}\nTry --help.");
            return ExitCode::from(2);
        }
    };
    let configs = match configs(args.rate) {
        Ok(configs) => configs,
        Err(e) => {
            eprintln!("bench: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cfg!(debug_assertions) {
        println!("WARNING: DEBUG BUILD. These numbers are meaningless; use --release.\n");
    }

    let signal = programme(args.rate, args.frames());
    let rows: Vec<Row> = configs
        .iter()
        .map(|config| {
            eprintln!("measuring {} ...", config.name);
            measure(config, &args, &signal)
        })
        .collect();
    report(&rows, &args);

    if rows.iter().all(|r| r.finite) {
        ExitCode::SUCCESS
    } else {
        println!("! = the output had samples that were NaN or infinite");
        ExitCode::FAILURE
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut parsed = Args {
        rate: 48_000,
        seconds: 20.0,
        block: 512,
    };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--rate" => parsed.rate = positive(&flag, "whole number", args.next())?,
            "--seconds" => parsed.seconds = positive(&flag, "number", args.next())?,
            "--block" => parsed.block = positive(&flag, "whole number", args.next())?,
            _ => return Err(format!("unknown argument {flag:?}")),
        }
    }
    if !(parsed.seconds.is_finite() && parsed.frames() > 0) {
        return Err("--seconds must be finite and cover at least one frame at --rate".into());
    }
    Ok(Some(parsed))
}

fn positive<T: FromStr + PartialOrd + Default>(
    flag: &str,
    what: &str,
    text: Option<String>,
) -> Result<T, String> {
    let text = text.ok_or_else(|| format!("{flag} needs a value"))?;
    match text.parse::<T>() {
        Ok(v) if v > T::default() => Ok(v),
        _ => Err(format!("{flag} must be a positive {what}, got {text:?}")),
    }
}

/// Ten peaking bands log-spaced from 60 Hz to 12 kHz, alternating +3 and -3 dB.
fn correction_bands() -> Vec<CorrectionBand> {
    const COUNT: usize = 10;
    (0..COUNT)
        .map(|i| CorrectionBand {
            kind: BandKind::Peaking,
            freq_hz: 60.0 * (12_000.0f32 / 60.0).powf(i as f32 / (COUNT - 1) as f32),
            q: 1.4,
            gain_db: if i % 2 == 0 { 3.0 } else { -3.0 },
        })
        .collect()
}

/// The default profile with `edit` applied.
fn variant(name: &str, edit: impl FnOnce(&mut Profile)) -> Config {
    let mut profile = Profile::default();
    edit(&mut profile);
    Config {
        name: name.into(),
        profile,
        single_stage: false,
    }
}

/// Every configuration, each checked with `Profile::validate(rate)`.
fn configs(rate: u32) -> Result<Vec<Config>, String> {
    let mut configs = vec![
        variant("flat", |p| {
            p.input_gain_db = 0.0;
            p.output_gain_db = 0.0;
            p.low_shelf_db = 0.0;
            p.high_shelf_db = 0.0;
            p.compressor1.ratio = 1.0;
            p.compressor2.ratio = 1.0;
            p.eq.gains_db = [0.0; EQ_BANDS];
            p.leveler.enabled = false;
            p.bass.enabled = false;
            p.contour.enabled = false;
            p.stereo.enabled = false;
            p.crossfeed.enabled = false;
            p.correction.enabled = false;
            p.limiter.enabled = false;
        }),
        variant(DEFAULT, |_| {}),
        variant("default, limiter instant", |p| {
            p.limiter.lookahead_ms = 0.0;
            p.limiter.true_peak = false;
        }),
    ];
    for (stage, enable) in STAGES {
        let mut config = variant(&format!("+ {stage}"), enable);
        config.single_stage = true;
        configs.push(config);
    }
    configs.push(variant("everything on", |p| {
        for (_, enable) in STAGES {
            enable(p);
        }
    }));
    for (name, text) in PRESETS {
        let profile = toml::from_str(text).map_err(|e| format!("preset {name}: {e}"))?;
        configs.push(Config {
            name: format!("preset {name}"),
            profile,
            single_stage: false,
        });
    }
    for config in &configs {
        config
            .profile
            .validate(rate)
            .map_err(|e| format!("{}: {e}", config.name))?;
    }
    Ok(configs)
}

/// The generator of the `noise` helper in the tests of src/chain.rs: white
/// noise in [-0.5, 0.5).
struct Lcg(u64);

impl Lcg {
    fn sample(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 40) as f64 / (1u64 << 24) as f64 - 0.5
    }
}

/// Interleaved stereo: four tones and white noise, slowly swelling and fading
/// so that the compressors and the leveler have work to do. The envelope runs
/// between -30 and -3 dBFS peak at 0.5 Hz.
///
/// On top, a drum-like hit (-6 dBFS, centred) every 0.73 s, at the same level
/// whatever the swell. Without it the chain's output never gets near the
/// limiter's ceiling; a hit in a quiet stretch is a peak the compressors' 10 ms
/// attack lets through, and with the makeup gain it reaches the limiter.
fn programme(rate: u32, frames: usize) -> Vec<f32> {
    // Hz, then the amplitude in the left and the right channel.
    const TONES: [(f64, [f64; 2]); 4] = [
        (55.0, [0.50, 0.45]),
        (220.0, [0.35, 0.40]),
        (1000.0, [0.25, 0.22]),
        (6000.0, [0.10, 0.12]),
    ];
    const DETUNE: [f64; CHANNELS] = [1.0, 1.004];
    const NOISE_AMPLITUDE: f64 = 0.2;
    const ENVELOPE_HZ: f64 = 0.5;
    const HIT_PERIOD_S: f64 = 0.73;
    const HIT_PEAK: f64 = 0.5;
    const HIT_HZ: f64 = 80.0;
    const HIT_DECAY_S: f64 = 0.02;

    let fs = f64::from(rate);
    let mut noise = [Lcg(1), Lcg(2)];
    let mut samples = Vec::with_capacity(frames * CHANNELS);
    let mut peak = 0.0f32;
    for i in 0..frames {
        let t = i as f64 / fs;
        // 0 dB at the crest of the swell, -27 dB in the trough; scaled below.
        let envelope = 10f64.powf((-13.5 + 13.5 * (TAU * ENVELOPE_HZ * t).sin()) / 20.0);
        for (ch, lcg) in noise.iter_mut().enumerate() {
            let tones: f64 = TONES
                .iter()
                .map(|&(hz, amplitude)| amplitude[ch] * (TAU * hz * DETUNE[ch] * t).sin())
                .sum();
            let x = (tones + NOISE_AMPLITUDE * lcg.sample()) * envelope;
            peak = peak.max(x.abs() as f32);
            samples.push(x as f32);
        }
    }
    let scale = 10f32.powf(-3.0 / 20.0) / peak;
    for (i, frame) in samples.chunks_exact_mut(CHANNELS).enumerate() {
        let since_hit = (i as f64 / fs) % HIT_PERIOD_S;
        let hit = HIT_PEAK * (-since_hit / HIT_DECAY_S).exp() * (TAU * HIT_HZ * since_hit).sin();
        for x in frame {
            *x = *x * scale + hit as f32;
        }
    }
    samples
}

/// One pass of the whole signal through a fresh processor. Only the blocks
/// are timed; the output is checked for NaN and infinities afterwards.
fn pass(config: &Config, args: &Args, signal: &[f32]) -> (Duration, bool) {
    let mut work = signal.to_vec();
    let mut processor = Processor::new(&config.profile, args.rate, CHANNELS);
    processor.set_volume_db(config.profile.contour.enabled.then_some(CONTOUR_VOLUME_DB));
    let start = Instant::now();
    for block in work.chunks_mut(args.block * CHANNELS) {
        processor.process(black_box(block));
    }
    black_box(&work);
    let elapsed = start.elapsed();
    (elapsed, work.iter().all(|x| x.is_finite()))
}

fn measure(config: &Config, args: &Args, signal: &[f32]) -> Row {
    pass(config, args, signal);
    let mut finite = true;
    let mut seconds: Vec<f64> = (0..RUNS)
        .map(|_| {
            let (elapsed, ok) = pass(config, args, signal);
            finite &= ok;
            elapsed.as_secs_f64()
        })
        .collect();
    seconds.sort_by(f64::total_cmp);
    let mid = seconds.len() / 2;
    let median = if seconds.len() % 2 == 1 {
        seconds[mid]
    } else {
        (seconds[mid - 1] + seconds[mid]) / 2.0
    };
    let frames = args.frames() as f64;
    Row {
        name: config.name.clone(),
        single_stage: config.single_stage,
        ns_per_frame: median * 1e9 / frames,
        spread_ns: (seconds[seconds.len() - 1] - seconds[0]) / 2.0 * 1e9 / frames,
        realtime: frames / f64::from(args.rate) / median,
        finite,
    }
}

/// `x` to two significant figures.
fn two_sig_figs(x: f64) -> String {
    if !(x.is_finite() && x != 0.0) {
        return x.to_string();
    }
    let round = |v: f64| {
        let scale = 10f64.powi(1 - v.abs().log10().floor() as i32);
        (v * scale).round() / scale
    };
    let rounded = round(x);
    let decimals = (1 - rounded.abs().log10().floor() as i32).max(0) as usize;
    format!("{rounded:.decimals$}")
}

fn report(rows: &[Row], args: &Args) {
    let label = |r: &Row| {
        if r.finite {
            r.name.clone()
        } else {
            format!("{} !", r.name)
        }
    };
    let width = rows
        .iter()
        .map(|r| label(r).chars().count())
        .max()
        .unwrap_or(0)
        .max("configuration".len());

    println!("median of {RUNS} runs; spread is half the min-max range as a share of the median");
    let header = format!(
        "{:<width$}  {:>9}  {:>11}  {:>11}  {:>8}",
        "configuration", "ns/frame", "x real time", "% of 1 core", "spread"
    );
    println!("{header}\n{}", "-".repeat(header.chars().count()));
    for r in rows {
        println!(
            "{:<width$}  {:>9.1}  {:>11.1}  {:>11}  {:>8}",
            label(r),
            r.ns_per_frame,
            r.realtime,
            two_sig_figs(100.0 / r.realtime),
            format!("±{:.1}%", 100.0 * r.spread_ns / r.ns_per_frame),
        );
    }
    println!();

    let default = rows
        .iter()
        .find(|r| r.name == DEFAULT)
        .expect("the default configuration is always measured");
    let worst = rows
        .iter()
        .filter(|r| r.single_stage)
        .max_by(|a, b| a.ns_per_frame.total_cmp(&b.ns_per_frame))
        .expect("every single-stage configuration is measured");
    let delta = worst.ns_per_frame - default.ns_per_frame;
    // ns per frame x frames per second, as a percentage of a second.
    let core_pct = delta * f64::from(args.rate) * 1e-7;
    let plus = |v: f64| if v >= 0.0 { "+" } else { "" };
    let noise = if delta.abs() <= default.spread_ns + worst.spread_ns {
        " (within the run-to-run spread, so no measurable cost)"
    } else {
        ""
    };
    println!(
        "Most expensive added stage: {} at {}{delta:.1} ns/frame over default \
         ({}{}% of one core, {}{}% of default's cost){noise}",
        worst.name,
        plus(delta),
        plus(core_pct),
        two_sig_figs(core_pct),
        plus(delta),
        two_sig_figs(100.0 * delta / default.ns_per_frame),
    );

    let build = if cfg!(debug_assertions) {
        "DEBUG BUILD, THE NUMBERS ARE MEANINGLESS: USE --release"
    } else {
        "release build"
    };
    println!(
        "{build}; rate {} Hz, block {} frames, {} s, {}",
        args.rate,
        args.block,
        args.seconds,
        std::env::consts::ARCH,
    );
}
