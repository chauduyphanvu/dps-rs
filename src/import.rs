//! `tonewell import`: reads an Equalizer APO / AutoEQ "ParametricEQ.txt" and writes
//! it into the `[correction]` table of a profile file, leaving the rest of the
//! profile alone.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tonewell::Profile;
use tonewell::profile::{BandKind, CorrectionBand, CorrectionParams, MAX_CORRECTION_BANDS};

use crate::PendingFile;
use crate::config::{Dirs, Source};

/// What an LS or HS filter without a Q gets.
const DEFAULT_SHELF_Q: f32 = 0.707;

/// A profile is checked at one rate; bands too high for another are skipped
/// there, not refused.
const CHECK_RATE: u32 = 48_000;

/// The rates a band too high for is warned about.
const COMMON_RATES: [u32; 2] = [44_100, 48_000];

/// Imports `input` into `profile`, else the settings file; `why`, if
/// given, says why that file and is printed after the first line.
pub fn run(
    input: &Path,
    profile: Option<PathBuf>,
    why: Option<String>,
    dirs: &Dirs,
) -> Result<(), String> {
    let text =
        fs::read_to_string(input).map_err(|e| format!("cannot read {}: {e}", input.display()))?;
    let correction = parse(&text).map_err(|e| format!("{}: {e}", input.display()))?;
    // Alone, so that a problem elsewhere in the target is not put down to the
    // import.
    Profile {
        correction: correction.clone(),
        ..Profile::default()
    }
    .validate(CHECK_RATE)
    .map_err(|e| format!("{}: {e}", input.display()))?;

    let source = Source::new(profile, None, dirs)?;
    let (Source::File(path) | Source::Settings(path)) = &source else {
        unreachable!("import takes no preset");
    };
    let mut target = source.load()?;
    target.correction = correction;
    rewrite(path, &target)?;

    let bands = target.correction.bands.len();
    crate::write_stdout(&format!(
        "imported {bands} band{} and a preamp of {} dB into {}\n{}\
         rewrote the file, so comments in it are not kept; a running tonewell picks the change up \
         on its own\n",
        if bands == 1 { "" } else { "s" },
        target.correction.preamp_db,
        source.name(),
        why.map(|why| why + "\n").unwrap_or_default(),
    ))?;
    for warning in skipped_warnings(&target) {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// One warning per common sample rate at which the correction skips bands.
fn skipped_warnings(profile: &Profile) -> Vec<String> {
    COMMON_RATES
        .into_iter()
        .filter_map(|rate| {
            let names: Vec<String> = profile
                .skipped_correction_bands(rate)
                .into_iter()
                .map(|i| {
                    format!(
                        "correction.bands[{i}] ({} Hz)",
                        profile.correction.bands[i].freq_hz
                    )
                })
                .collect();
            let (last, rest) = names.split_last()?;
            let names = if rest.is_empty() {
                last.clone()
            } else {
                format!("{} and {last}", rest.join(", "))
            };
            Some(format!(
                "at {rate} Hz the correction skips {names}: a band must be below half the sample rate"
            ))
        })
        .collect()
}

/// Replaces the file `path` names with `profile` as TOML, through a temporary
/// file beside it, so a `tonewell` watching the file never reads it half written.
/// A symlink stays and the file it points to is replaced; that file keeps its
/// permissions. The file must exist.
pub(crate) fn rewrite(path: &Path, profile: &Profile) -> Result<(), String> {
    let cannot = |e: io::Error| format!("cannot write {}: {e}", path.display());
    let real = fs::canonicalize(path).map_err(cannot)?;
    let permissions = fs::metadata(&real).map_err(cannot)?.permissions();
    let (pending, mut file) = PendingFile::beside(&real)?;
    let text = toml::to_string(profile).expect("a profile always serializes");
    file.write_all(text.as_bytes())
        .and_then(|()| file.set_permissions(permissions))
        .map_err(cannot)?;
    drop(file);
    pending.persist(&real)
}

/// Parses an Equalizer APO parametric EQ file: an optional `Preamp: X dB` and
/// `Filter N: ON|OFF TYPE Fc F Hz Gain G dB Q Q` lines. PK, PEQ, LSC, LS, HSC
/// and HS filters that are ON become correction bands. Errors name the line.
pub fn parse(text: &str) -> Result<CorrectionParams, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut preamp: Option<(usize, f32)> = None;
    let mut bands = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let number = i + 1;
        let line = line.trim_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let item =
            parse_line(line).map_err(|problem| format!("line {number}: {problem}: `{line}`"))?;
        match item {
            Item::Preamp(db) => {
                if let Some((first, _)) = preamp {
                    return Err(format!(
                        "line {number}: a second Preamp line, after the one on line {first}: `{line}`"
                    ));
                }
                preamp = Some((number, db));
            }
            Item::Filter(Some(band)) => bands.push(band),
            Item::Filter(None) => {}
        }
    }
    if bands.len() > MAX_CORRECTION_BANDS {
        return Err(format!(
            "{} filters are ON; at most {MAX_CORRECTION_BANDS} are supported",
            bands.len()
        ));
    }
    // An empty or wrong file would otherwise replace a correction with nothing.
    if bands.is_empty() && preamp.is_none() {
        return Err("no Preamp line and no filter that is ON".into());
    }
    Ok(CorrectionParams {
        enabled: true,
        preamp_db: preamp.map_or(0.0, |(_, db)| db),
        bands,
    })
}

enum Item {
    Preamp(f32),
    /// `None` for an OFF filter.
    Filter(Option<CorrectionBand>),
}

fn fields(s: &str) -> impl Iterator<Item = &str> {
    s.split([' ', '\t']).filter(|field| !field.is_empty())
}

fn parse_line(line: &str) -> Result<Item, String> {
    const EXPECTED: &str = "expected `Preamp:` or `Filter N:`";
    let (head, tail) = line.split_once(':').ok_or(EXPECTED)?;
    let mut head = fields(head);
    let mut tail = fields(tail);
    match (head.next(), head.next(), head.next()) {
        (Some(keyword), None, _) if keyword.eq_ignore_ascii_case("preamp") => {
            let db = quantity(&mut tail, "Preamp", "dB")?;
            match tail.next() {
                None => Ok(Item::Preamp(db)),
                Some(extra) => Err(format!("unexpected `{extra}` after the Preamp")),
            }
        }
        (Some(keyword), Some(n), None) if keyword.eq_ignore_ascii_case("filter") => {
            if !n.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("the filter number `{n}` is not a whole number"));
            }
            parse_filter(&mut tail).map(Item::Filter)
        }
        _ => Err(EXPECTED.into()),
    }
}

/// What follows `Filter N:`.
fn parse_filter<'a>(
    tokens: &mut impl Iterator<Item = &'a str>,
) -> Result<Option<CorrectionBand>, String> {
    match tokens.next() {
        Some(on) if on.eq_ignore_ascii_case("on") => {}
        // Whatever else an OFF filter says does not matter.
        Some(off) if off.eq_ignore_ascii_case("off") => return Ok(None),
        _ => return Err("expected ON or OFF after the filter number".into()),
    }
    let kind_name = tokens.next().ok_or("the filter has no type")?;
    let (kind, q_is_optional) = match kind_name.to_ascii_uppercase().as_str() {
        "PK" | "PEQ" => (BandKind::Peaking, false),
        "LSC" => (BandKind::LowShelf, false),
        "LS" => (BandKind::LowShelf, true),
        "HSC" => (BandKind::HighShelf, false),
        "HS" => (BandKind::HighShelf, true),
        _ => {
            return Err(format!(
                "unsupported filter type `{kind_name}`; only PK, PEQ, LSC, LS, HSC and HS are supported"
            ));
        }
    };
    let (mut freq_hz, mut gain_db, mut q) = (None, None, None);
    while let Some(token) = tokens.next() {
        match token.to_ascii_lowercase().as_str() {
            "fc" => set(&mut freq_hz, "Fc", quantity(tokens, "Fc", "Hz")?)?,
            "gain" => set(&mut gain_db, "Gain", quantity(tokens, "Gain", "dB")?)?,
            "q" => set(&mut q, "Q", number(tokens.next(), "Q")?)?,
            other => {
                let first_order_slope = other
                    .strip_suffix("db")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
                return Err(if first_order_slope {
                    format!("unsupported filter type `{kind_name} {token}`")
                } else {
                    format!("unexpected `{token}` in the filter")
                });
            }
        }
    }
    let q = match (q, q_is_optional) {
        (Some(q), _) => q,
        (None, true) => DEFAULT_SHELF_Q,
        (None, false) => return Err(format!("a {kind_name} filter needs a Q")),
    };
    Ok(Some(CorrectionBand {
        kind,
        freq_hz: freq_hz.ok_or("the filter has no Fc")?,
        q,
        gain_db: gain_db.ok_or("the filter has no Gain")?,
    }))
}

fn set(slot: &mut Option<f32>, name: &str, value: f32) -> Result<(), String> {
    match slot.replace(value) {
        None => Ok(()),
        Some(_) => Err(format!("{name} is given twice")),
    }
}

/// A number and the unit after it, such as `105 Hz`.
fn quantity<'a>(
    tokens: &mut impl Iterator<Item = &'a str>,
    name: &str,
    unit: &str,
) -> Result<f32, String> {
    let value = number(tokens.next(), name)?;
    match tokens.next() {
        Some(u) if u.eq_ignore_ascii_case(unit) => Ok(value),
        Some(u) => Err(format!(
            "expected {unit} after the {name} value, found `{u}`"
        )),
        None => Err(format!("expected {unit} after the {name} value")),
    }
}

fn number(token: Option<&str>, name: &str) -> Result<f32, String> {
    let token = token.ok_or_else(|| format!("{name} has no value"))?;
    match token.parse::<f32>() {
        Ok(v) if v.is_finite() => Ok(v),
        Ok(_) => Err(format!("{name} `{token}` is not a finite number")),
        Err(_) => Err(format!("{name} `{token}` is not a number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::TestDir;

    fn band(kind: BandKind, freq_hz: f32, q: f32, gain_db: f32) -> CorrectionBand {
        CorrectionBand {
            kind,
            freq_hz,
            q,
            gain_db,
        }
    }

    fn one(line: &str) -> CorrectionBand {
        let params = parse(line).unwrap_or_else(|e| panic!("{line:?}: {e}"));
        assert_eq!(params.bands.len(), 1, "{line:?}");
        params.bands[0]
    }

    /// `text` is refused at `line` (1-based) with `needle` in the message,
    /// which quotes that line.
    fn rejected(text: &str, line: usize, needle: &str) {
        let e = parse(text).unwrap_err();
        assert!(e.starts_with(&format!("line {line}: ")), "{e}");
        assert!(e.contains(needle), "{needle:?} not in {e:?}");
        let quoted = text.lines().nth(line - 1).unwrap().trim();
        assert!(e.ends_with(&format!(": `{quoted}`")), "{e}");
    }

    #[test]
    fn the_autoeq_layout_gives_a_preamp_and_the_filters_that_are_on() {
        let params = parse(
            "Preamp: -6.2 dB\n\
             Filter 1: ON LSC Fc 105 Hz Gain 6.5 dB Q 0.70\n\
             Filter 2: ON PK Fc 180 Hz Gain -3.2 dB Q 0.55\n\
             Filter 3: OFF PK Fc 2000 Hz Gain 1.0 dB Q 1.00\n\
             Filter 10: ON HSC Fc 10000 Hz Gain -2.0 dB Q 0.70\n",
        )
        .unwrap();
        assert_eq!(
            params,
            CorrectionParams {
                enabled: true,
                preamp_db: -6.2,
                bands: vec![
                    band(BandKind::LowShelf, 105.0, 0.70, 6.5),
                    band(BandKind::Peaking, 180.0, 0.55, -3.2),
                    band(BandKind::HighShelf, 10_000.0, 0.70, -2.0),
                ],
            }
        );
    }

    #[test]
    fn a_file_without_a_preamp_has_none() {
        let params = parse("Filter 1: ON PK Fc 1000 Hz Gain 2 dB Q 1\n").unwrap();
        assert!(params.enabled);
        assert_eq!(params.preamp_db, 0.0);
        assert_eq!(params.bands.len(), 1);
        let params = parse("Preamp: 2.5 dB").unwrap();
        assert_eq!((params.preamp_db, params.bands.len()), (2.5, 0));
    }

    #[test]
    fn keywords_are_not_case_sensitive() {
        assert_eq!(
            one("fILTER 1: on pk fc 1000 hz GAIN 2 Db q 1.5"),
            band(BandKind::Peaking, 1000.0, 1.5, 2.0)
        );
        assert_eq!(parse("PREAMP: -3 DB").unwrap().preamp_db, -3.0);
        assert_eq!(
            one("Filter 1: On Lsc Fc 90 Hz Gain 1 dB Q 0.7").kind,
            BandKind::LowShelf
        );
    }

    #[test]
    fn fields_are_separated_by_any_run_of_spaces_and_tabs() {
        assert_eq!(
            one("  Filter\t1:\t\tON   PK\tFc  1000 \t Hz Gain 2\tdB   Q\t1.5 \t"),
            band(BandKind::Peaking, 1000.0, 1.5, 2.0)
        );
        assert_eq!(parse("Preamp:\t-3\t dB").unwrap().preamp_db, -3.0);
        // The colon needs no space after it.
        assert_eq!(
            one("Filter 1:ON PK Fc 1000 Hz Gain 2 dB Q 1").freq_hz,
            1000.0
        );
    }

    #[test]
    fn a_bom_blank_lines_comments_and_crlf_are_fine() {
        let text = "\u{feff}# measured on a rig\r\n\r\nPreamp: -1 dB\r\n \t \r\n\
                    # Filter 9: ON PK Fc 5 Hz Gain 1 dB Q 1\r\n\
                    Filter 1: ON PK Fc 1000 Hz Gain 2 dB Q 1\r\n";
        let params = parse(text).unwrap();
        assert_eq!(params.preamp_db, -1.0);
        assert_eq!(params.bands, [band(BandKind::Peaking, 1000.0, 1.0, 2.0)]);
        // The BOM does not shift line numbers.
        rejected("\u{feff}# c\nPreamp: x dB", 2, "Preamp `x` is not a number");
    }

    #[test]
    fn filters_that_are_off_are_skipped_whatever_they_say() {
        let params = parse(
            "Preamp: 0 dB\n\
             Filter 1: OFF PK Fc 2000 Hz Gain 1.0 dB Q 1.00\n\
             Filter 2: off LP Fc 100 Hz\n\
             Filter 3: OFF\n",
        )
        .unwrap();
        assert!(params.enabled);
        assert!(params.bands.is_empty());
    }

    #[test]
    fn a_file_with_no_preamp_and_no_filter_on_is_refused() {
        for text in [
            "",
            "# nothing\n\n",
            "Filter 1: OFF PK Fc 2000 Hz Gain 1 dB Q 1\n",
        ] {
            assert_eq!(
                parse(text).unwrap_err(),
                "no Preamp line and no filter that is ON"
            );
        }
    }

    #[test]
    fn filter_numbers_are_not_checked_for_order_or_gaps() {
        let params = parse(
            "Filter 7: ON PK Fc 100 Hz Gain 1 dB Q 1\n\
             Filter 2: ON PK Fc 200 Hz Gain 1 dB Q 1\n\
             Filter 2: ON PK Fc 300 Hz Gain 1 dB Q 1\n\
             Filter 40: ON PK Fc 400 Hz Gain 1 dB Q 1\n",
        )
        .unwrap();
        let freqs: Vec<f32> = params.bands.iter().map(|b| b.freq_hz).collect();
        assert_eq!(freqs, [100.0, 200.0, 300.0, 400.0]);
    }

    #[test]
    fn peaking_and_shelf_types_and_their_aliases() {
        for (kind, name) in [
            (BandKind::Peaking, "PK"),
            (BandKind::Peaking, "PEQ"),
            (BandKind::LowShelf, "LSC"),
            (BandKind::LowShelf, "LS"),
            (BandKind::HighShelf, "HSC"),
            (BandKind::HighShelf, "HS"),
        ] {
            let b = one(&format!("Filter 1: ON {name} Fc 500 Hz Gain -2.5 dB Q 0.9"));
            assert_eq!(b, band(kind, 500.0, 0.9, -2.5), "{name}");
        }
    }

    #[test]
    fn ls_and_hs_may_leave_out_q_but_nothing_else_may() {
        assert_eq!(
            one("Filter 1: ON LS Fc 100 Hz Gain 3 dB"),
            band(BandKind::LowShelf, 100.0, 0.707, 3.0)
        );
        assert_eq!(
            one("Filter 1: ON HS Fc 8000 Hz Gain -3 dB"),
            band(BandKind::HighShelf, 8000.0, 0.707, -3.0)
        );
        assert_eq!(one("Filter 1: ON LS Fc 100 Hz Gain 3 dB Q 1.2").q, 1.2);
        for kind in ["PK", "PEQ", "LSC", "HSC"] {
            let line = format!("Filter 1: ON {kind} Fc 100 Hz Gain 3 dB");
            rejected(&line, 1, &format!("a {kind} filter needs a Q"));
        }
    }

    #[test]
    fn twenty_filters_are_fine() {
        let line = "Filter 1: ON PK Fc 1000 Hz Gain 1 dB Q 1\n";
        let off = "Filter 1: OFF PK Fc 1000 Hz Gain 1 dB Q 1\n";
        let text = format!("{}{off}{off}", line.repeat(MAX_CORRECTION_BANDS));
        assert_eq!(parse(&text).unwrap().bands.len(), MAX_CORRECTION_BANDS);
    }

    #[test]
    fn more_than_twenty_filters_are_refused_with_the_count_and_the_limit() {
        let line = "Filter 1: ON PK Fc 1000 Hz Gain 1 dB Q 1\n";
        let e = parse(&line.repeat(23)).unwrap_err();
        assert_eq!(e, "23 filters are ON; at most 20 are supported");
    }

    #[test]
    fn a_second_preamp_is_refused() {
        rejected(
            "Preamp: -3 dB\n# again\nPreamp: -4 dB\n",
            3,
            "a second Preamp line, after the one on line 1",
        );
    }

    #[test]
    fn other_filter_types_are_refused() {
        for kind in ["LP", "HP", "LPQ", "HPQ", "BP", "NO", "AP", "lp", "XYZ"] {
            let text = format!("Preamp: -1 dB\nFilter 1: ON {kind} Fc 100 Hz Gain 3 dB Q 0.7\n");
            rejected(&text, 2, &format!("unsupported filter type `{kind}`"));
        }
        rejected(
            "# a\n\nFilter 1: ON LS 6dB Fc 100 Hz Gain 3 dB\n",
            3,
            "unsupported filter type `LS 6dB`",
        );
        rejected(
            "Filter 1: ON HS 12dB Fc 100 Hz Gain 3 dB\n",
            1,
            "unsupported filter type `HS 12dB`",
        );
        rejected(
            "Filter 1: ON PK Fc 100 Hz BW Oct 1 Gain 3 dB\n",
            1,
            "unexpected `BW`",
        );
    }

    #[test]
    fn other_lines_are_refused() {
        for line in [
            "Device: Speakers",
            "Include: other.txt",
            "Channel: L",
            "Filter 1 ON PK Fc 100 Hz Gain 3 dB Q 1",
            "Preamp -3 dB",
            "Filters: ON PK Fc 100 Hz Gain 3 dB Q 1",
            "garbage",
        ] {
            rejected(
                &format!("\n{line}\n"),
                2,
                "expected `Preamp:` or `Filter N:`",
            );
        }
        rejected(
            "Filter x: ON PK Fc 100 Hz Gain 3 dB Q 1",
            1,
            "the filter number `x` is not a whole number",
        );
        rejected(
            "Filter: ON PK Fc 100 Hz Gain 3 dB Q 1",
            1,
            "expected `Preamp:`",
        );
        rejected(
            "Preamp: -3 dB extra",
            1,
            "unexpected `extra` after the Preamp",
        );
    }

    #[test]
    fn a_filter_needs_its_state_type_frequency_and_gain() {
        rejected("Filter 1:", 1, "expected ON or OFF");
        rejected(
            "Filter 1: PK Fc 100 Hz Gain 3 dB Q 1",
            1,
            "expected ON or OFF",
        );
        rejected("Filter 1: ON", 1, "the filter has no type");
        rejected("Filter 1: ON PK Gain 3 dB Q 1", 1, "the filter has no Fc");
        rejected("Filter 1: ON PK Fc 100 Hz Q 1", 1, "the filter has no Gain");
        rejected("Filter 1: ON HS Fc 100 Hz", 1, "the filter has no Gain");
        rejected("Filter 1: ON LS Gain 3 dB", 1, "the filter has no Fc");
        rejected(
            "Filter 1: ON PK Fc 100 Hz Fc 200 Hz Gain 3 dB Q 1",
            1,
            "Fc is given twice",
        );
    }

    #[test]
    fn numbers_must_parse_and_be_finite() {
        rejected("Preamp: -x dB", 1, "Preamp `-x` is not a number");
        rejected("Preamp: -6,2 dB", 1, "Preamp `-6,2` is not a number");
        rejected("Preamp:", 1, "Preamp has no value");
        rejected("Preamp: inf dB", 1, "Preamp `inf` is not a finite number");
        rejected("Preamp: 1e999 dB", 1, "is not a finite number");
        rejected("Preamp: -6", 1, "expected dB after the Preamp value");
        rejected(
            "Preamp: -6 Hz",
            1,
            "expected dB after the Preamp value, found `Hz`",
        );
        let filter = |fc: &str, gain: &str, q: &str| {
            format!("# c\nFilter 1: ON PK Fc {fc} Hz Gain {gain} dB Q {q}\n")
        };
        rejected(&filter("abc", "1", "1"), 2, "Fc `abc` is not a number");
        rejected(
            &filter("NaN", "1", "1"),
            2,
            "Fc `NaN` is not a finite number",
        );
        rejected(
            &filter("100", "-inf", "1"),
            2,
            "Gain `-inf` is not a finite number",
        );
        rejected(&filter("100", "1", "x"), 2, "Q `x` is not a number");
        rejected(
            "Filter 1: ON PK Fc 100 Gain 3 dB Q 1",
            1,
            "expected Hz after the Fc value, found `Gain`",
        );
        rejected(
            "Filter 1: ON PK Fc 100",
            1,
            "expected Hz after the Fc value",
        );
        rejected("Filter 1: ON PK Fc 100 Hz Gain 3 dB Q", 1, "Q has no value");
    }

    #[test]
    fn a_problem_names_the_first_line_it_is_on() {
        let text = "Preamp: -1 dB\nFilter 1: ON PK Fc 100 Hz Gain 3 dB Q 1\n\
                    Filter 2: ON LP Fc 100 Hz\nFilter 3: ON ZZ Fc 100 Hz\n";
        rejected(text, 3, "unsupported filter type `LP`");
    }

    #[test]
    fn the_warning_names_the_bands_a_common_rate_skips() {
        let mut p = Profile::default();
        assert!(skipped_warnings(&p).is_empty());
        p.correction.bands = vec![
            band(BandKind::Peaking, 1000.0, 1.0, 1.0),
            band(BandKind::Peaking, 23_000.0, 1.0, 1.0),
        ];
        assert_eq!(
            skipped_warnings(&p),
            [
                "at 44100 Hz the correction skips correction.bands[1] (23000 Hz): \
                 a band must be below half the sample rate"
            ]
        );
        p.correction
            .bands
            .push(band(BandKind::HighShelf, 24_000.0, 0.7, -1.0));
        assert_eq!(
            skipped_warnings(&p),
            [
                "at 44100 Hz the correction skips correction.bands[1] (23000 Hz) and \
                 correction.bands[2] (24000 Hz): a band must be below half the sample rate",
                "at 48000 Hz the correction skips correction.bands[2] (24000 Hz): \
                 a band must be below half the sample rate",
            ]
        );
    }

    #[test]
    fn rewrite_replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = TestDir::new("import-rewrite");
        let path = dir.0.join("profile.toml");
        fs::write(&path, "# mine\noutput_gain_db = 1.0\n").unwrap();
        let mut profile = Profile::default();
        profile.correction.enabled = true;
        rewrite(&path, &profile).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            toml::to_string(&profile).unwrap()
        );
        let names: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["profile.toml"]);

        let missing = dir.0.join("nope").join("profile.toml");
        assert!(
            rewrite(&missing, &profile)
                .unwrap_err()
                .starts_with("cannot write ")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_keeps_a_symlink_and_the_permissions_of_the_file() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = TestDir::new("import-rewrite-link");
        let real = dir.0.join("real.toml");
        let link = dir.0.join("link.toml");
        fs::write(&real, "").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        symlink("real.toml", &link).unwrap();
        rewrite(&link, &Profile::default()).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            toml::to_string(&Profile::default()).unwrap()
        );
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
