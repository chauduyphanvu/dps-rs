use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use dps::{Processor, Profile};
use hound::WavSpec;

// live.rs reloads its watched file through `super::load_profile`.
#[cfg(target_os = "macos")]
use config::load_profile;
use config::{Dirs, Source};
use presets::{PRESETS, Preset};

mod config;
mod doctor;
mod ffmpeg;
mod import;
mod interrupt;
#[cfg(target_os = "macos")]
mod live;
mod presets;
#[cfg(target_os = "macos")]
mod service;
mod wav;

const BLOCK_FRAMES: usize = 4096;

#[cfg(target_os = "macos")]
const EXAMPLES: &str = "\
Examples:
  dps                       Process system audio live with your settings file
  dps live --preset music   The same with the built-in music preset
  dps start                 The same in the background (dps status, dps stop)
  dps doctor                Check that live mode can work here
  dps process song.flac     Write song.dps.flac (formats other than WAV use ffmpeg)
  dps import hd650.txt      Import an AutoEQ headphone correction (ParametricEQ.txt)
  dps calibrate             Measure your speakers with the microphone and correct them
  dps presets               List the built-in presets
  dps profile               Print the default profile as TOML

Settings file: $XDG_CONFIG_HOME/dps/profile.toml, else ~/.config/dps/profile.toml,
created with the default profile on first use; --profile or --preset overrides it.";

#[cfg(not(target_os = "macos"))]
const EXAMPLES: &str = "\
Examples:
  dps process song.flac     Write song.dps.flac (formats other than WAV use ffmpeg)
  dps process song.wav --preset music
  dps import hd650.txt      Import an AutoEQ headphone correction (ParametricEQ.txt)
  dps presets               List the built-in presets
  dps profile               Print the default profile as TOML
  dps doctor                Check the settings and ffmpeg

Settings file: $XDG_CONFIG_HOME/dps/profile.toml, else ~/.config/dps/profile.toml,
created with the default profile on first use; --profile or --preset overrides it.";

/// Digital Power Station processing for system audio and audio files.
#[derive(Parser)]
#[command(version, after_help = EXAMPLES)]
#[cfg_attr(target_os = "macos", command(args_conflicts_with_subcommands = true))]
struct Cli {
    /// Bare `dps` runs live with these.
    #[cfg(target_os = "macos")]
    #[command(flatten)]
    source: SourceArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args)]
struct SourceArgs {
    /// Profile to use instead of the settings file; keys it leaves out take
    /// their default values.
    #[arg(short, long, value_name = "FILE.toml")]
    profile: Option<PathBuf>,
    /// Built-in preset to use instead of the settings file.
    #[arg(long, value_name = "NAME", value_parser = presets::parser(), conflicts_with = "profile")]
    preset: Option<&'static Preset>,
}

#[derive(Subcommand)]
enum Command {
    /// Process system audio live until Ctrl-C (what bare `dps` does).
    ///
    /// Processes all system audio in real time and plays it on the default
    /// output device. Needs macOS 14.2 or later.
    #[cfg(target_os = "macos")]
    Live {
        #[command(flatten)]
        source: SourceArgs,
        /// Run as the background service `dps start` sets up.
        #[arg(long, hide = true)]
        service: bool,
    },
    /// Process system audio live in the background.
    ///
    /// Keeps running after the terminal closes, reloading the settings file
    /// or --profile file whenever it is saved, until `dps stop`. macOS asks
    /// whether dps itself may record system audio, again after each rebuild
    /// or reinstall. Messages go to ~/Library/Logs/dps.log.
    #[cfg(target_os = "macos")]
    Start {
        #[command(flatten)]
        source: SourceArgs,
        /// Also start it at every login.
        #[arg(long)]
        login: bool,
    },
    /// Stop processing in the background.
    #[cfg(target_os = "macos")]
    Stop {
        /// Also stop starting it at login.
        #[arg(long)]
        login: bool,
    },
    /// Say whether dps is processing in the background, and how.
    #[cfg(target_os = "macos")]
    Status,
    /// Check what dps needs here and say how to fix what is missing.
    Doctor {
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Process an audio file: WAV directly, other formats through ffmpeg.
    ///
    /// WAV to WAV keeps the sample rate, channels, sample format, channel
    /// layout and the input's tags and other chunks, except peak data that
    /// no longer fits the audio. Other formats are decoded with ffmpeg (a
    /// decoding error stops the run) and encoded into the format the output's
    /// extension names: PCM formats keep the sample format, and the input's
    /// own extension keeps its codec and bit rate. Tags, chapters and cover
    /// art are kept where the format holds them. Only the first audio stream
    /// is processed; a warning names whatever the output leaves out or stores
    /// with fewer bits.
    Process {
        /// The audio file to process.
        input: PathBuf,
        /// [default: INPUT with .dps before its extension]
        output: Option<PathBuf>,
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Import a headphone or speaker correction (AutoEQ / Equalizer APO parametric EQ text).
    ///
    /// The AutoEQ project publishes a measured correction for each headphone
    /// model it covers, in its results, as a "ParametricEQ.txt" file. This
    /// fills the [correction] table of the settings file (or of --profile)
    /// with that file's preamp and filters and turns it on, so the EQ and
    /// every other value stay as they are. Importing again replaces the
    /// earlier correction. The profile file is rewritten, so comments in it
    /// are not kept; a running dps picks the change up on its own.
    Import {
        /// The ParametricEQ.txt file to import.
        input: PathBuf,
        /// Profile to write the correction into instead of the settings file.
        #[arg(short, long, value_name = "FILE.toml")]
        profile: Option<PathBuf>,
    },
    /// Measure the speakers with the microphone and write a correction for them.
    ///
    /// Plays a quiet warble tone for each third of an octave from 100 Hz to
    /// 8 kHz on the default output device, records it with the default
    /// input device (the Mac's microphone), and fits up to 8 peaking filters
    /// that flatten what was heard, with at most 6 dB of boost and 10 dB of
    /// cut. They go into the [correction] table of the settings file (or of
    /// --profile), turned on, as `dps import` does. It takes about 35 s: put
    /// the microphone where you listen and keep the room quiet; Ctrl-C stops
    /// it and writes nothing. --level is the only loudness control: dps
    /// leaves the system volume alone. Quit dps live, or stop the background
    /// run with `dps stop`, first. The result corrects the speakers as this
    /// microphone hears them at this position.
    #[cfg(target_os = "macos")]
    Calibrate {
        /// Profile to write the correction into instead of the settings file.
        #[arg(short, long, value_name = "FILE.toml")]
        profile: Option<PathBuf>,
        /// RMS level of the test tones in dBFS, from -60 to -12; no sample
        /// goes above its peak, 3 dB higher.
        #[arg(
            long,
            value_name = "DB",
            default_value_t = dps::calibrate::DEFAULT_LEVEL_DB,
            value_parser = dps::calibrate::parse_level,
            allow_negative_numbers = true
        )]
        level: f64,
        /// Measure and print the result, but write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// List the built-in presets.
    Presets,
    /// Print the default profile as TOML.
    Profile {
        /// Print this preset instead.
        #[arg(long, value_name = "NAME", value_parser = presets::parser())]
        preset: Option<&'static Preset>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let dirs = Dirs::from_env();
    let result = match cli.command {
        #[cfg(target_os = "macos")]
        None => run_live(cli.source, false, &dirs),
        #[cfg(target_os = "macos")]
        Some(Command::Live { source, service }) => run_live(source, service, &dirs),
        #[cfg(target_os = "macos")]
        Some(Command::Start { source, login }) => source
            .resolve(&dirs)
            .and_then(|source| service::start(source, login, &dirs)),
        #[cfg(target_os = "macos")]
        Some(Command::Stop { login }) => service::stop(login, &dirs),
        #[cfg(target_os = "macos")]
        Some(Command::Status) => service::status(&dirs),
        #[cfg(not(target_os = "macos"))]
        None => print_help(),
        Some(Command::Process {
            input,
            output,
            source,
        }) => source
            .resolve(&dirs)
            .and_then(|source| process(&input, output, &source)),
        Some(Command::Import { input, profile }) => import::run(&input, profile, &dirs),
        #[cfg(target_os = "macos")]
        Some(Command::Calibrate {
            profile,
            level,
            dry_run,
        }) => live::calibrate(profile, level, dry_run, &dirs),
        Some(Command::Presets) => print_presets(),
        Some(Command::Profile { preset }) => print_profile(preset),
        Some(Command::Doctor { source }) => return doctor::run(source.resolve(&dirs), &dirs),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

impl SourceArgs {
    fn resolve(self, dirs: &Dirs) -> Result<Source, String> {
        Source::new(self.profile, self.preset, dirs)
    }
}

fn process(input: &Path, output: Option<PathBuf>, source: &Source) -> Result<(), String> {
    interrupt::install()?;
    let output = match output {
        Some(output) => output,
        None => default_output(input)?,
    };
    if let (Ok(a), Ok(b)) = (fs::canonicalize(input), fs::canonicalize(&output))
        && a == b
    {
        return Err(format!(
            "output {} is the input file; choose a different output path",
            output.display()
        ));
    }
    // Opening it here tells a missing file from one that is not a WAV.
    File::open(input).map_err(|e| format!("cannot open {}: {e}", input.display()))?;

    let mut warnings = Vec::new();
    let (spec, clipped, profile) = match wav::Reader::open(input) {
        Ok(reader) if writes_wav(&output) => {
            let profile = source.load()?;
            let spec = reader.spec();
            validate(&profile, spec, source)?;
            let chunks = wav::Chunks::read(input)?;
            let (pending, file) = PendingFile::beside(&output)?;
            let writer = wav::Writer::with_chunks(file, &output, spec, chunks)?;
            let clipped = run_chain(reader, writer, &profile, input)?;
            interrupt::check()?;
            pending.persist(&output)?;
            (spec, clipped, profile)
        }
        // Through ffmpeg: decoded unless the WAV reader handles the input, then
        // processed into a WAV file in `scratch` that ffmpeg encodes into
        // `pending` with the input's tags.
        wav_input => {
            let probe = ffmpeg::probe(input, input)?;
            let profile = source.load()?;
            let (pending, _) = PendingFile::beside(&output)?;
            let scratch = ffmpeg::Scratch::new()?;
            let (reader, spec) = match wav_input {
                Ok(reader) => {
                    let spec = reader.spec();
                    (reader, spec)
                }
                Err(_) => {
                    let decoded = scratch.join("decoded.wav");
                    ffmpeg::decode(input, &decoded)?;
                    let reader = wav::Reader::open(&decoded)?;
                    let (sample_format, bits_per_sample) = probe.sample_format();
                    let spec = WavSpec {
                        sample_format,
                        bits_per_sample,
                        ..reader.spec()
                    };
                    (reader, spec)
                }
            };
            validate(&profile, spec, source)?;
            let processed = scratch.join("processed.wav");
            let file = File::create(&processed)
                .map_err(|e| format!("cannot create {}: {e}", processed.display()))?;
            let writer = wav::Writer::new(file, &processed, spec)?.count_float_overs();
            let clipped = run_chain(reader, writer, &profile, input)?;
            ffmpeg::encode(&processed, spec, input, &probe, &pending.path, &output)?;
            let written = ffmpeg::probe(&pending.path, &output)?;
            interrupt::check()?;
            pending.persist(&output)?;
            if let Some(dropped) = probe.dropped(&output) {
                warnings.push(format!(
                    "{} leaves out {dropped} of {}",
                    output.display(),
                    input.display()
                ));
            }
            if let Some(loss) = ffmpeg::depth_loss(&probe, &written, input) {
                warnings.push(format!("{} has {loss}", output.display()));
            }
            (spec, clipped, profile)
        }
    };

    let rate = spec.sample_rate;
    if let Some(bands) = profile.describe_skipped_eq_bands(rate) {
        warnings.insert(
            0,
            format!(
                "the EQ skipped {bands}: at the {rate} Hz sample rate of {}, a band's centre \
                 and bandwidth (freqs_hz / q) must be below {} Hz",
                input.display(),
                f64::from(rate) / 2.0
            ),
        );
    }
    let channels = spec.channels;
    if profile.stereo.enabled && channels != 2 {
        warnings.push(format!(
            "stereo.enabled is set but {} has {channels} channel(s); the stereo block was skipped",
            input.display()
        ));
    }
    if clipped > 0 {
        warnings.push(format!(
            "{clipped} samples clipped in {}; lower output_gain_db in the profile",
            output.display()
        ));
    }
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// `<stem>.dps.<extension>` beside the input, or `<name>.dps.wav` for an
/// input without an extension. An input already named `<stem>.dps.<ext>`
/// gets none, so that running dps over a folder twice does not process its
/// own output again into `.dps.dps.` files.
fn default_output(input: &Path) -> Result<PathBuf, String> {
    let name = input
        .file_name()
        .ok_or_else(|| format!("input {} is not a file path", input.display()))?;
    if input
        .file_stem()
        .map(Path::new)
        .and_then(Path::extension)
        .is_some_and(|e| e.eq_ignore_ascii_case("dps"))
        && input.extension().is_some()
    {
        return Err(format!(
            "{} looks like dps output already; to process it again, name the output",
            input.display()
        ));
    }
    let mut out = OsString::new();
    match (input.file_stem(), input.extension()) {
        (Some(stem), Some(extension)) => {
            out.push(stem);
            out.push(".dps.");
            out.push(extension);
        }
        _ => {
            out.push(name);
            out.push(".dps.wav");
        }
    }
    Ok(input.with_file_name(out))
}

/// An output without an extension is written as WAV, as `process` always did.
fn writes_wav(output: &Path) -> bool {
    output
        .extension()
        .is_none_or(|e| e.eq_ignore_ascii_case("wav"))
}

fn validate(profile: &Profile, spec: WavSpec, source: &Source) -> Result<(), String> {
    profile
        .validate(spec.sample_rate)
        .map_err(|e| format!("{}: {e}", source.name()))
}

/// Runs all of `reader` through the chain into `writer` and returns how many
/// samples were clipped. The output lines up with the input and is as long:
/// the chain's latency is cut from its start, and as much silence after the
/// input pushes the rest out.
fn run_chain(
    mut reader: wav::Reader,
    mut writer: wav::Writer,
    profile: &Profile,
    input: &Path,
) -> Result<u64, String> {
    let spec = reader.spec();
    let channels = usize::from(spec.channels);
    let mut processor = Processor::new(profile, spec.sample_rate, channels);
    let latency = processor.latency_frames() * channels;
    let mut to_cut = latency;
    let mut emit = |samples: &mut [f32]| {
        processor.process(samples);
        if samples.iter().any(|s| !s.is_finite()) {
            return Err(format!(
                "the output became non-finite (NaN or infinity) while processing {}; check the profile",
                input.display()
            ));
        }
        let cut = to_cut.min(samples.len());
        to_cut -= cut;
        writer.write(&samples[cut..])
    };
    let mut block = vec![0.0; BLOCK_FRAMES * channels];
    let mut samples_before = 0;
    loop {
        interrupt::check()?;
        let n = reader.read(&mut block)?;
        if n == 0 {
            break;
        }
        if let Some(i) = block[..n].iter().position(|s| !s.is_finite()) {
            let frame = (samples_before + i) / channels;
            return Err(format!(
                "{} has a NaN or infinite sample at {:.3} s, which cannot be processed; \
                 the input file is damaged",
                input.display(),
                frame as f64 / f64::from(spec.sample_rate)
            ));
        }
        samples_before += n;
        emit(&mut block[..n])?;
    }
    let mut tail = latency;
    while tail > 0 {
        interrupt::check()?;
        let n = tail.min(block.len());
        block[..n].fill(0.0);
        emit(&mut block[..n])?;
        tail -= n;
    }
    writer.finish()
}

/// A file created beside its destination and renamed over it only by
/// `persist`, so a failed run leaves no partial output and keeps any file
/// already at the destination. Dropped without `persist`, it is removed.
struct PendingFile {
    path: PathBuf,
    persisted: bool,
}

impl PendingFile {
    fn beside(dest: &Path) -> Result<(Self, File), String> {
        let stem = dest
            .file_stem()
            .ok_or_else(|| format!("output {} is not a file path", dest.display()))?;
        // The extension stays last because ffmpeg picks the format from it.
        let mut temp_name = OsString::from(".");
        temp_name.push(stem);
        temp_name.push(format!(".{}.tmp", std::process::id()));
        if let Some(extension) = dest.extension() {
            temp_name.push(".");
            temp_name.push(extension);
        }
        let path = dest.with_file_name(temp_name);
        let file = File::create_new(&path)
            .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
        let pending = Self {
            path,
            persisted: false,
        };
        Ok((pending, file))
    }

    fn persist(mut self, dest: &Path) -> Result<(), String> {
        fs::rename(&self.path, dest)
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(target_os = "macos")]
fn run_live(args: SourceArgs, background: bool, dirs: &Dirs) -> Result<(), String> {
    let source = args.resolve(dirs)?;
    if background {
        return service::serve(source, dirs, |source, report| {
            let options = live::Options {
                background: Some(report),
                ..live_options(source, dirs)?
            };
            live::run(options)
        });
    }
    // Two runs would each process the other's output.
    let _lock = service::lock_foreground(dirs)?;
    live::run(live_options(source, dirs)?)
}

/// A profile file, or else the settings file, is watched and saved to. A
/// preset is not watched, and edits to it are saved to the settings file.
#[cfg(target_os = "macos")]
fn live_options(source: Source, dirs: &Dirs) -> Result<live::Options, String> {
    let profile = source.load()?;
    let name = source.name();
    Ok(match source {
        Source::File(path) | Source::Settings(path) => live::Options {
            profile,
            name,
            watch: Some(path.clone()),
            save_path: path,
            background: None,
        },
        Source::Preset(_) => live::Options {
            profile,
            name,
            watch: None,
            save_path: dirs.settings_file()?,
            background: None,
        },
    })
}

#[cfg(not(target_os = "macos"))]
fn print_help() -> Result<(), String> {
    use clap::CommandFactory;
    Cli::command()
        .print_help()
        .map_err(|e| format!("cannot write to stdout: {e}"))
}

fn print_presets() -> Result<(), String> {
    let width = PRESETS.iter().map(|p| p.name.len()).max().unwrap_or(0);
    let mut text = String::new();
    for preset in &PRESETS {
        text += &format!("{:width$}  {}\n", preset.name, preset.description);
    }
    text += "\nStarting points reasoned from what each stage of the chain does, not \
             Bongiovi's\ntunings (the patents publish none). `dps profile --preset NAME` \
             prints one.\nEach levels its input first: measured, once settled, a mix at \
             -20 to -4 LUFS\ncomes out within 1 LU of a -14 LUFS one, at -30 LUFS within 2 LU. \
             A limiter\nkeeps every peak at or below -1 dBFS (night: -2 dBFS), even at \
             abrupt starts.\n";
    write_stdout(&text)
}

fn print_profile(preset: Option<&Preset>) -> Result<(), String> {
    match preset {
        Some(preset) => write_stdout(preset.toml),
        None => write_stdout(&config::default_toml()),
    }
}

fn write_stdout(text: &str) -> Result<(), String> {
    io::stdout()
        .write_all(text.as_bytes())
        .map_err(|e| format!("cannot write to stdout: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn default_output_puts_dps_before_the_extension() {
        let out = |input: &str| default_output(Path::new(input)).unwrap();
        assert_eq!(out("song.flac"), Path::new("song.dps.flac"));
        assert_eq!(out("dir/take.2.WAV"), Path::new("dir/take.2.dps.WAV"));
        assert_eq!(out("/a/b/noext"), Path::new("/a/b/noext.dps.wav"));
        assert_eq!(out(".hidden"), Path::new(".hidden.dps.wav"));
        assert!(default_output(Path::new("dir/..")).is_err());
        // Output of an earlier run is not processed into x.dps.dps.flac.
        for again in ["x.dps.flac", "dir/x.DPS.wav"] {
            let e = default_output(Path::new(again)).unwrap_err();
            assert!(e.contains("looks like dps output already"), "{e}");
        }
        assert_eq!(out("x.dps"), Path::new("x.dps.dps"));
        assert_eq!(out("dps.flac"), Path::new("dps.dps.flac"));
    }

    #[test]
    fn a_signal_stops_the_chain_at_the_next_block() {
        let dir = config::tests::TestDir::new("interrupt-chain");
        let (input, output) = (dir.0.join("in.wav"), dir.0.join("out.wav"));
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&input, spec).unwrap();
        for _ in 0..BLOCK_FRAMES * 3 {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
        let run = || {
            let reader = wav::Reader::open(&input).unwrap();
            let file = File::create(&output).unwrap();
            let writer = wav::Writer::new(file, &output, spec).unwrap();
            run_chain(reader, writer, &Profile::default(), &input)
        };
        assert_eq!(run(), Ok(0));
        interrupt::raise(15);
        let result = run();
        interrupt::raise(0);
        assert_eq!(
            result.unwrap_err(),
            "stopped by SIGTERM; nothing was written and the temporary files were removed"
        );
    }

    /// `samples` written as a float WAV file of `channels` at 48 kHz, run
    /// through `run_chain` with `profile` and read back.
    fn chained(
        dir: &config::tests::TestDir,
        samples: &[f32],
        channels: u16,
        profile: &Profile,
    ) -> Vec<f32> {
        let (input, output) = (dir.0.join("in.wav"), dir.0.join("out.wav"));
        let spec = WavSpec {
            channels,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&input, spec).unwrap();
        for &s in samples {
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
        let reader = wav::Reader::open(&input).unwrap();
        let writer = wav::Writer::new(File::create(&output).unwrap(), &output, spec).unwrap();
        assert_eq!(run_chain(reader, writer, profile, &input), Ok(0));
        hound::WavReader::open(&output)
            .unwrap()
            .samples::<f32>()
            .map(Result::unwrap)
            .collect()
    }

    /// Deterministic values in [-amp, amp).
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    #[test]
    fn the_output_lines_up_with_the_input_and_is_as_long() {
        // Every stage at unity but the limiter, with its lookahead and true
        // peak, on audio it never turns down: all it does is delay, and that
        // is taken back out, at every length from under the latency to past
        // a block.
        let dir = config::tests::TestDir::new("chain-latency");
        let unity = dps::profile::CompressorParams {
            ratio: 1.0,
            ..Default::default()
        };
        let profile = Profile {
            output_gain_db: 0.0,
            low_shelf_db: 0.0,
            high_shelf_db: 0.0,
            compressor1: unity,
            compressor2: unity,
            ..Profile::default()
        };
        assert!(profile.limiter.enabled && profile.limiter.lookahead_ms > 0.0);
        let latency = Processor::new(&profile, 48_000, 2).latency_frames();
        assert!(latency > 10);
        for frames in [
            0,
            10,
            latency,
            BLOCK_FRAMES - latency / 2,
            2 * BLOCK_FRAMES + 17,
        ] {
            let input = noise(2 * frames, 0.3, frames as u64);
            assert_eq!(chained(&dir, &input, 2, &profile), input, "{frames} frames");
        }

        // A file shorter than the latency that the limiter turns down comes
        // out whole, every sample turned down but there, under the ceiling.
        let ceiling = 10f64.powf(f64::from(profile.limiter.ceiling_db) / 20.0) as f32;
        let loud = noise(2 * 10, 4.0, 1);
        let out = chained(&dir, &loud, 2, &profile);
        assert_eq!(out.len(), loud.len());
        for (o, i) in out.iter().zip(&loud) {
            assert!(o.abs() <= ceiling && o.abs() < i.abs(), "{o} from {i}");
            assert_eq!(o.signum(), i.signum(), "{o} from {i}");
        }

        // Through the default chain, which also turns it down, the length
        // stays the input's.
        for frames in [1, 100, BLOCK_FRAMES + 1] {
            let input = noise(2 * frames, 2.0, 7);
            let out = chained(&dir, &input, 2, &Profile::default());
            assert_eq!(out.len(), input.len(), "{frames} frames");
        }
    }

    #[test]
    fn pending_file_keeps_the_extension_last() {
        let dir = config::tests::TestDir::new("pending-name");
        let pid = std::process::id();
        for (dest, temp) in [
            ("out.flac", format!(".out.{pid}.tmp.flac")),
            ("out", format!(".out.{pid}.tmp")),
        ] {
            let (pending, _) = PendingFile::beside(&dir.0.join(dest)).unwrap();
            assert_eq!(pending.path, dir.0.join(&temp));
            drop(pending);
            assert!(!dir.0.join(&temp).exists());
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod live_tests {
    use super::*;
    use config::tests::TestDir;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("dps").chain(args.iter().copied()))
    }

    #[test]
    fn bare_dps_runs_live_and_takes_the_source_flags() {
        let cli = parse(&[]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.source.profile.is_none() && cli.source.preset.is_none());

        let cli = parse(&["--preset", "voice"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.source.preset.unwrap().name, "voice");
        let cli = parse(&["-p", "mine.toml"]).unwrap();
        assert_eq!(cli.source.profile.unwrap(), Path::new("mine.toml"));

        let Some(Command::Live { source, service }) =
            parse(&["live", "--preset", "music"]).unwrap().command
        else {
            panic!("not live");
        };
        assert_eq!(source.preset.unwrap().name, "music");
        assert!(!service);

        for args in [
            &["--preset", "voice", "--profile", "a.toml"][..],
            &["live", "-p", "a.toml", "--preset", "voice"],
            &["--preset", "nope"],
            // Flags before a subcommand belong to bare `dps` only.
            &["--preset", "voice", "process", "a.wav"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn live_watches_and_saves_the_profile_file_or_settings_file_but_only_saves_a_preset() {
        let dir = TestDir::new("live-options");
        let dirs = dir.dirs();
        let settings = dirs.settings_file().unwrap();
        let mine = dir.0.join("mine.toml");
        fs::write(&mine, "output_gain_db = -2.0\n").unwrap();

        let options = |profile: Option<&Path>, preset: Option<&str>| {
            let preset = preset.map(|name| presets::find(name).unwrap());
            let source = Source::new(profile.map(Path::to_owned), preset, &dirs).unwrap();
            live_options(source, &dirs).unwrap()
        };

        let o = options(Some(&mine), None);
        assert_eq!(o.watch.as_deref(), Some(mine.as_path()));
        assert_eq!(o.save_path, mine);
        assert_eq!(o.name, mine.display().to_string());
        assert_eq!(o.profile.output_gain_db, -2.0);
        assert!(!settings.exists());

        let o = options(None, Some("music"));
        assert_eq!(o.watch, None);
        assert_eq!(o.save_path, settings);
        assert_eq!(o.name, "preset music");
        assert_eq!(o.profile, presets::find("music").unwrap().profile());
        assert!(!settings.exists(), "a preset run does not create it");

        let o = options(None, None);
        assert_eq!(o.watch.as_deref(), Some(settings.as_path()));
        assert_eq!(o.save_path, settings);
        assert_eq!(o.name, settings.display().to_string());
        assert_eq!(o.profile, Profile::default());
        assert_eq!(
            fs::read_to_string(&settings).unwrap(),
            config::default_toml()
        );
    }
}
