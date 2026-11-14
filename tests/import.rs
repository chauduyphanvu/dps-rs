//! Drives `dps import` through the built binary.

use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use dps::Profile;
use dps::biquad::Coefs;
use dps::profile::{BandKind, CorrectionBand, CorrectionParams};
use hound::{SampleFormat, WavSpec};

const FS: f64 = 48_000.0;

const HEADPHONES: &str = "\
# a made-up headphone
Preamp: -3.0 dB
Filter 1: ON PK Fc 1000 Hz Gain 6.0 dB Q 1.41
Filter 2: ON HSC Fc 8000 Hz Gain -4.0 dB Q 0.70
Filter 3: OFF PK Fc 300 Hz Gain 5.0 dB Q 1.00
";

fn headphones() -> CorrectionParams {
    CorrectionParams {
        enabled: true,
        preamp_db: -3.0,
        bands: vec![
            band(BandKind::Peaking, 1000.0, 1.41, 6.0),
            band(BandKind::HighShelf, 8000.0, 0.70, -4.0),
        ],
    }
}

fn band(kind: BandKind, freq_hz: f32, q: f32, gain_db: f32) -> CorrectionBand {
    CorrectionBand {
        kind,
        freq_hz,
        q,
        gain_db,
    }
}

/// A per-test directory, removed on drop. The binary runs with its home and
/// config directories inside it, so no test touches the real settings file.
struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(test: &str) -> Self {
        let root = std::env::temp_dir().join(format!("dps-import-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for sub in ["work", "config", "home"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { root }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.root.join("work").join(name)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    /// Where `dps` looks for its settings file here.
    fn settings(&self) -> PathBuf {
        self.root.join("config").join("dps").join("profile.toml")
    }

    /// Sorted names of the files in the work directory.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.root.join("work"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn dps(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dps"));
        cmd.env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("HOME", self.root.join("home"));
        cmd
    }

    fn import(&self, input: &Path, profile: Option<&Path>) -> Output {
        let mut cmd = self.dps();
        cmd.arg("import").arg(input);
        if let Some(profile) = profile {
            cmd.arg("--profile").arg(profile);
        }
        cmd.output().unwrap()
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_success(out: &Output) {
    assert!(out.status.success(), "dps failed: {}", stderr(out));
}

/// Exit code 1 with one stderr line, which mentions each of `needles`.
fn assert_user_error(out: &Output, needles: &[&str]) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(1), "stderr: {err}");
    assert_eq!(err.lines().count(), 1, "expected one stderr line: {err}");
    for needle in needles {
        assert!(err.contains(needle), "{needle:?} not in stderr: {err}");
    }
    assert!(!err.contains("panicked"), "{err}");
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(out));
}

fn load(path: &Path) -> Profile {
    toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// A mono 48 kHz float WAV, one second of the sum of sines `(Hz, amplitude)`.
fn write_tones(path: &Path, tones: &[(f64, f64)]) {
    let spec = WavSpec {
        channels: 1,
        sample_rate: FS as u32,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for n in 0..FS as usize {
        let t = n as f64 / FS;
        let x: f64 = tones
            .iter()
            .map(|(f, a)| a * (2.0 * PI * f * t).sin())
            .sum();
        writer.write_sample(x as f32).unwrap();
    }
    writer.finalize().unwrap();
}

/// The second half of a WAV's samples, after the chain has settled.
fn settled(path: &Path) -> Vec<f32> {
    let samples: Vec<f32> = hound::WavReader::open(path)
        .unwrap()
        .samples::<f32>()
        .map(Result::unwrap)
        .collect();
    samples[samples.len() / 2..].to_vec()
}

/// Amplitude of the `freq_hz` component of `x`, which holds whole cycles.
fn amplitude(x: &[f32], freq_hz: f64) -> f64 {
    let w = 2.0 * PI * freq_hz / FS;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, v) in x.iter().enumerate() {
        re += f64::from(*v) * (w * n as f64).cos();
        im += f64::from(*v) * (w * n as f64).sin();
    }
    2.0 * (re * re + im * im).sqrt() / x.len() as f64
}

#[test]
fn import_creates_the_settings_file_and_the_bands_shape_the_audio() {
    let dir = TempDir::new("settings");
    let file = dir.write("hd.txt", HEADPHONES);
    let settings = dir.settings();
    assert!(!settings.exists());

    let out = dir.import(&file, None);
    assert_success(&out);
    let said = stdout(&out);
    assert_eq!(
        said.lines().next().unwrap(),
        format!(
            "imported 2 bands and a preamp of -3 dB into {}",
            settings.display()
        )
    );
    assert!(said.contains("comments in it are not kept"), "{said}");
    assert!(said.contains("picks the change up on its own"), "{said}");
    assert!(!stderr(&out).contains("warning"), "{}", stderr(&out));

    let imported = load(&settings);
    assert_eq!(
        imported,
        Profile {
            correction: headphones(),
            ..Profile::default()
        }
    );

    // The same profile with the correction off differs from the imported one
    // by exactly the correction, which comes last before the limiter.
    let input = dir.join("tones.wav");
    write_tones(&input, &[(1000.0, 0.03), (100.0, 0.03)]);
    let (with, without) = (dir.join("with.wav"), dir.join("without.wav"));
    assert_success(&dir.process(&input, &with, None));
    let mut off = imported.clone();
    off.correction.enabled = false;
    let off = dir.write("off.toml", &toml::to_string(&off).unwrap());
    assert_success(&dir.process(&input, &without, Some(&off)));

    let (with, without) = (settled(&with), settled(&without));
    let peaking = Coefs::peaking(1000.0, f64::from(1.41f32), 6.0, FS);
    let shelf = Coefs::high_shelf(8000.0, f64::from(0.70f32), -4.0, FS);
    for freq in [1000.0, 100.0] {
        let got = 20.0 * (amplitude(&with, freq) / amplitude(&without, freq)).log10();
        let want = -3.0 + peaking.magnitude_db(freq, FS) + shelf.magnitude_db(freq, FS);
        assert!(
            (got - want).abs() < 0.02,
            "{freq} Hz: {got} dB, want {want} dB"
        );
    }
    // The centre of the band: its 6 dB less the 3 dB preamp.
    let at_centre = 20.0 * (amplitude(&with, 1000.0) / amplitude(&without, 1000.0)).log10();
    assert!((at_centre - 3.0).abs() < 0.1, "{at_centre} dB at 1 kHz");
}

#[test]
fn import_into_a_profile_file_changes_only_the_correction() {
    let dir = TempDir::new("profile-file");
    let mut mine = Profile {
        input_gain_db: 1.5,
        output_gain_db: -2.5,
        low_shelf_db: -6.0,
        ..Profile::default()
    };
    mine.compressor1.ratio = 3.5;
    mine.compressor2.attack_ms = 12.0;
    mine.eq.gains_db[3] = 4.5;
    mine.eq.freqs_hz[5] = 640.0;
    mine.eq.q[7] = 0.9;
    mine.leveler.enabled = true;
    mine.leveler.target_lufs = -18.0;
    mine.bass.enabled = true;
    mine.contour.enabled = true;
    mine.stereo.enabled = true;
    mine.crossfeed.enabled = true;
    mine.limiter.ceiling_db = -2.0;
    // A correction already there is replaced, switched on or not.
    mine.correction = CorrectionParams {
        enabled: false,
        preamp_db: -1.0,
        bands: vec![band(BandKind::LowShelf, 60.0, 0.7, 2.0)],
    };
    let before = format!("# my settings\n{}", toml::to_string(&mine).unwrap());
    let path = dir.write("mine.toml", &before);
    let file = dir.write("hd.txt", HEADPHONES);

    let out = dir.import(&file, Some(&path));
    assert_success(&out);
    assert_eq!(
        stdout(&out).lines().next().unwrap(),
        format!(
            "imported 2 bands and a preamp of -3 dB into {}",
            path.display()
        )
    );
    assert!(!dir.settings().exists(), "the settings file was created");

    let after = fs::read_to_string(&path).unwrap();
    let mut was: toml::Table = toml::from_str(&before).unwrap();
    let mut is: toml::Table = toml::from_str(&after).unwrap();
    assert_ne!(was.remove("correction"), is.remove("correction"));
    assert_eq!(was, is);
    assert_eq!(
        load(&path),
        Profile {
            correction: headphones(),
            ..mine
        }
    );
    assert_eq!(dir.names(), ["hd.txt", "mine.toml"]);
}

#[test]
fn a_malformed_file_prints_its_line_and_changes_nothing() {
    let dir = TempDir::new("malformed");
    let before = "# mine\noutput_gain_db = -2.0   # quieter\n";
    let path = dir.write("mine.toml", before);

    let bad = dir.write(
        "bad.txt",
        "Preamp: -3.0 dB\nFilter 1: ON PK Fc 1000 Hz Gain 6.0 dB Q 1.41\n\
         Filter 2: ON LP Fc 8000 Hz Q 0.70\n",
    );
    let out = dir.import(&bad, Some(&path));
    assert_user_error(
        &out,
        &[
            "bad.txt: line 3: unsupported filter type `LP`",
            "`Filter 2: ON LP Fc 8000 Hz Q 0.70`",
        ],
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    assert_eq!(dir.names(), ["bad.txt", "mine.toml"]);

    // Nor is a settings file made for an import that cannot succeed.
    assert_user_error(&dir.import(&bad, None), &["line 3"]);
    assert!(!dir.settings().exists());

    let out = dir.import(&dir.join("missing.txt"), Some(&path));
    assert_user_error(&out, &["cannot read", "missing.txt"]);

    // A value the profile does not allow is refused, naming the import file.
    let steep = dir.write("steep.txt", "Filter 1: ON PK Fc 1000 Hz Gain 3 dB Q 25\n");
    let out = dir.import(&steep, Some(&path));
    assert_user_error(
        &out,
        &[
            "steep.txt",
            "correction.bands[0].q = 25 is outside 0.1..=20",
        ],
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);

    // A target that is not a profile is left as it is.
    let garbled = dir.write("garbled.toml", "this is [not toml\n");
    let good = dir.write("hd.txt", HEADPHONES);
    let out = dir.import(&good, Some(&garbled));
    assert_user_error(&out, &["garbled.toml"]);
    assert_eq!(fs::read_to_string(&garbled).unwrap(), "this is [not toml\n");
    assert_eq!(
        dir.names(),
        [
            "bad.txt",
            "garbled.toml",
            "hd.txt",
            "mine.toml",
            "steep.txt"
        ]
    );
}

#[test]
fn importing_twice_replaces_the_correction() {
    let dir = TempDir::new("twice");
    let first = dir.write("first.txt", HEADPHONES);
    let second = dir.write(
        "second.txt",
        "Preamp: 1.5 dB\nFilter 1: ON LS Fc 100 Hz Gain 2.0 dB\n",
    );

    assert_success(&dir.import(&first, None));
    let once = fs::read_to_string(dir.settings()).unwrap();
    assert_success(&dir.import(&first, None));
    assert_eq!(fs::read_to_string(dir.settings()).unwrap(), once);

    let out = dir.import(&second, None);
    assert_success(&out);
    assert!(
        stdout(&out).starts_with("imported 1 band and a preamp of 1.5 dB into "),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        load(&dir.settings()),
        Profile {
            correction: CorrectionParams {
                enabled: true,
                preamp_db: 1.5,
                bands: vec![band(BandKind::LowShelf, 100.0, 0.707, 2.0)],
            },
            ..Profile::default()
        }
    );
    assert_eq!(dir.names(), ["first.txt", "second.txt"]);
}

#[test]
fn bands_too_high_for_a_common_rate_are_warned_about() {
    let dir = TempDir::new("warn");
    let high = dir.write(
        "high.txt",
        "Filter 1: ON PK Fc 1000 Hz Gain 2 dB Q 1\n\
         Filter 2: ON PK Fc 23000 Hz Gain 2 dB Q 1\n\
         Filter 3: ON HSC Fc 24000 Hz Gain -2 dB Q 0.7\n",
    );
    let out = dir.import(&high, None);
    assert_success(&out);
    let err = stderr(&out);
    assert!(
        err.contains(
            "warning: at 44100 Hz the correction skips correction.bands[1] (23000 Hz) and \
             correction.bands[2] (24000 Hz)"
        ),
        "{err}"
    );
    assert!(
        err.contains("warning: at 48000 Hz the correction skips correction.bands[2] (24000 Hz):"),
        "{err}"
    );
    // The bands are kept: a profile is valid at every rate.
    assert_eq!(load(&dir.settings()).correction.bands.len(), 3);
}
