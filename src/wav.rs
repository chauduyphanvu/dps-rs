//! Streaming WAV input and output as f32 samples, nominally in [-1, 1).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use hound::{Sample, SampleFormat, WavReader, WavSpec, WavWriter};

/// Reads the samples a block at a time; hound only reads the header, since
/// its reader takes one sample per call.
pub struct Reader {
    file: BufReader<File>,
    spec: WavSpec,
    frames: u64,
    /// Samples not yet read, over all channels.
    left: u64,
    /// How the stored samples decode, or why they cannot be, in which case
    /// the first read fails as hound's would.
    decode: Result<Decode, Undecodable>,
    width: usize,
    raw: Vec<u8>,
    path: PathBuf,
}

/// The sample layouts hound decodes: integers stored in 1 to 4 bytes (8-bit
/// unsigned, 24-bit also in 4 bytes) and 32-bit floats.
#[derive(Clone, Copy)]
enum Decode {
    U8,
    I16,
    I24,
    I24In4,
    I32,
    F32,
}

#[derive(Clone, Copy)]
enum Undecodable {
    TooWide,
    Unsupported,
}

impl Decode {
    /// What hound's `Sample::read` makes of samples `width` bytes wide.
    fn new(spec: WavSpec, width: usize) -> Result<Self, Undecodable> {
        match (spec.sample_format, width, spec.bits_per_sample) {
            (SampleFormat::Int, 1, 8) => Ok(Self::U8),
            (SampleFormat::Int, 2, 16) => Ok(Self::I16),
            (SampleFormat::Int, 3, 24) => Ok(Self::I24),
            (SampleFormat::Int, 4, 24) => Ok(Self::I24In4),
            (SampleFormat::Int, 4, 32) => Ok(Self::I32),
            (SampleFormat::Float, 4, 32) => Ok(Self::F32),
            (_, width, _) if width > 4 => Err(Undecodable::TooWide),
            _ => Err(Undecodable::Unsupported),
        }
    }

    /// Converts `raw`, `out.len()` samples, as hound and then `full_scale`
    /// would, one by one.
    fn convert(self, raw: &[u8], out: &mut [f32], bits: u16) {
        let scale = full_scale(bits) as f32;
        let int = |s: i32| s as f32 / scale;
        match self {
            Self::U8 => {
                for (x, b) in out.iter_mut().zip(raw) {
                    *x = int(i32::from(*b) - 128);
                }
            }
            Self::I16 => {
                for (x, b) in out.iter_mut().zip(raw.chunks_exact(2)) {
                    *x = int(i32::from(i16::from_le_bytes([b[0], b[1]])));
                }
            }
            Self::I24 => {
                for (x, b) in out.iter_mut().zip(raw.chunks_exact(3)) {
                    // Into the top three bytes, then shifted back down to
                    // extend the sign.
                    *x = int(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8);
                }
            }
            Self::I24In4 => {
                for (x, b) in out.iter_mut().zip(raw.chunks_exact(4)) {
                    // The fourth byte is padding, whatever it holds.
                    *x = int(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8);
                }
            }
            Self::I32 => {
                for (x, b) in out.iter_mut().zip(raw.chunks_exact(4)) {
                    *x = int(i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                }
            }
            Self::F32 => {
                for (x, b) in out.iter_mut().zip(raw.chunks_exact(4)) {
                    *x = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
            }
        }
    }
}

impl Reader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let inner = WavReader::new(BufReader::new(file)).map_err(|e| match e {
            hound::Error::Unsupported => format!(
                "{}: unsupported WAV encoding (only integer PCM and 32-bit float are supported)",
                path.display()
            ),
            hound::Error::FormatError(reason) => {
                format!("{}: not a valid WAV file ({reason})", path.display())
            }
            other => format!("{}: not a valid WAV file ({other})", path.display()),
        })?;
        let spec = inner.spec();
        match (spec.sample_format, spec.bits_per_sample) {
            (SampleFormat::Int, 8 | 16 | 24 | 32) | (SampleFormat::Float, 32) => {}
            (format, bits) => {
                let kind = match format {
                    SampleFormat::Int => "integer",
                    SampleFormat::Float => "float",
                };
                return Err(format!(
                    "{}: unsupported sample format {bits}-bit {kind} \
                     (supported: 8/16/24/32-bit integer, 32-bit float)",
                    path.display()
                ));
            }
        }
        let (frames, left) = (u64::from(inner.duration()), u64::from(inner.len()));
        let mut file = inner.into_inner();
        let cannot_read = |e: io::Error| format!("cannot read {}: {e}", path.display());
        // hound stops just past the data chunk's header, which ends with the
        // length it found to be `left` whole samples.
        let mut data_bytes = [0; 4];
        file.seek_relative(-4)
            .and_then(|()| file.read_exact(&mut data_bytes))
            .map_err(cannot_read)?;
        let width = u64::from(u32::from_le_bytes(data_bytes))
            .checked_div(left)
            .unwrap_or(0) as usize;
        Ok(Self {
            file,
            spec,
            frames,
            left,
            decode: Decode::new(spec, width),
            width,
            raw: Vec::new(),
            path: path.to_owned(),
        })
    }

    pub fn spec(&self) -> WavSpec {
        self.spec
    }

    /// The length in frames that the header states.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Fills `out` from the start with the next interleaved samples and
    /// returns how many it read; 0 means the end of the file.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize, String> {
        let n = (out.len() as u64).min(self.left) as usize;
        if n == 0 {
            return Ok(0);
        }
        let cannot_read = |e: hound::Error| format!("cannot read {}: {e}", self.path.display());
        let decode = self.decode.map_err(|why| {
            cannot_read(match why {
                Undecodable::TooWide => hound::Error::TooWide,
                Undecodable::Unsupported => hound::Error::Unsupported,
            })
        })?;
        self.raw.resize(n * self.width, 0);
        read_all(&mut self.file, &mut self.raw).map_err(|e| cannot_read(e.into()))?;
        decode.convert(&self.raw, &mut out[..n], self.spec.bits_per_sample);
        self.left -= n as u64;
        Ok(n)
    }
}

/// Fills `buf` as hound does, with its error for a file that ends first.
fn read_all(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<()> {
    let mut n = 0;
    while n < buf.len() {
        match reader.read(&mut buf[n..])? {
            0 => return Err(io::Error::other("Failed to read enough bytes.")),
            read => n += read,
        }
    }
    Ok(())
}

/// A WAV file's chunks other than its samples: the fmt chunk with its
/// channel mask, and the rest, such as LIST/INFO tags and bext.
pub struct Chunks {
    /// The RIFF header and every chunk before the data chunk.
    before: Vec<u8>,
    after: Vec<u8>,
    /// The width of one sample in the data, from the fmt chunk.
    sample_bytes: u16,
}

/// Chunks that describe the samples themselves, which processing changes.
const STALE: [&[u8; 4]; 3] = [b"PEAK", b"levl", b"md5 "];

impl Chunks {
    /// The chunks of `path`, a file `Reader::open` accepted.
    pub fn read(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        Self::parse(BufReader::new(file))
            .map_err(|e| format!("cannot read {}: {e}", path.display()))
    }

    /// The header hound writes for `spec`.
    fn for_spec(spec: WavSpec) -> Self {
        let mut header = Vec::new();
        WavWriter::new(Cursor::new(&mut header), spec)
            .and_then(WavWriter::finalize)
            .expect("hound writes a header for every spec Reader accepts");
        Self::parse(Cursor::new(header)).expect("hound's own header parses")
    }

    fn parse(mut reader: impl Read + Seek) -> io::Result<Self> {
        let mut chunks = Self {
            before: vec![0; 12],
            after: Vec::new(),
            sample_bytes: 0,
        };
        reader.read_exact(&mut chunks.before)?;
        let mut past_data = false;
        loop {
            let mut header = [0; 8];
            match reader.read_exact(&mut header) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let id = &header[..4];
            let len = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
            let padded = u64::from(len) + u64::from(len % 2);
            if id == b"data" && !past_data {
                past_data = true;
                reader.seek(SeekFrom::Current(padded as i64))?;
                continue;
            }
            let mut chunk = header.to_vec();
            let read = reader.by_ref().take(padded).read_to_end(&mut chunk)?;
            if read as u64 != padded {
                // A last chunk without its pad byte is common; one cut short
                // is not a chunk.
                if read as u64 + 1 != padded {
                    if past_data {
                        break;
                    }
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                chunk.push(0);
            }
            if id == b"fmt " && len >= 14 {
                let field = |at: usize| u16::from_le_bytes([chunk[8 + at], chunk[9 + at]]);
                let (channels, block_align) = (field(2), field(12));
                chunks.sample_bytes = block_align.checked_div(channels).unwrap_or(0);
            }
            if STALE.iter().any(|s| s[..] == *id) {
                continue;
            }
            if past_data {
                chunks.after.extend(chunk);
            } else {
                chunks.before.extend(chunk);
            }
        }
        Ok(chunks)
    }
}

pub struct Writer {
    out: BufWriter<File>,
    path: PathBuf,
    spec: WavSpec,
    sample_bytes: u16,
    /// Where the data chunk's size field is.
    data_size_at: u64,
    data_bytes: u64,
    after: Vec<u8>,
    clipped: u64,
    count_float_overs: bool,
}

impl Writer {
    /// Writes a WAV file of `spec` to `file`; `path` is the name used in
    /// error messages.
    pub fn new(file: File, path: &Path, spec: WavSpec) -> Result<Self, String> {
        Self::with_chunks(file, path, spec, Chunks::for_spec(spec))
    }

    /// Writes `chunks`, those of an input of the same `spec`, around the
    /// samples, so its channel mask and tags are kept.
    pub fn with_chunks(
        file: File,
        path: &Path,
        spec: WavSpec,
        chunks: Chunks,
    ) -> Result<Self, String> {
        // The widths hound writes; any other container gets hound's header.
        let supported = match spec.sample_format {
            SampleFormat::Int => matches!(
                (spec.bits_per_sample, chunks.sample_bytes),
                (8, 1) | (16, 2) | (24, 3 | 4) | (32, 4)
            ),
            SampleFormat::Float => chunks.sample_bytes == 4,
        };
        let chunks = if supported {
            chunks
        } else {
            Chunks::for_spec(spec)
        };
        let mut out = BufWriter::new(file);
        out.write_all(&chunks.before)
            .and_then(|()| out.write_all(b"data\0\0\0\0"))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(Self {
            out,
            path: path.to_owned(),
            spec,
            sample_bytes: chunks.sample_bytes,
            data_size_at: chunks.before.len() as u64 + 4,
            data_bytes: 0,
            after: chunks.after,
            clipped: 0,
            count_float_overs: false,
        })
    }

    /// Also counts float samples beyond full scale as clipped, for a file
    /// that is only an intermediate on the way to an integer or lossy format.
    /// They are still written unclamped.
    pub fn count_float_overs(mut self) -> Self {
        self.count_float_overs = true;
        self
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let spec = self.spec;
        let (out, bytes) = (&mut self.out, self.sample_bytes);
        let result = match spec.sample_format {
            SampleFormat::Float => {
                if self.count_float_overs {
                    self.clipped += samples.iter().filter(|s| s.abs() > 1.0).count() as u64;
                }
                samples
                    .iter()
                    .try_for_each(|&s| s.write_padded(out, 32, bytes))
            }
            SampleFormat::Int => {
                let scale = full_scale(spec.bits_per_sample);
                let clipped = &mut self.clipped;
                samples.iter().try_for_each(|&s| {
                    let x = (f64::from(s) * scale).round();
                    // Two's complement has no +1.0, and f32 cannot hold every
                    // 32-bit integer (i32::MAX reads back as exactly 1.0), so a
                    // value at +full scale is clamped one step without counting
                    // as clipping.
                    if x > scale || x < -scale {
                        *clipped += 1;
                    }
                    let x = x.clamp(-scale, scale - 1.0) as i32;
                    x.write_padded(out, spec.bits_per_sample, bytes)
                })
            }
        };
        result.map_err(|e| format!("cannot write {}: {e}", self.path.display()))?;
        self.data_bytes += samples.len() as u64 * u64::from(bytes);
        let riff_bytes = self.data_size_at + self.data_bytes + 1 + self.after.len() as u64;
        if riff_bytes > u64::from(u32::MAX) {
            return Err(format!(
                "cannot write {}: it would pass the 4 GiB limit of a WAV file",
                self.path.display()
            ));
        }
        Ok(())
    }

    /// Completes the file and returns how many samples were clipped.
    pub fn finish(self) -> Result<u64, String> {
        let cannot = |e: io::Error| format!("cannot write {}: {e}", self.path.display());
        let mut out = self.out;
        let pad = self.data_bytes % 2;
        if pad == 1 {
            out.write_all(&[0]).map_err(cannot)?;
        }
        out.write_all(&self.after).map_err(cannot)?;
        let mut file = out.into_inner().map_err(|e| cannot(e.into_error()))?;
        let end = self.data_size_at + 4 + self.data_bytes + pad + self.after.len() as u64;
        let mut patch = |at: u64, value: u64| {
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&(value as u32).to_le_bytes())
        };
        patch(4, end - 8)
            .and_then(|()| patch(self.data_size_at, self.data_bytes))
            .map_err(cannot)?;
        Ok(self.clipped)
    }
}

/// 2^(bits-1), the magnitude of the most negative integer sample.
fn full_scale(bits: u16) -> f64 {
    f64::from(1u32 << (bits - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_spec(bits: u16) -> WavSpec {
        WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        }
    }

    fn temp_path(test: &str, bits: u16) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tonewell-wav-{}-{test}-{bits}.wav",
            std::process::id()
        ))
    }

    fn write_ints(test: &str, bits: u16, samples: &[f32]) -> (u64, Vec<i32>) {
        let path = temp_path(test, bits);
        let mut writer = Writer::new(File::create(&path).unwrap(), &path, int_spec(bits)).unwrap();
        writer.write(samples).unwrap();
        let clipped = writer.finish().unwrap();
        let stored = WavReader::open(&path)
            .unwrap()
            .samples::<i32>()
            .map(Result::unwrap)
            .collect();
        std::fs::remove_file(&path).unwrap();
        (clipped, stored)
    }

    #[test]
    fn written_samples_round_to_the_nearest_step_of_two_to_bits_minus_one() {
        for bits in [8u16, 16, 24] {
            let full = f64::from(1u32 << (bits - 1));
            // Quarter steps are exact in f32 at every depth here.
            let k = (full / 3.0).floor();
            let input: Vec<f32> = [
                k + 0.25,
                k + 0.75,
                -(k + 0.25),
                -(k + 0.75),
                0.5 * full,
                full - 1.0,
                -full,
                0.25,
                0.75,
            ]
            .iter()
            .map(|steps| (steps / full) as f32)
            .collect();
            let k = k as i32;
            let full = full as i32;
            let want = [k, k + 1, -k, -(k + 1), full / 2, full - 1, -full, 0, 1];
            let (clipped, stored) = write_ints("round", bits, &input);
            assert_eq!(stored, want, "{bits}-bit");
            assert_eq!(clipped, 0, "{bits}-bit");
        }
    }

    #[test]
    fn read_samples_are_scaled_by_two_to_bits_minus_one() {
        for bits in [8u16, 16, 24, 32] {
            let full = 1i64 << (bits - 1);
            let ints = [-full, full / 2, 1, -1, full - 1];
            let path = temp_path("read", bits);
            let mut writer = WavWriter::create(&path, int_spec(bits)).unwrap();
            for &s in &ints {
                writer.write_sample(s as i32).unwrap();
            }
            writer.finalize().unwrap();
            let mut reader = Reader::open(&path).unwrap();
            let mut got = [0.0f32; 5];
            assert_eq!(reader.read(&mut got).unwrap(), 5);
            std::fs::remove_file(&path).unwrap();
            // The exact ratio, rounded once to f32 (only 32-bit needs it).
            let want = ints.map(|s| (s as f64 / full as f64) as f32);
            assert_eq!(got, want, "{bits}-bit");
            assert_eq!(got[0], -1.0);
            assert_eq!(got[1], 0.5);
        }
    }

    #[test]
    fn out_of_range_samples_are_clamped_and_counted() {
        let (clipped, stored) = write_ints("clamp", 16, &[3.0, -3.0, 1.0, -1.0, 0.5, -0.5]);
        assert_eq!(stored, [32_767, -32_768, 32_767, -32_768, 16_384, -16_384]);
        assert_eq!(clipped, 2);

        let (clipped, stored) = write_ints("clamp", 32, &[1.0, -1.0, 1.5]);
        assert_eq!(stored, [i32::MAX, i32::MIN, i32::MAX]);
        assert_eq!(clipped, 1);
    }

    #[test]
    fn float_overs_are_kept_and_counted_only_on_request() {
        let spec = WavSpec {
            sample_format: SampleFormat::Float,
            ..int_spec(32)
        };
        let samples = [1.0, -1.0, 1.5, -2.0, 0.25];
        for (count, want) in [(false, 0), (true, 2)] {
            let path = temp_path(&format!("float-overs-{count}"), 32);
            let mut writer = Writer::new(File::create(&path).unwrap(), &path, spec).unwrap();
            if count {
                writer = writer.count_float_overs();
            }
            writer.write(&samples).unwrap();
            assert_eq!(writer.finish().unwrap(), want);
            let stored: Vec<f32> = WavReader::open(&path)
                .unwrap()
                .samples::<f32>()
                .map(Result::unwrap)
                .collect();
            std::fs::remove_file(&path).unwrap();
            assert_eq!(stored, samples);
        }
    }

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    /// A RIFF file of `chunks`, with its size field right.
    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend((body.len() as u32 + 4).to_le_bytes());
        out.extend(b"WAVE");
        out.extend(body);
        out
    }

    /// WAVE_FORMAT_EXTENSIBLE for 24-bit integer samples in `container`
    /// bytes with this channel mask.
    fn extensible_fmt(channels: u16, container: u16, mask: u32) -> Vec<u8> {
        let block = channels * container;
        let mut fmt = Vec::new();
        fmt.extend(0xfffeu16.to_le_bytes());
        fmt.extend(channels.to_le_bytes());
        fmt.extend(48_000u32.to_le_bytes());
        fmt.extend((48_000 * u32::from(block)).to_le_bytes());
        fmt.extend(block.to_le_bytes());
        fmt.extend((container * 8).to_le_bytes());
        fmt.extend(22u16.to_le_bytes());
        fmt.extend(24u16.to_le_bytes());
        fmt.extend(mask.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM.
        fmt.extend([
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
        chunk(b"fmt ", &fmt)
    }

    /// Copies `input` through `Writer::with_chunks` and returns the output.
    fn copy(test: &str, input: &[u8]) -> Vec<u8> {
        let (from, to) = (temp_path(test, 0), temp_path(&format!("{test}-out"), 0));
        std::fs::write(&from, input).unwrap();
        let mut reader = Reader::open(&from).unwrap();
        let spec = reader.spec();
        let chunks = Chunks::read(&from).unwrap();
        let mut writer =
            Writer::with_chunks(File::create(&to).unwrap(), &to, spec, chunks).unwrap();
        let mut block = vec![0.0; 64];
        loop {
            let n = reader.read(&mut block).unwrap();
            if n == 0 {
                break;
            }
            writer.write(&block[..n]).unwrap();
        }
        writer.finish().unwrap();
        let output = std::fs::read(&to).unwrap();
        std::fs::remove_file(&from).unwrap();
        std::fs::remove_file(&to).unwrap();
        output
    }

    #[test]
    fn a_copy_keeps_the_fmt_chunk_and_every_tag_chunk_in_place() {
        let samples: Vec<u8> = (0..4 * 3 * 5).map(|i| (i * 7) as u8).collect();
        let list = chunk(b"LIST", b"INFOINAM\x05\0\0\0Song\0\0");
        // hound mis-skips odd-sized chunks before the data, so this is even.
        let bext = chunk(b"bext", &[b'x'; 602]);
        let quad = extensible_fmt(4, 3, 0x33);
        for (name, input) in [
            (
                "tags-before",
                riff(&[
                    quad.clone(),
                    bext.clone(),
                    chunk(b"data", &samples),
                    list.clone(),
                ]),
            ),
            (
                "odd-data",
                riff(&[
                    extensible_fmt(1, 3, 0x4),
                    chunk(b"data", &samples[..9]),
                    list.clone(),
                ]),
            ),
        ] {
            assert_eq!(copy(name, &input), input, "{name}");
        }

        // Stale chunks about the old samples are dropped.
        let peak = chunk(b"PEAK", &[0; 24]);
        let input = riff(&[quad.clone(), peak, chunk(b"data", &samples)]);
        let want = riff(&[quad, chunk(b"data", &samples)]);
        assert_eq!(copy("stale", &input), want);

        // Mono labelled front centre stays front centre, not front left.
        let mono_fc = extensible_fmt(1, 3, 0x4);
        let input = riff(&[mono_fc, chunk(b"data", &samples), list]);
        assert_eq!(copy("mono", &input), input);
    }

    #[test]
    fn a_new_file_gets_the_header_hound_writes() {
        let spec = int_spec(24);
        let path = temp_path("plain", 24);
        let mut writer = Writer::new(File::create(&path).unwrap(), &path, spec).unwrap();
        // An even number of bytes, since hound leaves out the pad byte.
        writer.write(&[0.5, -0.25, 0.0, -1.0]).unwrap();
        writer.finish().unwrap();
        let ours = std::fs::read(&path).unwrap();

        let mut hounds = WavWriter::create(&path, spec).unwrap();
        for x in [1 << 22, -(1 << 21), 0, -(1 << 23)] {
            hounds.write_sample(x).unwrap();
        }
        hounds.finalize().unwrap();
        assert_eq!(ours, std::fs::read(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
    }
}
