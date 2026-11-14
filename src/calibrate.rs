//! `dps calibrate` without the hardware: the test tones, the analysis of
//! their recording, and the correction fitted to it.
//!
//! Each third-octave band from 100 Hz to 8 kHz gets a warble tone, a sine
//! sweeping up and down across the band four times a second, which spreads
//! over the room modes a pure tone would land on. A band's level is the power
//! of the recording through a band filter, less the room's noise through that
//! filter, relative to what the same filter makes of the tone itself at the
//! recording's rate. So the analysis assumes nothing about the two devices'
//! rates, clocks or latency: it finds the tones in the recording by
//! itself. Up to eight peaking filters fitted to the inverse of the levels
//! make the correction, deliberately limited in range and boost, since a
//! built-in microphone in a room is no measurement rig.

use std::f64::consts::{LN_2, PI, SQRT_2, TAU};
use std::fmt;
use std::ops::RangeInclusive;

use crate::biquad::{Biquad, Coefs};
use crate::profile::{BandKind, CorrectionBand, CorrectionParams};

pub const BANDS: usize = 20;

/// Third-octave centres, at their nominal values. Below 100 Hz room modes
/// and small microphones decide what is measured; above 8 kHz the
/// microphone's directivity does.
pub const CENTRES_HZ: [f64; BANDS] = [
    100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0,
    2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0,
];

/// The order the bands play in, as indices into `CENTRES_HZ`. Tones one or
/// two apart in time are at least two octaves apart, outside each other's
/// band filters, so neither the tail of one tone nor a latency misjudged by
/// a tone or two passes for another.
const ORDER: [usize; BANDS] = [
    0, 7, 14, 1, 8, 15, 2, 9, 16, 3, 10, 17, 4, 11, 18, 5, 12, 19, 6, 13,
];

pub const DEFAULT_LEVEL_DB: f64 = -30.0;
/// The RMS levels in dBFS `--level` takes.
pub const LEVELS_DB: RangeInclusive<f64> = -60.0..=-12.0;

// Times, in seconds, are whole multiples of 10 ms so that they are whole
// frames at every common sample rate.
/// Silence before the first tone, where the room's noise is measured.
const LEAD_S: f64 = 1.0;
const TONE_S: f64 = 1.1;
/// Silence after each tone while the room dies away.
const GAP_S: f64 = 0.4;
const SLOT_S: f64 = TONE_S + GAP_S;
const FADE_S: f64 = 0.02;
/// One sweep of the warble, up and back down.
const SWEEP_S: f64 = 0.25;
/// The start of each tone is left out while the room builds up to it.
const SETTLE_S: f64 = 0.25;
/// What is measured of each tone: three whole sweeps, ending well before
/// the fade-out.
const WINDOW_S: f64 = 3.0 * SWEEP_S;
/// What is left out at both ends of the silence before the first tone.
const NOISE_MARGIN_S: f64 = 0.05;
/// The latencies looked for, from the output device's first callback to its
/// sound in the recording: from a little early (the callbacks run on two
/// clocks) to well past Bluetooth's and AirPlay's.
const EARLIEST_S: f64 = -0.1;
const LATEST_S: f64 = 2.5;
/// How long a band filter runs before what it outputs is used.
const FILTER_SETTLE_S: f64 = 0.1;

/// 2^(1/3): a band filter's corners are this far either side of its centre,
/// and the tone sweeps half as far.
const EDGE: f64 = 1.259_921_049_894_873_2;
/// The two stages of a fourth-order Butterworth filter.
const BUTTERWORTH_Q: [f64; 2] = [0.541_196_100_146_197, 1.306_562_964_876_376_5];

/// A band is usable this far above the room's noise.
const MIN_SNR_DB: f64 = 10.0;
const MIN_USABLE: usize = 8;
/// The bands whose mean level the deviations are taken from, so that the
/// correction leaves the overall loudness alone.
const REFERENCE_HZ: RangeInclusive<f64> = 300.0..=3000.0;
/// Boosting a null wastes headroom and never fills it.
pub const MAX_BOOST_DB: f64 = 6.0;
pub const MAX_CUT_DB: f64 = 10.0;
const MAX_FILTERS: usize = 8;
const Q_RANGE: RangeInclusive<f64> = 0.5..=4.0;
/// A new filter starts from whichever of these fits best.
const Q_START: [f64; 4] = [0.5, 1.0, 2.0, 4.0];
/// No filter is added once every band is this close to what it wants.
const CLOSE_DB: f64 = 1.0;
const MIN_GAIN_DB: f64 = 0.5;
const MAX_PASSES: usize = 500;

/// Parses `--level`.
pub fn parse_level(text: &str) -> Result<f64, String> {
    match text.trim().parse::<f64>() {
        Ok(db) if LEVELS_DB.contains(&db) => Ok(db),
        _ => Err(format!(
            "expected a level in dBFS from {} to {}",
            LEVELS_DB.start(),
            LEVELS_DB.end()
        )),
    }
}

/// What is played and how it is recorded.
#[derive(Debug, Clone)]
pub struct Plan {
    level_db: f64,
    output_rate: f64,
    input_rate: f64,
    /// Whether each band fits both rates; one that does not stays silent.
    fits: [bool; BANDS],
}

impl Plan {
    /// Tones at an RMS level of `level_db` dBFS, played at `output_rate` and
    /// recorded at `input_rate`.
    pub fn new(level_db: f64, output_rate: f64, input_rate: f64) -> Self {
        // Room for the band filter's upper corner below the lower Nyquist.
        let lower = output_rate.min(input_rate);
        Self {
            level_db,
            output_rate,
            input_rate,
            fits: CENTRES_HZ.map(|centre| centre * EDGE < 0.45 * lower),
        }
    }

    pub fn level_db(&self) -> f64 {
        self.level_db
    }

    /// How long the playback lasts.
    pub fn seconds(&self) -> f64 {
        LEAD_S + BANDS as f64 * SLOT_S
    }

    /// How long to record after the playback starts: the playback, and the
    /// longest latency looked for.
    pub fn recording_seconds(&self) -> f64 {
        self.seconds() + LATEST_S
    }

    /// The frames `analyse` needs after the anchor, at the input rate.
    pub fn recording_frames(&self) -> usize {
        (self.recording_seconds() * self.input_rate).ceil() as usize
    }

    /// The largest magnitude the playback reaches: a sine's peak at the
    /// level, 3 dB above it.
    pub fn peak(&self) -> f32 {
        amplitude(self.level_db) as f32
    }

    /// The tone playing or dying away `seconds` after the playback started,
    /// as its place in the order (from 0) and its centre; `None` before the
    /// first and after the last.
    pub fn playing(&self, seconds: f64) -> Option<(usize, f64)> {
        if seconds < LEAD_S {
            return None;
        }
        let slot = ((seconds - LEAD_S) / SLOT_S) as usize;
        (slot < BANDS).then(|| (slot, CENTRES_HZ[ORDER[slot]]))
    }

    /// The playback at `rate`, one channel. Deterministic: the same plan
    /// and rate give the same samples.
    pub fn render(&self, rate: f64) -> Vec<f32> {
        let mut out = vec![0.0; frames(self.seconds(), rate)];
        let amplitude = amplitude(self.level_db);
        let len = frames(TONE_S, rate);
        for (band, &centre) in CENTRES_HZ.iter().enumerate() {
            if !self.fits[band] {
                continue;
            }
            let first = frames(tone_start(band), rate);
            let mut phase = 0.0f64;
            for (i, x) in out[first..first + len].iter_mut().enumerate() {
                let t = i as f64 / rate;
                *x = (amplitude * fade(t) * phase.sin()) as f32;
                let octaves = triangle(t / SWEEP_S) / 6.0;
                phase = (phase + TAU * centre * (octaves * LN_2).exp() / rate) % TAU;
            }
        }
        out
    }
}

fn amplitude(level_db: f64) -> f64 {
    10f64.powf(level_db / 20.0) * SQRT_2
}

fn frames(seconds: f64, rate: f64) -> usize {
    (seconds * rate).round() as usize
}

/// When `band`'s tone starts, from the start of the playback.
fn tone_start(band: usize) -> f64 {
    let slot = ORDER
        .iter()
        .position(|&b| b == band)
        .expect("every band plays");
    LEAD_S + slot as f64 * SLOT_S
}

/// Raised-cosine fades at both ends of a tone `t` seconds in, so that it
/// starts and stops without a click.
fn fade(t: f64) -> f64 {
    let edge = (t.min(TONE_S - t) / FADE_S).max(0.0);
    if edge >= 1.0 {
        1.0
    } else {
        0.5 - 0.5 * (PI * edge).cos()
    }
}

/// From 0 up to 1, down to -1 and back to 0 over each whole number.
fn triangle(x: f64) -> f64 {
    let u = x.fract();
    if u < 0.25 {
        4.0 * u
    } else if u < 0.75 {
        2.0 - 4.0 * u
    } else {
        4.0 * u - 4.0
    }
}

/// A fourth-order Butterworth high-pass a third of an octave below
/// `centre` and low-pass a third of an octave above it: 1.3 to 1.7 dB down
/// across the tone's band, the same at every centre, and 15 dB further down
/// at the tone's second harmonic.
fn band_filter(centre: f64, rate: f64) -> [Coefs; 4] {
    let (low, high) = (centre / EDGE, centre * EDGE);
    [
        Coefs::high_pass(low, BUTTERWORTH_Q[0], rate),
        Coefs::high_pass(low, BUTTERWORTH_Q[1], rate),
        Coefs::low_pass(high, BUTTERWORTH_Q[0], rate),
        Coefs::low_pass(high, BUTTERWORTH_Q[1], rate),
    ]
}

/// `samples[from..to]` through a band filter that has run on the samples
/// up to `settle` frames before them.
fn filtered(
    samples: &[f32],
    coefs: &[Coefs; 4],
    from: usize,
    to: usize,
    settle: usize,
) -> Vec<f32> {
    let mut filters = coefs.map(Biquad::new);
    let mut tick = |x: f32| filters.iter_mut().fold(x, |y, f| f.tick(y));
    for &x in &samples[from.saturating_sub(settle)..from] {
        tick(x);
    }
    samples[from..to].iter().map(|&x| tick(x)).collect()
}

/// Mean power of `len` frames from `from` through a band filter.
fn power(samples: &[f32], coefs: &[Coefs; 4], from: usize, len: usize, settle: usize) -> f64 {
    let y = filtered(samples, coefs, from, from + len, settle);
    y.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / len as f64
}

/// Why a band has no measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unusable {
    /// It came through less than 10 dB above the room's noise.
    Noise,
    /// It is too high for one of the devices' sample rates, so it was not
    /// played.
    Range,
}

/// One band's measurement.
#[derive(Debug, Clone, PartialEq)]
pub struct Band {
    pub centre_hz: f64,
    /// How far the band came through above the room's noise; `None` if it
    /// was not played.
    pub snr_db: Option<f64>,
    /// Its level less the mean level of the usable bands from 300 Hz to
    /// 3 kHz; `None` if it is unusable.
    pub deviation_db: Option<f64>,
    /// What the fit aimed at: the deviation's inverse, limited to
    /// `MAX_BOOST_DB` of boost and `MAX_CUT_DB` of cut.
    pub wanted_db: Option<f64>,
    /// What the fitted filters do at the centre, without the preamp.
    pub applied_db: f64,
    pub unusable: Option<Unusable>,
}

/// What `analyse` found.
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    /// In the order of `CENTRES_HZ`.
    pub bands: Vec<Band>,
    pub correction: CorrectionParams,
    /// From the anchor to the playback's start in the recording.
    pub latency_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The recording is shorter than `Plan::recording_frames` after the
    /// anchor.
    Short { recorded_s: f64, needed_s: f64 },
    /// Every recorded sample is exactly zero.
    Silent,
    /// Fewer than 8 bands are usable, or none from 300 Hz to 3 kHz.
    TooQuiet { usable: usize, level_db: f64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short {
                recorded_s,
                needed_s,
            } => write!(
                f,
                "the recording ended after {recorded_s:.1} s, before the test tones did \
                 ({needed_s:.1} s)"
            ),
            Self::Silent => write!(
                f,
                "the microphone recorded nothing but exact silence, which is what macOS gives \
                 an app not allowed to use it: allow your terminal app in System Settings > \
                 Privacy & Security > Microphone, then run dps calibrate again"
            ),
            Self::TooQuiet { usable, level_db } => write!(
                f,
                "only {usable} of the {BANDS} test tones came through at least {MIN_SNR_DB} dB \
                 above the room's noise, and {MIN_USABLE} are needed (some from 300 Hz to \
                 3 kHz): the tones were too quiet for this room; raise the system volume or \
                 --level (now {level_db} dBFS), or make the room quieter"
            ),
        }
    }
}

/// Measures the playback of `plan` in `recording`, a single channel at the
/// plan's input rate, and fits a correction to it. `recording[anchor]` is
/// what the microphone heard when the output device started on the
/// playback; the latency after that is found here.
pub fn analyse(plan: &Plan, recording: &[f32], anchor: usize) -> Result<Calibration, Error> {
    let rate = plan.input_rate;
    let needed = anchor + plan.recording_frames();
    if recording.len() < needed {
        return Err(Error::Short {
            recorded_s: recording.len().saturating_sub(anchor) as f64 / rate,
            needed_s: plan.recording_seconds(),
        });
    }
    if recording.iter().all(|&x| x == 0.0) {
        return Err(Error::Silent);
    }
    // What a flat, silent room with no latency would give: the tones
    // themselves. It goes through the same search, so that what the band
    // filters delay is no latency, and their windows line up.
    let mut ideal = plan.render(rate);
    ideal.resize(plan.recording_frames(), 0.0);
    let ideal_lag = find_lag(plan, &ideal, 0) as usize;
    let lag = find_lag(plan, recording, anchor);
    // `find_lag` keeps `anchor + lag` at or above 0.
    let at = |seconds: f64| (anchor as isize + lag) as usize + frames(seconds, rate);
    let settle = frames(FILTER_SETTLE_S, rate);
    let window = frames(WINDOW_S, rate);
    let noise_len = frames(LEAD_S - 2.0 * NOISE_MARGIN_S, rate);

    // Each band, and its level in dB if it is usable.
    let measured: Vec<(Band, Option<f64>)> = CENTRES_HZ
        .iter()
        .enumerate()
        .map(|(band, &centre_hz)| {
            let mut result = Band {
                centre_hz,
                snr_db: None,
                deviation_db: None,
                wanted_db: None,
                applied_db: 0.0,
                unusable: Some(Unusable::Range),
            };
            if !plan.fits[band] {
                return (result, None);
            }
            let coefs = band_filter(centre_hz, rate);
            let start = tone_start(band) + SETTLE_S;
            let heard = power(recording, &coefs, at(start), window, settle);
            let noise = power(recording, &coefs, at(NOISE_MARGIN_S), noise_len, settle);
            let reference = power(
                &ideal,
                &coefs,
                ideal_lag + frames(start, rate),
                window,
                settle,
            );
            let snr_db = 10.0 * (heard / noise).log10();
            result.snr_db = Some(snr_db);
            // Written so that 0 / 0 (NaN) counts as too quiet.
            if !(heard > 0.0 && snr_db >= MIN_SNR_DB) {
                result.unusable = Some(Unusable::Noise);
                return (result, None);
            }
            result.unusable = None;
            (result, Some(10.0 * ((heard - noise) / reference).log10()))
        })
        .collect();

    let usable = measured.iter().filter(|(_, level)| level.is_some()).count();
    let reference: Vec<f64> = measured
        .iter()
        .filter(|(band, _)| REFERENCE_HZ.contains(&band.centre_hz))
        .filter_map(|(_, level)| *level)
        .collect();
    if usable < MIN_USABLE || reference.is_empty() {
        return Err(Error::TooQuiet {
            usable,
            level_db: plan.level_db,
        });
    }
    let mean = reference.iter().sum::<f64>() / reference.len() as f64;
    let mut targets = Vec::new();
    let mut bands = Vec::with_capacity(BANDS);
    for (mut band, level) in measured {
        if let Some(level) = level {
            let deviation = level - mean;
            let wanted = (-deviation).clamp(-MAX_CUT_DB, MAX_BOOST_DB);
            band.deviation_db = Some(deviation);
            band.wanted_db = Some(wanted);
            targets.push((band.centre_hz, wanted));
        }
        bands.push(band);
    }

    let fit_rate = plan.output_rate;
    let filters = fit(&targets, fit_rate);
    for band in &mut bands {
        band.applied_db = curve_db(&filters, band.centre_hz, fit_rate);
    }
    let highest = max_curve_db(&filters, fit_rate);
    let preamp_db = if highest > 0.0 {
        -(highest * 10.0).ceil() / 10.0
    } else {
        0.0
    };
    Ok(Calibration {
        bands,
        correction: CorrectionParams {
            enabled: true,
            preamp_db: preamp_db as f32,
            bands: filters,
        },
        latency_s: (lag - ideal_lag as isize) as f64 / rate,
    })
}

/// The latency, in recording frames after the anchor, that lines the
/// tones up best with the recording, to the nearest millisecond. Each
/// band's power through its filter over a tone's length is taken at every
/// latency looked for; the latencies are scored by the sum over bands of
/// that power's rise above its lowest, relative to its highest, so that a
/// band lost in noise adds little.
fn find_lag(plan: &Plan, recording: &[f32], anchor: usize) -> isize {
    let rate = plan.input_rate;
    let step = frames(0.001, rate).max(1);
    let earliest = (EARLIEST_S * rate).round() as isize;
    let earliest = earliest.max(-(anchor as isize));
    let latest = frames(LATEST_S, rate) as isize;
    let lags: Vec<isize> = (earliest..=latest).step_by(step).collect();
    let tone = frames(TONE_S, rate);
    let settle = frames(FILTER_SETTLE_S, rate);
    let mut score = vec![0.0; lags.len()];
    for (band, &centre) in CENTRES_HZ.iter().enumerate() {
        if !plan.fits[band] {
            continue;
        }
        let start = anchor as isize + frames(tone_start(band), rate) as isize;
        let from = (start + earliest) as usize;
        let to = (start + latest) as usize + tone;
        let y = filtered(recording, &band_filter(centre, rate), from, to, settle);
        let mut prefix = Vec::with_capacity(y.len() + 1);
        prefix.push(0.0f64);
        for v in &y {
            let last = prefix[prefix.len() - 1];
            prefix.push(last + f64::from(*v).powi(2));
        }
        let energy: Vec<f64> = lags
            .iter()
            .map(|&lag| {
                let i = (start + lag) as usize - from;
                prefix[i + tone] - prefix[i]
            })
            .collect();
        let lowest = energy.iter().copied().fold(f64::INFINITY, f64::min);
        let highest = energy.iter().copied().fold(0.0, f64::max);
        if highest > 0.0 {
            for (s, e) in score.iter_mut().zip(&energy) {
                *s += (e - lowest) / highest;
            }
        }
    }
    let best = (0..lags.len())
        .reduce(|best, i| if score[i] > score[best] { i } else { best })
        .unwrap_or(0);
    lags[best]
}

/// One peaking filter while it is being fitted.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Peak {
    freq: f64,
    q: f64,
    gain: f64,
}

impl Peak {
    fn coefs(&self, rate: f64) -> Coefs {
        Coefs::peaking(self.freq, self.q, self.gain, rate)
    }

    fn response(&self, freqs: &[f64], rate: f64) -> Vec<f64> {
        let coefs = self.coefs(rate);
        freqs.iter().map(|&f| coefs.magnitude_db(f, rate)).collect()
    }

    /// This filter with one of its frequency, Q (both by a factor of
    /// e^`step`) or gain (by `step` dB) moved, kept within its limits.
    fn nudged(self, param: usize, step: f64, freqs: &RangeInclusive<f64>) -> Self {
        let mut p = self;
        match param {
            0 => {
                p.freq = (p.freq.ln() + step)
                    .exp()
                    .clamp(*freqs.start(), *freqs.end())
            }
            1 => {
                p.q = (p.q.ln() + step)
                    .exp()
                    .clamp(*Q_RANGE.start(), *Q_RANGE.end())
            }
            _ => p.gain = (p.gain + step).clamp(-MAX_CUT_DB, MAX_BOOST_DB),
        }
        p
    }
}

fn sum_db(peaks: &[Peak], freq: f64, rate: f64) -> f64 {
    peaks
        .iter()
        .map(|p| p.coefs(rate).magnitude_db(freq, rate))
        .sum()
}

fn squared_error(peaks: &[Peak], targets: &[(f64, f64)], rate: f64) -> f64 {
    targets
        .iter()
        .map(|&(f, want)| (sum_db(peaks, f, rate) - want).powi(2))
        .sum()
}

/// Peaking filters whose summed response comes close to `targets`, pairs
/// of frequency and gain in dB, at their frequencies. Greedy: a filter goes
/// where the error is largest, then all of them are refined together, until
/// every target is within `CLOSE_DB` or there are `MAX_FILTERS`. Filters
/// under `MIN_GAIN_DB` are dropped; the boost is capped at `MAX_BOOST_DB`.
fn fit(targets: &[(f64, f64)], rate: f64) -> Vec<CorrectionBand> {
    let lowest = targets.iter().map(|t| t.0).fold(f64::INFINITY, f64::min);
    let highest = targets.iter().map(|t| t.0).fold(0.0, f64::max);
    let freqs = lowest..=highest;
    let mut peaks: Vec<Peak> = Vec::new();
    while peaks.len() < MAX_FILTERS {
        let Some((freq, error)) = targets
            .iter()
            .map(|&(f, want)| (f, want - sum_db(&peaks, f, rate)))
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        else {
            break;
        };
        if error.abs() < CLOSE_DB {
            break;
        }
        let gain = error.clamp(-MAX_CUT_DB, MAX_BOOST_DB);
        let new = Q_START
            .map(|q| Peak { freq, q, gain })
            .into_iter()
            .min_by(|a, b| {
                let with = |p: &Peak| {
                    let mut all = peaks.clone();
                    all.push(*p);
                    squared_error(&all, targets, rate)
                };
                with(a).total_cmp(&with(b))
            })
            .expect("Q_START is not empty");
        peaks.push(new);
        refine(&mut peaks, targets, rate, &freqs);
    }
    loop {
        let before = peaks.len();
        peaks.retain(|p| p.gain.abs() >= MIN_GAIN_DB);
        if peaks.len() == before {
            break;
        }
        refine(&mut peaks, targets, rate, &freqs);
    }
    let mut bands: Vec<CorrectionBand> = peaks
        .iter()
        .map(|p| CorrectionBand {
            kind: BandKind::Peaking,
            freq_hz: p.freq.round() as f32,
            q: ((p.q * 100.0).round() / 100.0) as f32,
            gain_db: ((p.gain * 10.0).round() / 10.0) as f32,
        })
        .collect();
    cap_boost(&mut bands, rate);
    bands.retain(|b| f64::from(b.gain_db.abs()) >= MIN_GAIN_DB);
    bands.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    bands
}

/// Moves each filter's frequency, Q and gain in turn while that lowers the
/// squared error at the targets, in steps that halve whenever no move
/// helps (a pattern search).
fn refine(peaks: &mut [Peak], targets: &[(f64, f64)], rate: f64, freqs: &RangeInclusive<f64>) {
    let at: Vec<f64> = targets.iter().map(|t| t.0).collect();
    let error = |sum: &[f64]| -> f64 {
        sum.iter()
            .zip(targets)
            .map(|(s, &(_, want))| (s - want).powi(2))
            .sum()
    };
    let mut rows: Vec<Vec<f64>> = peaks.iter().map(|p| p.response(&at, rate)).collect();
    let mut sum: Vec<f64> = (0..at.len())
        .map(|k| rows.iter().map(|row| row[k]).sum())
        .collect();
    let mut best = error(&sum);
    let mut trial_sum = vec![0.0; at.len()];
    // Frequency and Q by a sixth of an octave and a factor of 1.41, gain by
    // 1 dB, down to a hundredth of that.
    let mut steps = [LN_2 / 6.0, LN_2 / 2.0, 1.0];
    for _ in 0..MAX_PASSES {
        let mut improved = false;
        for i in 0..peaks.len() {
            for (param, &step) in steps.iter().enumerate() {
                for direction in [1.0, -1.0] {
                    let trial = peaks[i].nudged(param, direction * step, freqs);
                    if trial == peaks[i] {
                        continue;
                    }
                    let row = trial.response(&at, rate);
                    for (t, ((s, old), new)) in
                        trial_sum.iter_mut().zip(sum.iter().zip(&rows[i]).zip(&row))
                    {
                        *t = s - old + new;
                    }
                    let e = error(&trial_sum);
                    if e < best {
                        best = e;
                        peaks[i] = trial;
                        rows[i] = row;
                        sum.copy_from_slice(&trial_sum);
                        improved = true;
                        break;
                    }
                }
            }
        }
        if !improved {
            if steps[2] < 0.01 {
                break;
            }
            steps = steps.map(|s| s / 2.0);
        }
    }
}

/// The filters' summed response in dB at `freq`.
fn curve_db(bands: &[CorrectionBand], freq: f64, rate: f64) -> f64 {
    bands
        .iter()
        .map(|b| {
            Coefs::peaking(
                f64::from(b.freq_hz),
                f64::from(b.q),
                f64::from(b.gain_db),
                rate,
            )
            .magnitude_db(freq, rate)
        })
        .sum()
}

/// The highest the filters' summed response goes from 20 Hz to 20 kHz (or
/// just under Nyquist), on a grid of 1/48 octave.
fn max_curve_db(bands: &[CorrectionBand], rate: f64) -> f64 {
    let top = 20_000f64.min(0.49 * rate);
    (0..)
        .map(|i| 20.0 * 2f64.powf(f64::from(i) / 48.0))
        .take_while(|&f| f <= top)
        .map(|f| curve_db(bands, f, rate))
        .fold(f64::NEG_INFINITY, f64::max)
}

/// Scales the boosting filters down until the summed response stays at or
/// under `MAX_BOOST_DB`: filters that overlap can add up to more than any
/// one of them. Gains are rounded down to 0.1 dB, so every round lowers
/// them and the loop ends.
fn cap_boost(bands: &mut [CorrectionBand], rate: f64) {
    loop {
        let highest = max_curve_db(bands, rate);
        if highest <= MAX_BOOST_DB {
            return;
        }
        let scale = MAX_BOOST_DB / highest;
        for band in bands.iter_mut().filter(|b| b.gain_db > 0.0) {
            band.gain_db = ((f64::from(band.gain_db) * scale * 10.0).floor() / 10.0) as f32;
        }
    }
}

impl Calibration {
    /// The bands as a table, then the fitted filters.
    pub fn table(&self) -> String {
        let mut text = String::from("   band  deviation  correction  above noise\n");
        for band in &self.bands {
            let centre = format!("{} Hz", band.centre_hz);
            let deviation = band
                .deviation_db
                .map_or("-".into(), |d| format!("{d:+.1} dB"));
            let snr = band.snr_db.map_or("-".into(), |s| format!("{s:.0} dB"));
            let note = match band.unusable {
                None => "",
                Some(Unusable::Noise) => "  unusable: less than 10 dB above the room's noise",
                Some(Unusable::Range) => "  unusable: too high for the devices' sample rates",
            };
            text += &format!(
                "{centre:>7}  {deviation:>9}  {:>10}  {snr:>11}{note}\n",
                format!("{:+.1} dB", band.applied_db)
            );
        }
        let filters = &self.correction.bands;
        text += &format!(
            "\ncorrection: a preamp of {:.1} dB and {} peaking filter{}\n",
            self.correction.preamp_db,
            filters.len(),
            if filters.len() == 1 { "" } else { "s" }
        );
        for band in filters {
            text += &format!(
                "  {:>5} Hz  Q {:<4}  {:+.1} dB\n",
                band.freq_hz, band.q, band.gain_db
            );
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::correction::Correction;
    use crate::profile::Profile;

    const FS: f64 = 48_000.0;

    /// Deterministic values in [-1, 1).
    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    /// A speaker and room for `record`.
    struct Room {
        /// The speaker's response, as filters at the given rate.
        speaker: fn(f64) -> Vec<Coefs>,
        /// Frames, at the input rate, from the anchor to the playback.
        delay: usize,
        /// White noise, RMS.
        hiss: f64,
        /// Noise under about 120 Hz, RMS.
        rumble: f64,
        /// How much faster the microphone's clock runs than its rate says.
        clock_ppm: f64,
    }

    impl Default for Room {
        fn default() -> Self {
            Self {
                speaker: |_| Vec::new(),
                delay: 300,
                hiss: 1e-4,
                rumble: 0.0,
                clock_ppm: 0.0,
            }
        }
    }

    const ANCHOR_S: f64 = 0.2;

    /// What the microphone records of `plan`'s playback in `room`, played
    /// through `correction` if given, and the anchor. The room's sound is
    /// rendered at the microphone's real rate, which is how a continuous
    /// signal would be sampled by it.
    fn record(
        plan: &Plan,
        room: &Room,
        correction: Option<&CorrectionParams>,
    ) -> (Vec<f32>, usize) {
        let rate = plan.input_rate * (1.0 + room.clock_ppm * 1e-6);
        let mut sound = plan.render(rate);
        if let Some(params) = correction {
            let mut stage = Correction::new(params, rate, 1);
            for x in &mut sound {
                stage.frame(std::slice::from_mut(x));
            }
        }
        for coefs in (room.speaker)(rate) {
            let mut filter = Biquad::new(coefs);
            for x in &mut sound {
                *x = filter.tick(*x);
            }
        }
        let anchor = frames(ANCHOR_S, plan.input_rate);
        let len = anchor + plan.recording_frames() + 100;
        let mut recording = vec![0.0f32; len];
        let start = anchor + room.delay;
        let n = sound.len().min(len - start);
        recording[start..start + n].copy_from_slice(&sound[..n]);
        if room.hiss > 0.0 {
            let hiss = noise(len, 1);
            let scale = room.hiss / rms(&hiss);
            for (r, h) in recording.iter_mut().zip(&hiss) {
                *r += (f64::from(*h) * scale) as f32;
            }
        }
        if room.rumble > 0.0 {
            let mut rumble = noise(len, 2);
            for _ in 0..2 {
                let mut filter = Biquad::new(Coefs::low_pass(120.0, 0.707, plan.input_rate));
                for x in &mut rumble {
                    *x = filter.tick(*x);
                }
            }
            let scale = room.rumble / rms(&rumble);
            for (r, h) in recording.iter_mut().zip(&rumble) {
                *r += (f64::from(*h) * scale) as f32;
            }
        }
        (recording, anchor)
    }

    fn measure(plan: &Plan, room: &Room, correction: Option<&CorrectionParams>) -> Calibration {
        let (recording, anchor) = record(plan, room, correction);
        analyse(plan, &recording, anchor).unwrap_or_else(|e| panic!("{e}"))
    }

    fn band(calibration: &Calibration, centre: f64) -> &Band {
        calibration
            .bands
            .iter()
            .find(|b| b.centre_hz == centre)
            .unwrap()
    }

    /// The largest deviation of a usable band from flat.
    fn worst(calibration: &Calibration) -> f64 {
        calibration
            .bands
            .iter()
            .filter_map(|b| b.deviation_db)
            .fold(0.0, |w, d| w.max(d.abs()))
    }

    /// A speaker with a boomy bass, a dip, a presence peak and falling
    /// treble, none past what the correction may undo.
    fn speaker(rate: f64) -> Vec<Coefs> {
        vec![
            Coefs::peaking(200.0, 1.0, 4.0, rate),
            Coefs::peaking(1500.0, 2.0, -5.0, rate),
            Coefs::peaking(4000.0, 1.5, 3.0, rate),
            Coefs::high_shelf(6000.0, 0.7, -3.0, rate),
        ]
    }

    #[test]
    fn the_bands_are_the_third_octaves_from_100_hz_to_8_khz() {
        assert_eq!((CENTRES_HZ[0], CENTRES_HZ[BANDS - 1]), (100.0, 8000.0));
        for pair in CENTRES_HZ.windows(2) {
            let ratio = pair[1] / pair[0];
            assert!((ratio / EDGE - 1.0).abs() < 0.03, "{pair:?}");
        }
        assert!((EDGE.powi(3) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn tones_close_in_time_are_two_octaves_apart_in_frequency() {
        let mut sorted = ORDER;
        sorted.sort_unstable();
        assert_eq!(sorted, std::array::from_fn(|i| i));
        for apart in [1, 2] {
            for i in 0..BANDS - apart {
                let gap = ORDER[i].abs_diff(ORDER[i + apart]);
                assert!(gap >= 6, "slots {i} and {}", i + apart);
            }
        }
        // The latencies looked for stay clear of three slots, where the
        // order next puts a neighbouring band.
        const { assert!(LATEST_S - EARLIEST_S < 3.0 * SLOT_S - TONE_S) };
    }

    #[test]
    fn the_measured_part_of_a_tone_is_three_sweeps_inside_its_steady_part() {
        const {
            assert!(SETTLE_S >= FADE_S + 0.2);
            assert!(SETTLE_S + WINDOW_S <= TONE_S - FADE_S - 0.05);
            assert!(LEAD_S > 2.0 * NOISE_MARGIN_S + 0.5);
        };
    }

    #[test]
    fn rendering_is_deterministic_and_never_passes_the_peak() {
        let plan = Plan::new(-30.0, FS, FS);
        let a = plan.render(FS);
        assert_eq!(a, plan.render(FS));
        assert_eq!(a.len(), 31 * 48_000);
        let peak = a.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= plan.peak(), "{peak} > {}", plan.peak());
        assert!(peak > 0.999 * plan.peak());
        // -30 dBFS RMS is a peak of -27 dBFS.
        assert!((db(f64::from(plan.peak())) - -26.99).abs() < 0.01);
        // The lead is silent.
        assert!(a[..48_000].iter().all(|&x| x == 0.0));
        // Other levels and rates.
        let loud = Plan::new(-12.0, 44_100.0, 48_000.0);
        let b = loud.render(44_100.0);
        assert_eq!(b.len(), 31 * 44_100);
        assert!(b.iter().all(|x| x.abs() <= loud.peak()));
    }

    #[test]
    fn each_tone_is_at_the_level_and_fades_in_and_out() {
        for level in [-60.0, -30.0, -12.0] {
            let plan = Plan::new(level, FS, FS);
            let x = plan.render(FS);
            for band in [0, 10, 19] {
                let first = frames(tone_start(band), FS);
                let steady = &x[first + frames(SETTLE_S, FS)..][..frames(WINDOW_S, FS)];
                assert!((db(rms(steady)) - level).abs() < 0.01, "{level} {band}");
                // Silence either side, and a fade with no step at either end.
                assert_eq!(x[first - 1], 0.0);
                assert_eq!(x[first], 0.0);
                let end = first + frames(TONE_S, FS);
                assert_eq!(x[end], 0.0);
                let small = 0.01 * plan.peak();
                assert!(x[first + 1].abs() < small && x[end - 1].abs() < small);
                let fade = frames(FADE_S, FS);
                let early = x[first..first + fade / 4]
                    .iter()
                    .fold(0.0f32, |m, v| m.max(v.abs()));
                assert!(early < 0.2 * plan.peak(), "{early}");
            }
        }
    }

    #[test]
    fn a_tone_stays_in_its_band() {
        let plan = Plan::new(-30.0, FS, FS);
        let x = plan.render(FS);
        let settle = frames(FILTER_SETTLE_S, FS);
        let window = frames(WINDOW_S, FS);
        for band in [6, 10, 13] {
            let from = frames(tone_start(band) + SETTLE_S, FS);
            let total = rms(&x[from..from + window]).powi(2);
            let own = power(&x, &band_filter(CENTRES_HZ[band], FS), from, window, settle);
            // The filter's own loss across the band, which the reference
            // takes back out.
            let loss = 10.0 * (own / total).log10();
            assert!((-1.7..-1.2).contains(&loss), "band {band}: {loss} dB");
            // An octave away, and two octaves away, which is as close as
            // the order puts the next two tones.
            for (apart, least) in [(3, 12.0), (6, 30.0)] {
                for other in [band - apart, band + apart] {
                    let filter = band_filter(CENTRES_HZ[other], FS);
                    let below = 10.0 * (own / power(&x, &filter, from, window, settle)).log10();
                    assert!(below > least, "band {band} in {other}: {below} dB");
                }
            }
        }
    }

    #[test]
    fn a_band_too_high_for_a_rate_is_left_silent() {
        // A 16 kHz microphone records up to 8 kHz: the 6300 and 8000 Hz bands'
        // filters would not fit under that.
        let plan = Plan::new(-30.0, 48_000.0, 16_000.0);
        let silent: Vec<f64> = CENTRES_HZ
            .iter()
            .zip(plan.fits)
            .filter(|(_, fits)| !fits)
            .map(|(c, _)| *c)
            .collect();
        assert_eq!(silent, [6300.0, 8000.0]);
        let x = plan.render(48_000.0);
        for band in [18, 19] {
            let first = frames(tone_start(band), 48_000.0);
            assert!(x[first..first + 48_000].iter().all(|&v| v == 0.0));
        }
        assert!(Plan::new(-30.0, 44_100.0, 48_000.0).fits.iter().all(|&f| f));
    }

    #[test]
    fn playing_names_the_tone_in_order() {
        let plan = Plan::new(-30.0, FS, FS);
        assert_eq!(plan.playing(0.5), None);
        assert_eq!(plan.playing(1.0), Some((0, 100.0)));
        assert_eq!(plan.playing(2.4), Some((0, 100.0)));
        assert_eq!(plan.playing(2.6), Some((1, 500.0)));
        assert_eq!(plan.playing(30.9), Some((19, 2000.0)));
        assert_eq!(plan.playing(31.0), None);
        assert!((plan.recording_seconds() - 33.5).abs() < 1e-9);
        assert_eq!(plan.recording_frames(), 33 * 48_000 + 24_000);
    }

    #[test]
    fn levels_from_minus_60_to_minus_12_dbfs_are_accepted() {
        for (text, level) in [
            ("-30", -30.0),
            ("-60", -60.0),
            ("-12", -12.0),
            (" -24.5 ", -24.5),
        ] {
            assert_eq!(parse_level(text), Ok(level), "{text}");
        }
        for text in ["-5", "-60.1", "0", "12", "nan", "inf", "", "loud"] {
            assert_eq!(
                parse_level(text).unwrap_err(),
                "expected a level in dBFS from -60 to -12",
                "{text}"
            );
        }
    }

    #[test]
    fn a_flat_path_measures_flat_at_any_pair_of_rates() {
        for (output, input) in [
            (48_000.0, 48_000.0),
            (48_000.0, 44_100.0),
            (44_100.0, 96_000.0),
        ] {
            let plan = Plan::new(-30.0, output, input);
            let c = measure(&plan, &Room::default(), None);
            assert!(worst(&c) < 0.1, "{output} {input}: {}", worst(&c));
            assert!(c.bands.iter().all(|b| b.unusable.is_none()));
            assert_eq!(c.correction.bands, [], "{output} {input}");
            assert_eq!(c.correction.preamp_db, 0.0);
        }
    }

    #[test]
    fn the_latency_is_found_wherever_it_is() {
        let plan = Plan::new(-30.0, FS, FS);
        for delay in [0, 300, 12_000, 110_000] {
            let room = Room {
                delay,
                speaker,
                ..Room::default()
            };
            let c = measure(&plan, &room, None);
            let want = delay as f64 / FS;
            assert!(
                (c.latency_s - want).abs() < 0.002,
                "{delay}: {}",
                c.latency_s
            );
        }
    }

    /// The deviations of `room`'s speaker measured, then measured again
    /// through the correction fitted to them.
    fn corrected(plan: &Plan, room: &Room) -> (Calibration, Calibration) {
        let before = measure(plan, room, None);
        let after = measure(plan, room, Some(&before.correction));
        (before, after)
    }

    #[test]
    fn the_fitted_correction_flattens_a_speaker() {
        let plan = Plan::new(-30.0, FS, FS);
        let room = Room {
            speaker,
            ..Room::default()
        };
        let (before, after) = corrected(&plan, &room);
        assert!(worst(&before) > 3.5, "{}", worst(&before));
        assert!(worst(&after) <= 1.5, "{}", before.table() + &after.table());
        assert!(before.bands.iter().all(|b| b.unusable.is_none()));
        let filters = &before.correction.bands;
        assert!(!filters.is_empty() && filters.len() <= MAX_FILTERS);
        Profile {
            correction: before.correction.clone(),
            ..Profile::default()
        }
        .validate(48_000)
        .unwrap();
    }

    #[test]
    fn rates_and_clocks_that_differ_change_nothing() {
        // A 44.1 kHz microphone whose clock runs 100 ppm fast, beside a
        // 48 kHz output.
        let plan = Plan::new(-30.0, 48_000.0, 44_100.0);
        let room = Room {
            speaker,
            delay: 2_000,
            clock_ppm: 100.0,
            ..Room::default()
        };
        let (before, after) = corrected(&plan, &room);
        assert!(worst(&after) <= 1.5, "{}", before.table() + &after.table());
        let same = measure(
            &Plan::new(-30.0, FS, FS),
            &Room {
                speaker,
                ..Room::default()
            },
            None,
        );
        for (a, b) in before.bands.iter().zip(&same.bands) {
            let (a, b) = (a.deviation_db.unwrap(), b.deviation_db.unwrap());
            assert!((a - b).abs() < 0.15, "{a} {b}");
        }
    }

    #[test]
    fn a_deep_narrow_null_is_not_boosted_past_the_cap() {
        let plan = Plan::new(-30.0, FS, FS);
        let room = Room {
            speaker: |rate| vec![Coefs::peaking(1000.0, 5.0, -30.0, rate)],
            ..Room::default()
        };
        let c = measure(&plan, &room, None);
        let null = band(&c, 1000.0);
        assert!(null.deviation_db.unwrap() < -MAX_BOOST_DB, "{null:?}");
        assert_eq!(null.wanted_db, Some(MAX_BOOST_DB));
        assert!(null.applied_db <= MAX_BOOST_DB);
        let highest = max_curve_db(&c.correction.bands, FS);
        assert!(highest <= MAX_BOOST_DB && highest > 3.0, "{highest}");
        let preamp = -(highest * 10.0).ceil() / 10.0;
        assert_eq!(c.correction.preamp_db, preamp as f32);
    }

    #[test]
    fn boosts_never_pass_the_cap() {
        // Everything is quieter than 300 Hz to 3 kHz: the bands outside want
        // all of the boost, side by side.
        let plan = Plan::new(-30.0, FS, FS);
        let room = Room {
            speaker: |rate| {
                vec![
                    Coefs::high_pass(250.0, 0.707, rate),
                    Coefs::low_pass(3500.0, 0.707, rate),
                ]
            },
            ..Room::default()
        };
        let c = measure(&plan, &room, None);
        let wanted: Vec<f64> = c.bands.iter().filter_map(|b| b.wanted_db).collect();
        assert!(
            wanted.iter().filter(|&&w| w == MAX_BOOST_DB).count() >= 4,
            "{wanted:?}"
        );
        assert!(max_curve_db(&c.correction.bands, FS) <= MAX_BOOST_DB);
        assert!(c.bands.iter().all(|b| b.applied_db <= MAX_BOOST_DB));
        assert!(c.correction.preamp_db <= -5.0);
    }

    #[test]
    fn bands_under_the_room_noise_are_unusable_and_the_rest_still_fit() {
        let plan = Plan::new(-30.0, FS, FS);
        let room = Room {
            speaker,
            rumble: 0.04,
            ..Room::default()
        };
        let (before, after) = corrected(&plan, &room);
        for centre in [100.0, 125.0] {
            let b = band(&before, centre);
            assert_eq!(b.unusable, Some(Unusable::Noise), "{b:?}");
            assert!(b.snr_db.unwrap() < MIN_SNR_DB && b.deviation_db.is_none());
        }
        for b in before.bands.iter().filter(|b| b.centre_hz >= 400.0) {
            assert_eq!(b.unusable, None, "{b:?}");
        }
        assert!(worst(&after) <= 1.5, "{}", before.table() + &after.table());
    }

    #[test]
    fn too_few_usable_bands_is_an_error_naming_the_remedies() {
        let plan = Plan::new(-30.0, FS, FS);
        let room = Room {
            hiss: 0.18,
            ..Room::default()
        };
        let (recording, anchor) = record(&plan, &room, None);
        let Err(Error::TooQuiet { usable, level_db }) = analyse(&plan, &recording, anchor) else {
            panic!("not too quiet");
        };
        assert!((1..MIN_USABLE).contains(&usable), "{usable}");
        assert_eq!(level_db, -30.0);
        let message = Error::TooQuiet { usable, level_db }.to_string();
        for needle in ["system volume", "--level", "-30 dBFS", "8 are needed"] {
            assert!(message.contains(needle), "{needle:?} not in {message}");
        }
    }

    #[test]
    fn an_all_zero_recording_is_the_permission_error() {
        let plan = Plan::new(-30.0, FS, FS);
        let recording = vec![0.0; plan.recording_frames() + 500];
        assert_eq!(analyse(&plan, &recording, 500), Err(Error::Silent));
        let message = Error::Silent.to_string();
        assert!(
            message.contains("System Settings > Privacy & Security > Microphone"),
            "{message}"
        );
        // One sample is enough to tell it from a missing permission.
        let mut one = recording;
        one[1000] = 1e-6;
        assert!(matches!(
            analyse(&plan, &one, 500),
            Err(Error::TooQuiet { usable: 0, .. })
        ));
    }

    #[test]
    fn a_recording_that_ends_early_is_an_error() {
        let plan = Plan::new(-30.0, FS, FS);
        let recording = vec![0.1; plan.recording_frames() + 499];
        assert_eq!(
            analyse(&plan, &recording, 500),
            Err(Error::Short {
                recorded_s: (plan.recording_frames() - 1) as f64 / FS,
                needed_s: 33.5
            })
        );
    }

    fn targets_of(filters: &[Coefs]) -> Vec<(f64, f64)> {
        CENTRES_HZ
            .iter()
            .map(|&f| (f, filters.iter().map(|c| c.magnitude_db(f, FS)).sum()))
            .collect()
    }

    #[test]
    fn the_fit_follows_its_targets_within_its_limits() {
        let targets = targets_of(&[
            Coefs::peaking(150.0, 1.2, -6.0, FS),
            Coefs::peaking(900.0, 3.0, 4.0, FS),
            Coefs::peaking(3000.0, 0.8, -3.0, FS),
            Coefs::peaking(6000.0, 2.0, 5.0, FS),
        ]);
        let bands = fit(&targets, FS);
        assert!(!bands.is_empty() && bands.len() <= MAX_FILTERS);
        for b in &bands {
            assert_eq!(b.kind, BandKind::Peaking);
            assert!(Q_RANGE.contains(&f64::from(b.q)), "{b:?}");
            assert!(f64::from(b.gain_db.abs()) >= MIN_GAIN_DB, "{b:?}");
            assert!((100.0..=8000.0).contains(&b.freq_hz), "{b:?}");
        }
        for &(f, want) in &targets {
            let got = curve_db(&bands, f, FS);
            assert!((got - want).abs() < CLOSE_DB, "{f} Hz: {got} for {want}");
        }
        let freqs: Vec<f32> = bands.iter().map(|b| b.freq_hz).collect();
        assert!(freqs.is_sorted(), "{freqs:?}");
    }

    #[test]
    fn the_fit_stops_at_eight_filters() {
        // Alternating +-3 dB from band to band is beyond eight filters.
        let targets: Vec<(f64, f64)> = CENTRES_HZ
            .iter()
            .enumerate()
            .map(|(i, &f)| (f, if i % 2 == 0 { 3.0 } else { -3.0 }))
            .collect();
        assert_eq!(fit(&targets, FS).len(), MAX_FILTERS);
    }

    #[test]
    fn nothing_to_correct_fits_nothing() {
        let flat: Vec<(f64, f64)> = CENTRES_HZ.iter().map(|&f| (f, 0.3)).collect();
        assert_eq!(fit(&flat, FS), []);
    }

    #[test]
    fn the_table_shows_every_band_and_the_filters() {
        let plan = Plan::new(-30.0, 48_000.0, 16_000.0);
        let room = Room {
            speaker,
            rumble: 0.04,
            ..Room::default()
        };
        let c = measure(&plan, &room, None);
        let table = c.table();
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[0], "   band  deviation  correction  above noise");
        assert_eq!(lines.len(), 1 + BANDS + 2 + c.correction.bands.len());
        assert!(lines[1].starts_with(" 100 Hz          -"), "{}", lines[1]);
        assert!(lines[1].ends_with("unusable: less than 10 dB above the room's noise"));
        assert!(lines[20].starts_with("8000 Hz          -"), "{}", lines[20]);
        assert!(lines[20].ends_with("-  unusable: too high for the devices' sample rates"));
        let usable = lines[11];
        assert!(
            usable.starts_with("1000 Hz") && !usable.contains("unusable"),
            "{usable}"
        );
        assert!(
            lines[22].starts_with("correction: a preamp of -"),
            "{}",
            lines[22]
        );
        assert!(lines[23].contains(" Hz  Q "), "{}", lines[23]);
    }
}
