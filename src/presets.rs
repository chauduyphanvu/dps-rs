//! Built-in profiles. The patents publish no tunings, so these are starting
//! points reasoned from what each stage of the chain does, not Bongiovi's.

use clap::builder::{PossibleValuesParser, TypedValueParser};
use tonewell::Profile;

pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    /// Every key of a profile, so it can be copied and edited as it is.
    pub toml: &'static str,
}

pub const PRESETS: [Preset; 5] = [
    Preset {
        name: "headphones",
        description: "gentlest levelling, a little bass and air, mild widening, crossfeed (mixes \
                      at -20 to -4 LUFS settle at -13.5 LUFS)",
        toml: include_str!("../presets/headphones.toml"),
    },
    Preset {
        name: "laptop-speakers",
        description: "bass through harmonics, more presence, wider stereo, firm levelling (mixes \
                      at -20 to -4 LUFS settle at -14.9 LUFS)",
        toml: include_str!("../presets/laptop-speakers.toml"),
    },
    Preset {
        name: "music",
        description: "all-rounder: bass and treble lift, some widening, levelling (mixes at -20 \
                      to -4 LUFS settle at -13.1 LUFS)",
        toml: include_str!("../presets/music.toml"),
    },
    Preset {
        name: "night",
        description: "low volume late at night: heavy levelling, low cut, dialogue lift, stereo \
                      off (a 20 LU quiet/loud swing comes out 3.4 LU)",
        toml: include_str!("../presets/night.toml"),
    },
    Preset {
        name: "voice",
        description: "speech: rumble cut, presence lift, stereo off, firm levelling (speech at \
                      -16 to -24 LUFS settles at -14.5 LUFS)",
        toml: include_str!("../presets/voice.toml"),
    },
];

impl Preset {
    pub fn profile(&self) -> Profile {
        toml::from_str(self.toml).expect("built-in presets are valid TOML profiles")
    }
}

pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// Accepts exactly the preset names, so clap lists them in help and errors.
pub fn parser() -> impl TypedValueParser<Value = &'static Preset> {
    PossibleValuesParser::new(PRESETS.iter().map(|p| p.name))
        .map(|name| find(&name).expect("clap only passes listed names"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonewell::Processor;
    use tonewell::profile::EQ_BANDS;

    /// Deterministic noise in [-0.5, 0.5).
    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
            })
            .collect()
    }

    /// Every key path in `text`, sorted.
    fn keys(text: &str) -> Vec<String> {
        fn walk(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
            for (key, value) in table {
                let key = format!("{prefix}{key}");
                match value {
                    toml::Value::Table(inner) => walk(inner, &format!("{key}."), out),
                    _ => out.push(key),
                }
            }
        }
        let mut out = Vec::new();
        walk(&text.parse().unwrap(), "", &mut out);
        out.sort();
        out
    }

    #[test]
    fn names_are_sorted_and_unique() {
        let names: Vec<_> = PRESETS.iter().map(|p| p.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted);
    }

    #[test]
    fn every_preset_sets_every_key_of_the_profile() {
        let want = keys(&toml::to_string(&Profile::default()).unwrap());
        assert_eq!(want.len(), 50);
        for preset in &PRESETS {
            assert_eq!(keys(preset.toml), want, "{}", preset.name);
        }
    }

    #[test]
    fn presets_are_valid_at_common_rates() {
        for preset in &PRESETS {
            let profile = preset.profile();
            for fs in [44_100, 48_000, 96_000, 192_000] {
                if let Err(e) = profile.validate(fs) {
                    panic!("{} at {fs} Hz: {e}", preset.name);
                }
            }
        }
    }

    #[test]
    fn presets_keep_eq_bands_within_6_db_on_the_default_layout() {
        let default = Profile::default();
        for preset in &PRESETS {
            let profile = preset.profile();
            assert_eq!(profile.eq.freqs_hz, default.eq.freqs_hz, "{}", preset.name);
            for i in 0..EQ_BANDS {
                let g = profile.eq.gains_db[i];
                assert!(g.abs() <= 6.0, "{} band {i}: {g} dB", preset.name);
            }
        }
    }

    #[test]
    fn presets_sound_different_from_the_default_and_each_other() {
        // Stereo noise with an uncorrelated side, loud enough for both
        // compressors and the stereo AGC to act.
        let n = 48_000;
        let (l, r) = (noise(n, 11), noise(n, 12));
        let input: Vec<f32> = l
            .iter()
            .zip(&r)
            .flat_map(|(l, r)| [1.2 * l + 0.3 * r, 1.2 * l - 0.3 * r])
            .collect();
        let run = |profile: &Profile| {
            let mut x = input.clone();
            Processor::new(profile, 48_000, 2).process(&mut x);
            x
        };
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let mut outputs = vec![("default", run(&Profile::default()))];
        outputs.extend(PRESETS.iter().map(|p| (p.name, run(&p.profile()))));
        for (i, (a_name, a)) in outputs.iter().enumerate() {
            for (b_name, b) in &outputs[i + 1..] {
                let diff: Vec<f32> = a.iter().zip(b).map(|(x, y)| x - y).collect();
                // The difference is at least a tenth of the signal, -20 dB.
                let ratio = rms(&diff) / rms(a).max(rms(b));
                assert!(ratio > 0.1, "{a_name} vs {b_name}: {ratio}");
            }
        }
    }
}
