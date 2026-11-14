//! Audio formats other than WAV, through the system's ffmpeg and ffprobe.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hound::{SampleFormat, WavSpec};

use crate::interrupt;

/// What ffprobe reports about a file: its first audio stream, which dps
/// processes, and the streams it does not.
#[derive(Debug, Default, PartialEq)]
pub struct Probe {
    codec: String,
    sample_fmt: String,
    bits_per_raw_sample: u16,
    /// The stream's, or else the file's when it has no video.
    bit_rate: Option<u64>,
    channel_layout: String,
    /// The indexes of its attached pictures (cover art).
    pictures: Vec<u32>,
    more_audio: usize,
    video: usize,
    /// Subtitle, data and attachment streams.
    other: usize,
}

/// The bodies of the `[name]` sections of ffprobe's default output.
fn sections<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let (open, close) = (format!("[{name}]"), format!("[/{name}]"));
    text.split(&open)
        .skip(1)
        .filter_map(|rest| rest.split_once(&close).map(|(body, _)| body))
        .collect()
}

fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

fn number(body: &str, key: &str) -> Option<u64> {
    field(body, key)?.parse().ok()
}

impl Probe {
    fn parse(text: &str) -> Option<Self> {
        let streams = sections(text, "STREAM");
        let is = |s: &str, kind: &str| field(s, "codec_type") == Some(kind);
        let picture = |s: &str| field(s, "DISPOSITION:attached_pic") == Some("1");
        let audio: Vec<&str> = streams.iter().copied().filter(|s| is(s, "audio")).collect();
        let first = audio.first()?;
        // ffprobe guesses a format from the extension alone, so a file it
        // cannot decode can still come back with a codec but no audio.
        if number(first, "sample_rate")? == 0 || number(first, "channels")? == 0 {
            return None;
        }
        let video = streams
            .iter()
            .filter(|s| is(s, "video") && !picture(s))
            .count();
        let file_rate = sections(text, "FORMAT")
            .first()
            .and_then(|f| number(f, "bit_rate"))
            .filter(|_| video == 0);
        Some(Self {
            codec: field(first, "codec_name")?.to_owned(),
            sample_fmt: field(first, "sample_fmt")?.to_owned(),
            bits_per_raw_sample: number(first, "bits_per_raw_sample")
                .and_then(|b| u16::try_from(b).ok())
                .unwrap_or(0),
            // Ogg and Matroska give no per-stream rate for Opus or Vorbis.
            bit_rate: number(first, "bit_rate").or(file_rate).filter(|&r| r > 0),
            channel_layout: field(first, "channel_layout")
                .unwrap_or_default()
                .to_owned(),
            pictures: streams
                .iter()
                .filter(|s| is(s, "video") && picture(s))
                .filter_map(|s| number(s, "index").and_then(|i| u32::try_from(i).ok()))
                .collect(),
            more_audio: audio.len() - 1,
            video,
            other: streams
                .iter()
                .filter(|s| !is(s, "audio") && !is(s, "video"))
                .count(),
        })
    }

    /// The layout to label the processed WAV with. hound marks its channels
    /// by count alone, so mono comes out as front-left, which the AAC and
    /// Opus encoders reject, and quad as front-left, -right, -centre and LFE.
    fn layout(&self, channels: u16) -> Option<&str> {
        let named = match self.channel_layout.as_str() {
            "" | "unknown" => None,
            layout if layout.ends_with(" channels") => None,
            layout => Some(layout),
        };
        named.or(match channels {
            1 => Some("mono"),
            2 => Some("stereo"),
            _ => None,
        })
    }

    /// The WAV sample format that holds the decoded samples exactly: the
    /// source's integer depth, or 32-bit float for float and lossy sources.
    pub fn sample_format(&self) -> (SampleFormat, u16) {
        match self.sample_fmt.trim_end_matches('p') {
            "u8" => (SampleFormat::Int, 8),
            "s16" => (SampleFormat::Int, 16),
            "s32" if (1..=24).contains(&self.bits_per_raw_sample) => (SampleFormat::Int, 24),
            "s32" => (SampleFormat::Int, 32),
            _ => (SampleFormat::Float, 32),
        }
    }

    fn is_lossless(&self) -> bool {
        self.codec.starts_with("pcm_")
            || matches!(self.codec.as_str(), "flac" | "alac" | "wavpack" | "tta")
    }

    /// What a lossless stream stores: whether its samples are float, and
    /// their bits.
    fn stored(&self) -> Option<(bool, u16)> {
        if !self.is_lossless() {
            return None;
        }
        let raw = self.bits_per_raw_sample;
        let int = |container: u16| {
            (
                false,
                if (1..=container).contains(&raw) {
                    raw
                } else {
                    container
                },
            )
        };
        Some(match self.sample_fmt.trim_end_matches('p') {
            "u8" => int(8),
            "s16" => int(16),
            "s32" => int(32),
            "s64" => int(64),
            "flt" => (true, 32),
            "dbl" => (true, 64),
            _ => return None,
        })
    }

    /// The streams of this input that an output named `output` leaves out,
    /// as a phrase, or `None` if it keeps them all.
    pub fn dropped(&self, output: &Path) -> Option<String> {
        let count = |n: usize, what: &str| match n {
            1 => format!("1 {what}"),
            n => format!("{n} {what}s"),
        };
        let mut parts = Vec::new();
        if self.more_audio > 0 {
            parts.push(count(self.more_audio, "more audio stream"));
        }
        if self.video > 0 {
            parts.push(count(self.video, "video stream"));
        }
        if self.other > 0 {
            parts.push(count(self.other, "subtitle or data stream"));
        }
        if !self.pictures.is_empty() && !carries_pictures(output) {
            let format = extension(output).unwrap_or_else(|| "wav".into());
            parts.push(format!(
                "its cover art ({} that .{format} files cannot hold)",
                count(self.pictures.len(), "picture")
            ));
        }
        let (last, rest) = parts.split_last()?;
        Some(if rest.is_empty() {
            last.clone()
        } else {
            format!("{} and {last}", rest.join(", "))
        })
    }
}

/// How `output`, written from `source` (the file `input`), stores fewer bits
/// or a narrower sample format than `source`, as a phrase, or `None`.
pub fn depth_loss(source: &Probe, output: &Probe, input: &Path) -> Option<String> {
    let ((in_float, in_bits), (out_float, out_bits)) = (source.stored()?, output.stored()?);
    let name =
        |float: bool, bits: u16| format!("{bits}-bit {}", if float { "float" } else { "integer" });
    (out_bits < in_bits || (in_float && !out_float)).then(|| {
        format!(
            "{} samples, less than the {} samples of {}",
            name(out_float, out_bits),
            name(in_float, in_bits),
            input.display()
        )
    })
}

/// Probes `path`; `shown` is the name messages give it.
pub fn probe(path: &Path, shown: &Path) -> Result<Probe, String> {
    let args = [
        "-v".into(),
        "error".into(),
        "-show_entries".into(),
        "stream=index,codec_type,codec_name,sample_fmt,sample_rate,channels,channel_layout,\
         bits_per_raw_sample,bit_rate:stream_disposition=attached_pic:format=bit_rate"
            .into(),
        "-of".into(),
        "default".into(),
        url(path),
    ];
    let cannot_read = format!("cannot read {}", shown.display());
    let stdout = run("ffprobe", &args, shown, &cannot_read, &[(path, shown)])?;
    Probe::parse(&String::from_utf8_lossy(&stdout))
        .ok_or_else(|| format!("{cannot_read}: it has no audio that ffmpeg can decode"))
}

/// Decodes the input's first audio stream to a 32-bit float WAV file. A
/// decoding error, as in a damaged or truncated file, fails the run rather
/// than leaving the output short.
pub fn decode(input: &Path, wav: &Path) -> Result<(), String> {
    let args = decode_args(input, wav);
    let failure = format!("ffmpeg could not decode all of {}", input.display());
    run("ffmpeg", &args, input, &failure, &[]).map(drop)
}

fn decode_args(input: &Path, wav: &Path) -> Vec<OsString> {
    let mut args = ffmpeg_args();
    args.extend(["-xerror".into(), "-i".into(), url(input)]);
    args.extend(["-map", "0:a:0", "-c:a", "pcm_f32le", "-f", "wav", "-y"].map(OsString::from));
    args.push(url(wav));
    args
}

/// Encodes `wav`, the processed audio of `input` in the format `spec`, into
/// `dest` in the format named by the extension of `output` (the name `dest`
/// will be renamed to), with the tags, chapters and cover art of `input`
/// where that format holds them.
pub fn encode(
    wav: &Path,
    spec: WavSpec,
    input: &Path,
    probe: &Probe,
    dest: &Path,
    output: &Path,
) -> Result<(), String> {
    let encoders = if same_extension(input, output) {
        Encoders::list(output)?
    } else {
        Encoders::default()
    };
    let options = output_options(input, output, probe, spec, &encoders)?;
    let args = encode_args(wav, spec, input, probe, dest, output, options);
    let failure = format!("ffmpeg failed to write {}", output.display());
    run(
        "ffmpeg",
        &args,
        output,
        &failure,
        &[(dest, output), (input, input)],
    )
    .map(drop)
}

fn encode_args(
    wav: &Path,
    spec: WavSpec,
    input: &Path,
    probe: &Probe,
    dest: &Path,
    output: &Path,
    options: Vec<String>,
) -> Vec<OsString> {
    let mut args = ffmpeg_args();
    args.extend(["-i".into(), url(wav), "-i".into(), url(input)]);
    args.extend(["-map", "0:a", "-map_metadata", "1"].map(OsString::from));
    // Naming a stream's metadata source stops ffmpeg copying any stream's by
    // default, so each mapped stream gets its own.
    args.extend(["-map_metadata:s:a:0", "1:s:a:0"].map(OsString::from));
    if carries_pictures(output) && !probe.pictures.is_empty() {
        for (k, index) in probe.pictures.iter().enumerate() {
            args.extend([
                "-map".into(),
                format!("1:{index}").into(),
                format!("-map_metadata:s:v:{k}").into(),
                format!("1:s:{index}").into(),
            ]);
        }
        args.extend(["-c:v", "copy", "-disposition:v", "attached_pic"].map(OsString::from));
        if matches!(extension(output).as_deref(), Some("aif" | "aiff" | "aifc")) {
            // AIFF holds pictures only in an ID3v2 tag.
            args.extend(["-write_id3v2", "1"].map(OsString::from));
        }
    }
    if let Some(layout) = probe.layout(spec.channels) {
        args.extend([
            "-af".into(),
            format!("channelmap=channel_layout={layout}").into(),
        ]);
    }
    args.extend(options.into_iter().map(OsString::from));
    args.extend(["-y".into(), url(dest)]);
    args
}

fn extension(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_lowercase())
}

fn same_extension(input: &Path, output: &Path) -> bool {
    extension(output).is_some() && extension(output) == extension(input)
}

/// Whether ffmpeg writes cover art into a file named `output`. It also
/// accepts a picture for .mka and .mov but stores it as a video track or
/// not at all.
fn carries_pictures(output: &Path) -> bool {
    matches!(
        extension(output).as_deref(),
        Some("flac" | "mp3" | "m4a" | "m4b" | "mp4" | "aif" | "aiff" | "aifc")
    )
}

/// ffmpeg options for writing `output` from a processed WAV of `spec`.
///
/// ffmpeg writes PCM containers as 16-bit unless told otherwise, so those
/// keep the processed file's sample format. An output with the input's
/// extension keeps its codec, which ffmpeg might otherwise swap for the
/// container's default (ALAC in .m4a would become AAC, Opus in .mka AC-3),
/// and a lossy codec's bit rate, which ffmpeg would otherwise reset to its
/// default.
fn output_options(
    input: &Path,
    output: &Path,
    probe: &Probe,
    spec: WavSpec,
    encoders: &Encoders,
) -> Result<Vec<String>, String> {
    let pcm = |big_endian| vec!["-c:a".into(), pcm(spec, big_endian)];
    Ok(match extension(output).as_deref() {
        None => [pcm(false), vec!["-f".into(), "wav".into()]].concat(),
        Some("wav" | "w64") => pcm(false),
        Some("aif" | "aiff" | "aifc" | "caf" | "au") => pcm(true),
        Some(_) if same_extension(input, output) => {
            let encoder = encoders.for_codec(&probe.codec).ok_or_else(|| {
                format!(
                    "cannot write {}: this ffmpeg has no encoder for {}, the codec of {}; \
                     name an output with another extension",
                    output.display(),
                    probe.codec,
                    input.display()
                )
            })?;
            let mut options = vec!["-c:a".into(), encoder.name.clone()];
            if encoder.experimental {
                options.extend(["-strict".into(), "experimental".into()]);
            }
            if let (false, Some(rate)) = (probe.is_lossless(), probe.bit_rate) {
                options.extend(["-b:a".into(), rate.to_string()]);
            }
            options
        }
        Some(_) => Vec::new(),
    })
}

fn pcm(spec: WavSpec, big_endian: bool) -> String {
    let (name, endian) = match (spec.sample_format, spec.bits_per_sample) {
        // 8-bit WAV is unsigned and 8-bit AIFF signed, and neither has an
        // endianness.
        (SampleFormat::Int, 8) => (if big_endian { "pcm_s8" } else { "pcm_u8" }, ""),
        (SampleFormat::Int, 16) => ("pcm_s16", if big_endian { "be" } else { "le" }),
        (SampleFormat::Int, 24) => ("pcm_s24", if big_endian { "be" } else { "le" }),
        (SampleFormat::Int, _) => ("pcm_s32", if big_endian { "be" } else { "le" }),
        (SampleFormat::Float, _) => ("pcm_f32", if big_endian { "be" } else { "le" }),
    };
    format!("{name}{endian}")
}

/// The audio encoders this ffmpeg has.
#[derive(Debug, Default)]
struct Encoders(Vec<Encoder>);

#[derive(Debug, PartialEq)]
struct Encoder {
    name: String,
    codec: String,
    experimental: bool,
}

impl Encoders {
    fn list(subject: &Path) -> Result<Self, String> {
        let args = ["-hide_banner", "-encoders"].map(OsString::from);
        let text = run(
            "ffmpeg",
            &args,
            subject,
            "cannot list ffmpeg's encoders",
            &[],
        )?;
        Ok(Self::parse(&String::from_utf8_lossy(&text)))
    }

    /// Lines such as ` A..X.D opus   Opus` or
    /// ` A....D libopus   libopus Opus (codec opus)` after the legend.
    fn parse(text: &str) -> Self {
        let lines = text
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("---"));
        Self(
            lines
                .filter_map(|line| {
                    let mut words = line.split_whitespace();
                    let flags = words.next().filter(|f| f.starts_with('A'))?;
                    let name = words.next()?.to_owned();
                    let codec = line
                        .trim_end()
                        .strip_suffix(')')
                        .and_then(|l| l.rsplit_once("(codec "))
                        .map_or_else(|| name.clone(), |(_, codec)| codec.to_owned());
                    Some(Encoder {
                        name,
                        codec,
                        experimental: flags.chars().nth(3) == Some('X'),
                    })
                })
                .collect(),
        )
    }

    /// The encoder `-c:a codec` would pick unless it is experimental and a
    /// stable one exists: the native opus and vorbis encoders are both, and
    /// libopus and libvorbis the usual ones.
    fn for_codec(&self, codec: &str) -> Option<&Encoder> {
        let of_codec = || self.0.iter().filter(|e| e.codec == codec);
        let stable = |e: &&Encoder| !e.experimental;
        let named = |e: &&Encoder| e.name == codec;
        of_codec()
            .filter(stable)
            .find(named)
            .or_else(|| of_codec().find(stable))
            .or_else(|| of_codec().find(named))
            .or_else(|| of_codec().next())
    }
}

/// A private directory for intermediate files, removed with its contents on
/// drop.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!("dps-{}-{nanos}", std::process::id()));
        fs::create_dir(&dir)
            .map_err(|e| format!("cannot create temporary directory {}: {e}", dir.display()))?;
        Ok(Self(dir))
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// With stdin closed as well, ffmpeg can neither take keys from the
/// terminal nor change its settings.
fn ffmpeg_args() -> Vec<OsString> {
    ["-nostdin", "-v", "error"].map(OsString::from).into()
}

/// ffmpeg reads `name:` at the start of a path as a protocol, and a path
/// starting with `-` as an option; the explicit file protocol avoids both.
fn url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path);
    url
}

/// Runs ffmpeg or ffprobe and returns its stdout. `subject` is the file the
/// run is for, `failure` begins the message if it fails, and each `(path,
/// shown)` in `renames` names a path in that message as the user knows it.
/// A signal to dps ends the run, and the program with it.
fn run(
    program: &str,
    args: &[OsString],
    subject: &Path,
    failure: &str,
    renames: &[(&Path, &Path)],
) -> Result<Vec<u8>, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => format!(
                "{} needs ffmpeg, which is not installed (no {program} on PATH); \
                 install it with `brew install ffmpeg`",
                subject.display()
            ),
            _ => format!("cannot run {program}: {e}"),
        })?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let status = loop {
        if let Err(stopped) = interrupt::check() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(stopped);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("cannot run {program}: {e}")),
        }
    };
    // A Ctrl-C at the terminal reaches ffmpeg too, which then fails.
    interrupt::check()?;
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let mut message = first_error(&String::from_utf8_lossy(&stderr));
    for &(path, shown) in renames.iter().chain([&(subject, subject)]) {
        let shown = shown.display().to_string();
        message = message.replace(&*url(path).to_string_lossy(), &shown);
        message = message.replace(&path.display().to_string(), &shown);
    }
    // ffprobe starts with the file name, which `failure` already gives.
    let subject = format!("{}: ", subject.display());
    let message = message.strip_prefix(&subject).unwrap_or(&message);
    Err(format!("{failure}: {message}"))
}

/// The first line ffmpeg logged, which names the cause, without the
/// `[component @ 0x...]` tags. The lines after it report the knock-on
/// failures, ending in a generic one such as "Error opening output files:
/// Invalid argument".
fn first_error(stderr: &str) -> String {
    let Some(line) = stderr.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return "it exited with an error and no message".into();
    };
    let mut line = line;
    while let Some(rest) = line.strip_prefix('[')
        && let Some((tag, rest)) = rest.split_once("] ")
        && tag.contains(" @ ")
    {
        line = rest;
    }
    line.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(codec: &str, sample_fmt: &str, bits: u16, bit_rate: Option<u64>) -> Probe {
        Probe {
            codec: codec.into(),
            sample_fmt: sample_fmt.into(),
            bits_per_raw_sample: bits,
            bit_rate,
            ..Probe::default()
        }
    }

    fn with_layout(layout: &str) -> Probe {
        Probe {
            channel_layout: layout.into(),
            ..probe("flac", "s16", 16, None)
        }
    }

    fn spec(sample_format: SampleFormat, bits_per_sample: u16) -> WavSpec {
        WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample,
            sample_format,
        }
    }

    fn stream(lines: &str) -> String {
        format!("[STREAM]\n{lines}[/STREAM]\n")
    }

    #[test]
    fn probe_output_is_parsed_and_undecodable_streams_rejected() {
        let flac = stream(
            "index=0\ncodec_name=flac\ncodec_type=audio\nsample_fmt=s16\nsample_rate=44100\n\
             channels=2\nchannel_layout=stereo\nbits_per_raw_sample=16\nbit_rate=N/A\n\
             DISPOSITION:attached_pic=0\n",
        );
        assert_eq!(
            Probe::parse(&flac),
            Some(Probe {
                channel_layout: "stereo".into(),
                ..probe("flac", "s16", 16, None)
            })
        );
        let mp3 = stream(
            "index=0\ncodec_name=mp3\ncodec_type=audio\nsample_fmt=fltp\nsample_rate=48000\n\
             channels=1\nbit_rate=320000\nbits_per_raw_sample=N/A\n",
        );
        assert_eq!(
            Probe::parse(&mp3),
            Some(probe("mp3", "fltp", 0, Some(320_000)))
        );
        // What ffprobe reports for a text file named .flac.
        let garbage = stream(
            "index=0\ncodec_name=flac\ncodec_type=audio\nsample_fmt=unknown\nsample_rate=0\n\
             channels=0\nchannel_layout=unknown\nbit_rate=N/A\nbits_per_raw_sample=N/A\n",
        );
        assert_eq!(Probe::parse(&garbage), None);
        // No audio stream at all.
        assert_eq!(Probe::parse(""), None);
        let picture =
            stream("index=0\ncodec_name=png\ncodec_type=video\nDISPOSITION:attached_pic=1\n");
        assert_eq!(Probe::parse(&picture), None);
    }

    #[test]
    fn probe_counts_the_streams_dps_does_not_process() {
        let opus = "index=1\ncodec_name=opus\ncodec_type=audio\nsample_fmt=fltp\n\
                    sample_rate=48000\nchannels=2\nbit_rate=N/A\n";
        let text = [
            stream("index=0\ncodec_name=mjpeg\ncodec_type=video\nDISPOSITION:attached_pic=1\n"),
            stream(opus),
            stream("index=2\ncodec_name=aac\ncodec_type=audio\nsample_rate=44100\nchannels=1\n"),
            stream("index=3\ncodec_name=png\ncodec_type=video\nDISPOSITION:attached_pic=1\n"),
            stream("index=4\ncodec_name=subrip\ncodec_type=subtitle\n"),
            "[FORMAT]\nbit_rate=140000\n[/FORMAT]\n".into(),
        ]
        .concat();
        let p = Probe::parse(&text).unwrap();
        assert_eq!(p.codec, "opus");
        // The file's rate stands in for the stream's.
        assert_eq!(p.bit_rate, Some(140_000));
        assert_eq!(p.pictures, [0, 3]);
        assert_eq!((p.more_audio, p.video, p.other), (1, 0, 1));
        assert_eq!(
            p.dropped(Path::new("out.flac")).unwrap(),
            "1 more audio stream and 1 subtitle or data stream"
        );
        assert_eq!(
            p.dropped(Path::new("out.wav")).unwrap(),
            "1 more audio stream, 1 subtitle or data stream and its cover art (2 pictures \
             that .wav files cannot hold)"
        );

        // With video in the file, its rate is not the audio's.
        let with_video = [
            stream(opus),
            stream("index=2\ncodec_name=h264\ncodec_type=video\nDISPOSITION:attached_pic=0\n"),
            "[FORMAT]\nbit_rate=5000000\n[/FORMAT]\n".into(),
        ]
        .concat();
        let p = Probe::parse(&with_video).unwrap();
        assert_eq!(p.bit_rate, None);
        assert_eq!(p.dropped(Path::new("o")).unwrap(), "1 video stream");
        assert_eq!(
            probe("flac", "s16", 16, None).dropped(Path::new("o.wav")),
            None
        );
    }

    #[test]
    fn decoded_samples_keep_the_sources_integer_depth() {
        let cases = [
            ("u8", 0, SampleFormat::Int, 8),
            ("s16", 16, SampleFormat::Int, 16),
            ("s16p", 0, SampleFormat::Int, 16),
            ("s32", 24, SampleFormat::Int, 24),
            ("s32p", 20, SampleFormat::Int, 24),
            ("s32", 32, SampleFormat::Int, 32),
            ("s32", 0, SampleFormat::Int, 32),
            ("fltp", 0, SampleFormat::Float, 32),
            ("dbl", 0, SampleFormat::Float, 32),
            ("s64", 0, SampleFormat::Float, 32),
        ];
        for (fmt, bits, format, depth) in cases {
            assert_eq!(
                probe("x", fmt, bits, None).sample_format(),
                (format, depth),
                "{fmt} {bits}"
            );
        }
    }

    #[test]
    fn a_lower_depth_or_float_to_integer_is_a_loss() {
        let loss = |a: &Probe, b: &Probe| depth_loss(a, b, Path::new("in.x"));
        let flac32 = probe("flac", "s32", 32, None);
        let flac24 = probe("flac", "s32", 24, None);
        let f64 = probe("pcm_f64le", "dbl", 0, None);
        let f32 = probe("pcm_f32be", "flt", 0, None);
        let s16 = probe("pcm_s16le", "s16", 0, None);
        assert_eq!(
            loss(&flac32, &flac24).unwrap(),
            "24-bit integer samples, less than the 32-bit integer samples of in.x"
        );
        assert_eq!(
            loss(&f64, &f32).unwrap(),
            "32-bit float samples, less than the 64-bit float samples of in.x"
        );
        assert!(loss(&f32, &flac24).is_some());
        assert!(loss(&f32, &probe("pcm_s32le", "s32", 32, None)).is_some());
        assert_eq!(loss(&flac24, &flac24), None);
        assert_eq!(loss(&s16, &flac24), None);
        assert_eq!(loss(&flac24, &f32), None);
        // Lossy codecs have no depth to lose.
        assert_eq!(loss(&probe("mp3", "fltp", 0, None), &s16), None);
        assert_eq!(loss(&flac24, &probe("aac", "fltp", 0, None)), None);
    }

    const ENCODERS: &str = "\
Encoders:
 V..... = Video
 A..... = Audio
 ------
 V....D a64multi             Multicolor charset for Commodore 64 (codec a64_multi)
 A....D aac                  AAC (Advanced Audio Coding)
 A..... aac_at               aac (AudioToolbox) (codec aac)
 A....D alac                 ALAC (Apple Lossless Audio Codec)
 A....D flac                 FLAC (Free Lossless Audio Codec)
 A....D libmp3lame           libmp3lame MP3 (MPEG audio layer 3) (codec mp3)
 A..X.D opus                 Opus
 A....D libopus              libopus Opus (codec opus)
 A....D pcm_s24le            PCM signed 24-bit little-endian
 A..X.D vorbis               Vorbis
";

    #[test]
    fn encoders_are_chosen_like_ffmpeg_but_stable_first() {
        let encoders = Encoders::parse(ENCODERS);
        assert_eq!(encoders.0.len(), 9);
        let pick = |codec| {
            encoders
                .for_codec(codec)
                .map(|e| (e.name.as_str(), e.experimental))
        };
        assert_eq!(pick("aac"), Some(("aac", false)));
        assert_eq!(pick("mp3"), Some(("libmp3lame", false)));
        assert_eq!(pick("opus"), Some(("libopus", false)));
        assert_eq!(pick("vorbis"), Some(("vorbis", true)));
        assert_eq!(pick("alac"), Some(("alac", false)));
        assert_eq!(pick("a64_multi"), None, "not an audio encoder");
        assert_eq!(pick("wmapro"), None);
    }

    #[test]
    fn output_options_keep_depth_codec_or_bit_rate() {
        let encoders = Encoders::parse(ENCODERS);
        let opts = |input: &str, output: &str, p: &Probe, s: WavSpec| {
            output_options(Path::new(input), Path::new(output), p, s, &encoders)
                .map(|o| o.join(" "))
        };
        let s16 = spec(SampleFormat::Int, 16);
        let s24 = spec(SampleFormat::Int, 24);
        let f32 = spec(SampleFormat::Float, 32);
        let alac = probe("alac", "s32p", 24, Some(1_400_000));
        let aac = probe("aac", "fltp", 0, Some(256_000));
        let mp3 = probe("mp3", "fltp", 0, None);
        let opus = probe("opus", "fltp", 0, Some(140_000));
        let vorbis = probe("vorbis", "fltp", 0, Some(160_000));
        let flac = probe("flac", "s16", 16, None);
        let wav = probe("pcm_s16le", "s16", 0, Some(1_411_200));

        assert_eq!(
            opts("a.flac", "b.wav", &flac, s16).unwrap(),
            "-c:a pcm_s16le"
        );
        assert_eq!(opts("a.mp3", "b.WAV", &mp3, f32).unwrap(), "-c:a pcm_f32le");
        assert_eq!(
            opts("a.flac", "b", &flac, s16).unwrap(),
            "-c:a pcm_s16le -f wav"
        );
        assert_eq!(
            opts("a.flac", "b.aiff", &flac, s24).unwrap(),
            "-c:a pcm_s24be"
        );
        assert_eq!(
            opts("a.wav", "b.aif", &wav, spec(SampleFormat::Int, 8)).unwrap(),
            "-c:a pcm_s8"
        );
        // Every PCM container keeps the depth or float format.
        assert_eq!(opts("a.wav", "b.caf", &wav, s24).unwrap(), "-c:a pcm_s24be");
        assert_eq!(opts("a.wav", "b.au", &wav, f32).unwrap(), "-c:a pcm_f32be");
        assert_eq!(opts("a.wav", "b.w64", &wav, f32).unwrap(), "-c:a pcm_f32le");
        assert_eq!(opts("a.m4a", "a.dps.m4a", &alac, s16).unwrap(), "-c:a alac");
        assert_eq!(
            opts("a.m4a", "a.dps.M4A", &aac, f32).unwrap(),
            "-c:a aac -b:a 256000"
        );
        assert_eq!(
            opts("a.mp3", "a.dps.mp3", &mp3, f32).unwrap(),
            "-c:a libmp3lame"
        );
        // Containers that hold several codecs keep the input's.
        assert_eq!(
            opts("a.mka", "a.dps.mka", &opus, f32).unwrap(),
            "-c:a libopus -b:a 140000"
        );
        assert_eq!(
            opts("a.ogg", "a.dps.ogg", &vorbis, f32).unwrap(),
            "-c:a vorbis -strict experimental -b:a 160000"
        );
        assert_eq!(opts("a.mka", "a.dps.mka", &flac, s16).unwrap(), "-c:a flac");
        let e = opts("a.wma", "a.dps.wma", &probe("wmapro", "fltp", 0, None), f32).unwrap_err();
        assert_eq!(
            e,
            "cannot write a.dps.wma: this ffmpeg has no encoder for wmapro, the codec of \
             a.wma; name an output with another extension"
        );
        // A new format gets ffmpeg's defaults for it.
        assert_eq!(opts("a.m4a", "a.flac", &alac, s16).unwrap(), "");
        assert_eq!(opts("a.wav", "a.mp3", &wav, s16).unwrap(), "");
    }

    #[test]
    fn ffmpeg_never_reads_stdin_and_takes_paths_only_as_files() {
        let strings = |args: Vec<OsString>| {
            args.into_iter()
                .map(|a| a.into_string().unwrap())
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            strings(decode_args(Path::new("-in:%d.flac"), Path::new("/t/d.wav"))),
            "-nostdin -v error -xerror -i file:-in:%d.flac -map 0:a:0 -c:a pcm_f32le -f wav -y \
             file:/t/d.wav"
        );
        let cover = Probe {
            pictures: vec![2],
            channel_layout: "stereo".into(),
            ..probe("flac", "s16", 16, None)
        };
        let s16 = spec(SampleFormat::Int, 16);
        let args = |output: &str| {
            strings(encode_args(
                Path::new("/t/p.wav"),
                s16,
                Path::new("in.flac"),
                &cover,
                Path::new(".o.tmp.x"),
                Path::new(output),
                vec!["-c:a".into(), "flac".into()],
            ))
        };
        assert_eq!(
            args("o.flac"),
            "-nostdin -v error -i file:/t/p.wav -i file:in.flac -map 0:a -map_metadata 1 \
             -map_metadata:s:a:0 1:s:a:0 -map 1:2 -map_metadata:s:v:0 1:s:2 -c:v copy \
             -disposition:v attached_pic -af channelmap=channel_layout=stereo -c:a flac -y \
             file:.o.tmp.x"
        );
        assert!(args("o.aiff").contains(" -write_id3v2 1 "));
        assert!(!args("o.wav").contains("-map 1:"));
    }

    #[test]
    fn layout_is_the_sources_or_else_mono_or_stereo() {
        assert_eq!(with_layout("5.1(side)").layout(6), Some("5.1(side)"));
        assert_eq!(with_layout("quad").layout(4), Some("quad"));
        assert_eq!(with_layout("mono").layout(1), Some("mono"));
        for unnamed in ["", "unknown", "6 channels"] {
            assert_eq!(with_layout(unnamed).layout(6), None, "{unnamed:?}");
            assert_eq!(with_layout(unnamed).layout(1), Some("mono"), "{unnamed:?}");
            assert_eq!(
                with_layout(unnamed).layout(2),
                Some("stereo"),
                "{unnamed:?}"
            );
        }
    }

    #[test]
    fn first_error_drops_component_tags() {
        let unknown_extension = "\
[AVFormatContext @ 0x7c52c28280] Unable to choose an output format for 'out.xyz'; use a standard extension for the filename or specify the format manually.
[out#0 @ 0x7c52c78180] Error initializing the muxer for out.xyz: Invalid argument
Error opening output file out.xyz.
Error opening output files: Invalid argument
";
        assert_eq!(
            first_error(unknown_extension),
            "Unable to choose an output format for 'out.xyz'; use a standard extension \
             for the filename or specify the format manually."
        );
        assert_eq!(
            first_error("[aost#0:0/pcm @ 0x1] [enc:pcm @ 0x2] Could not open encoder\n"),
            "Could not open encoder"
        );
        assert_eq!(first_error("\n\nplain message\n"), "plain message");
        assert_eq!(
            first_error("[not a tag] kept\n"),
            "[not a tag] kept",
            "only `[x @ 0x...]` tags are dropped"
        );
        assert!(first_error("").contains("no message"));
    }
}
