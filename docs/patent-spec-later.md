# Bongiovi Acoustics: later DSP patents (beyond the core shelf / compressor / shelf / EQ chain)

Scope: patents filed after the core 2006-2012 "System and method for digital signal processing" shelf, compressor, shelf, 11-band graphic EQ chain (covered by another researcher). This file covers each additional processing block, quotes every numeric parameter, and says what is left unspecified.

## 0. How to read this file

- **Verbatim** patent text is in blockquotes or in double quotes and is marked with `¶[nnnn]`. These are Google Patents paragraph IDs (the application-publication numbering). Google's text carries no column/line numbers, so column citations are not possible. Claim text is cited as "claim N".
- Lines starting with **Inference:** are mine, not the patents'. Lines starting with **Figure-only:** are things I read off a drawing sheet (PNG from Google Patents) and that the text does not state.
- Source for every patent: `https://patents.google.com/patent/US<number>B2/en`, fetched at about one request per 4 s, generic User-Agent, no personal details. `robots.txt` on patents.google.com allows `/patent/`. About 16 drawing sheets were fetched from `patentimages.storage.googleapis.com`, which serves no robots.txt (AccessDenied XML). Justia returned HTTP 403 to curl and to WebFetch; I did not retry or work around it. WebSearch was used only to discover patent numbers. No content from search snippets is used here.
- Google's paragraph numbering skips numbers where pseudo-code or equations sit (for example ¶[0041]-[0042] and ¶[0046]/[0048] in US 9,195,433). Equations in the long 2008-era text are MathML that Google flattens badly, so I did not rely on those.
- **Reading depth.** Read in full: 9195433, 9348904 (diffed to 9195433), 9397629 (diffed), 9906858 (delta read in full), 9398394, 9264004, 9564146, 9741355 (diffed against 9264004, claims 1-12 read), 9883318 / 10412533 / 10999695 (diffed against 9398394 incl. every number token, claim 1 read), 10917722 / 10069471 / 10848867 / 11425499 (diffed, claims read in full), 9344828 FIG. 16 text and claims, 9276542 claims and FIG. 16 text, 9350309 claims, 9281794 safety-parameter text and claims, 9413321 material-profile text and claims, 10701505 DSP section. Claims plus abstract only: 9793872, 10158337, 10666216, 10848118, 11431312 (diffed against 9413321 or 9350309; only continuation boilerplate changes), 9615189, 11211043, 8705765.
- The ~300-paragraph core text embedded in 9344828 / 9276542 / 9281794 / 9413321 / 9350309 (shelf theory, Mitra-Regalia, C code listing) is the other researcher's. I point to it where a later block reuses it.

## 1. Confirmed patent list

All numbers, titles and dates below are from the fetched Google Patents pages (dates = filing / grant, per Google's DC.date metadata). Assignee on all: Bongiovi Acoustics LLC.

| US patent | Title | Filed / granted | Inventors (as listed) | Block family |
|---|---|---|---|---|
| 9,195,433 | In-line signal processor | 2014-01-13 / 2015-11-24 | Bongiovi, Zelniker, Butera III, Fuller, Qiu Xihong | **A** HPF + 3-band chain, inside a cable/profile module |
| 9,348,904 | System and method for digital signal processing | 2013-10-22 / 2016-05-24 | Bongiovi, Zelniker, Fuller | **A** (same disclosure as 9195433) |
| 9,397,629 | System and method for digital signal processing | 2013-10-22 / 2016-07-19 | Bongiovi, Fuller, Zelniker | **A** (same disclosure) |
| 9,906,858 | System and method for digital signal processing | 2016-07-19 / 2018-02-27 | Bongiovi, Fuller, Zelniker | **A'** adds input gain, parallel splitter (sidechain) variants, 2-band option |
| 10,069,471 | System and method for digital signal processing | 2016-05-24 / 2018-09-04 | Bongiovi, Zelniker, Fuller | **A** claims numeric shelf ranges |
| 10,848,867 | System and method for digital signal processing | 2018-09-04 / 2020-11-24 | same | **A** same claims family |
| 11,425,499 | System and method for digital signal processing | 2020-11-24 / 2022-08-23 | same | **A** claim 1 fixes 1000 Hz / -24 dB |
| 10,917,722 | System and method for digital signal processing | 2019-06-04 / 2021-02-09 | Bongiovi, Fuller, Zelniker | **A'** claims 2-band + generic "modulators" |
| 10,701,505 | System, method, and apparatus for generating and digitally processing a head related audio transfer function | 2018-01-08 / 2020-06-30 | Copt, Butera III, Summers III, Bongiovi, Zelniker, Fuller | Uses **A**; ¶[0064] states the second-shelf numeric ranges |
| 9,344,828 | System and method for digital signal processing | 2012-12-21 / 2016-05-17 | Bongiovi, Fuller, Zelniker, Butera III | **B** broadcast pre/post-transmission split |
| 9,276,542 | System and method for digital signal processing | 2013-03-14 / 2016-03-01 | same | **B** (claims the split with the core chain) |
| 9,350,309 | System and method for digital signal processing | 2012-10-09 / 2016-05-24 | Bongiovi, Fuller, Zelniker | **C** core chain into headphones + claimed output limiter |
| 9,793,872 | System and method for digital signal processing | 2016-05-24 / 2017-10-17 | same | **C** continuation |
| 9,398,394 | System and method for stereo field enhancement in two-channel audio systems | 2013-07-08 / 2016-07-19 | Bongiovi, Zelniker, Butera III | **D** M/S dynamic width |
| 9,883,318 | same title | 2016-07-19 / 2018-01-30 | same | **D** continuation-in-part |
| 10,412,533 | same title | 2018-01-30 / 2019-09-10 | same | **D** |
| 10,999,695 | same title ("two channel") | 2019-09-10 / 2021-05-04 | same | **D** |
| 9,264,004 | System and method for narrow bandwidth digital signal processing | 2013-09-20 / 2016-02-16 | Bongiovi, Zelniker, Butera III | **E** voice / narrow band |
| 9,741,355 | same title | 2016-02-16 / 2017-08-22 | same | **E** continuation |
| 9,564,146 | System and method for digital signal processing in deep diving environment | 2014-08-01 / 2017-02-07 | Bongiovi, Copt, Fuller, Butera III, Zelniker | **F** pitch shift + shelves + AGC + EQ |
| 9,281,794 | System and method for digital signal processing | 2012-04-10 / 2016-03-08 | Bongiovi, Fuller, Zelniker | **G** "safe listening" peak ceiling |
| 9,413,321 | System and method for digital signal processing | 2013-12-23 / 2016-08-09 | Bongiovi, Zelniker, Fuller, Copt | **H** material-profile EQ for non-loudspeaker transducers |
| 10,158,337 / 10,666,216 / 10,848,118 / 11,431,312 | same title | 2016-2022 | + Copt | **H** continuations |
| 9,615,189 | Artificial ear apparatus and associated methods for generating a head related audio transfer function | 2014-09-12 / 2017-04-04 | Copt, Butera III, Summers III | **I** acoustic hardware (no DSP block) |
| 11,211,043 | Audio enhanced hearing protection system | 2019-04-11 / 2021-12-28 | Copt, Butera III, Summers III, Harpster | **I** hardware (no DSP block) |
| 8,705,765 | Ringtone enhancement systems and methods | 2010-01-06 / 2014-04-22 | Bongiovi | **I** profile loading + core chain (core era) |

Corrections to the brief's list (all from page titles I fetched):
- 9,276,542, 9,344,828 and 9,350,309 are **not** the HPF / band-split generation. They are the broadcast pre/post split (9,276,542, 9,344,828) and the headphones case (9,350,309). The HPF + 3-band generation is 9,195,433, 9,348,904, 9,397,629, 9,906,858 and the 10,069,471 / 10,848,867 / 11,425,499 / 10,917,722 continuations.
- 9,397,629 is the 3-band chain, **not** stereo. Stereo field is 9,398,394 (+ 9,883,318, 10,412,533, 10,999,695).
- 9,264,004 is "narrow bandwidth", not stereo. 9,564,146 is deep diving (confirmed).
- 9,615,189 is an artificial-ear HRTF generator (acoustic hardware). 9,621,994 / 9,906,867 / 9,998,832 are "Surface acoustic transducer" (hardware; titles seen in Google's cited-by tables, pages not read). I found no Bongiovi patent titled for "bass enhancement".
- Internal family cross-references disagree on one number (10,848,118's text calls application 16/224,067 "U.S. Pat. No. 10,622,958"; Google lists that application as 10,666,216). I used Google's headers. This does not affect any DSP content.

## 2. Family map and what is new vs. the core

```
Core (other researcher):  gain -> LS1 -> HS1 -> COMP1 -> LS2 -> HS2 -> 11-band bell EQ -> COMP2 -> gain

A   HPF -> [LS1+HS1] -> COMP -> [LS2+HS2 = inverse] -> gain + 10 peak/dip -> 3-band split
        -> (low COMP, mid straight, high COMP) -> weighted sum -> gain          (9195433, 9348904, 9397629, 9906858, ...)
B   same shelves/compressor/EQ, cut in two halves with a codec in between      (9344828, 9276542)
C   core chain, claimed with a final gain-reduction-above-threshold stage        (9350309, 9793872)
D   crossover -> M/S -> shelf-weighted detector -> side-channel AGC -> recombine
        (+ optional HF-band L/R delay)                                           (9398394 family)
E   resonant HPF -> comp -> clip -> split -> [resonant LPF || pass-through] -> sum -> EQ -> comp   (9264004, 9741355)
F   pitch shift -> LS/HS (24 dB tilt) -> AGC (0-20 dB) -> LS/HS (inverse) -> EQ <200 Hz -> gain      (9564146)
G   a threshold = profile value - safety parameter (20-30 dB, e.g. 23 dB) wired into the core's gain and compressor stages  (9281794)
H   graphic EQ coefficients chosen by a "material profile" for a vibration transducer   (9413321 family)
```

The shared "compressor" in A, D, E and the core is described by the same pseudo-code (section 3).

## 3. Shared primitive reused by blocks A, D, E, G: the compressor

This is the one primitive the later patents all point back to without re-specifying. It is verbatim in 9195433 (¶[0045]-[0049]), 9348904 (starts ¶[0040]), 9397629 (starts ¶[0039]); the long-form theory is in the core text (9344828 ¶[0117]-[0135]).

Verbatim, 9195433 ¶[0045]-[0049]:

> The compressor first computes an approximation of the signal level, where att represents attack time; rel represents release time; and invThr represents a precomputed threshold:
> ```
> temp = abs(x(k))
> if temp > level (k-1)
>   level(k) = att * (level(k-1) − temp) + temp
> else
>   level = rel * (level(k-1) − temp) + temp
> ```
> This level computation is done for each input sample. The ratio of the signal's level to invThr then determines the next step. If the ratio is less than one, the signal is passed through unaltered. If the ratio exceeds one, a table in the memory may provide a constant that's a function of both invThr and level:
> ```
> if (level * thr < 1)
>   output(k) = x(k)
> else
>   index = floor(level * invThr)
> if (index > 99)
>   index = 99
> gainReduction = table [index]
> output(k) = gainReduction * x(k)
> ```

Control set, ¶[0032]: "standard dynamic range compression controls such as threshold, ratio, attack and release", "soft and hard knees", and "automatic gain controller".

Core text (9344828 ¶[0117]): "A compressor is characterized by four quantities: the attack time, Tatt, the release time, Trel, the threshold, KT, and the ratio, r." ¶[0132]/[0135]: GR table of "100 integral points of Gexcess" and "tables for six different values of ratio, r". ¶[0129] (reconstructed from MathML, check against PDF): GR = (Gexcess)^((1−r)/r) with Gexcess = L[k]·KT⁻¹ (¶[0127]).

Left unspecified in every patent: numeric attack, release, threshold or ratio for any compressor in blocks A, D, E, G; the table contents; how `att`/`rel` (a per-sample coefficient) relates to milliseconds; whether `thr` in the `if` is `invThr` (the code mixes the two names, so treat it as a typo). **Inference:** with `level(k) = att*(level(k-1) - temp) + temp`, `att` and `rel` are one-pole coefficients in [0,1) (closer to 1 is slower), and `level*invThr >= 1` means "level at or above threshold". The table is indexed by excess-gain ratio 1..99 (index above 99 clamps), so it is a static curve in linear gain.

Band-split compressors in A: ¶[0036]: "each of the low band compressor 130 and high band compressor 131 may be the same as the first compressor 114", and ¶[0015] "computationally and/or configured identically as the first compressor".

---

# 4. Block A: HPF + shelf pair + compressor + inverse shelf pair + peak/dip + 3-band split

**Patents:** 9,195,433, 9,348,904, 9,397,629 (identical technical disclosure; the in-line patent wraps it in a cable). 9,906,858 and 10,917,722 add variants (4.9). Claim-only numeric ranges: 10,069,471 / 10,848,867 / 11,425,499 (4.10). Paragraph IDs below are 9195433's; the same paragraphs sit at 9348904 ¶[0025]-[0047] and 9397629 ¶[0024]-[0059] (no numeric differences).

## 4.1 Stage order (FIG. 1 / FIG. 2, ¶[0028], method FIG. 3 / FIG. 4)

```
in -> [HPF, 4th order = 2 biquads] -> [first filter module: LS1 then HS1, each 2nd order]
   -> [COMP1] -> [second filter module: LS2 then HS2 = (partial) inverse of first]
   -> [first processing module: gain element (static) -> peak/dip bank of 10 cascaded 2nd-order filters]
   -> [band splitter: low / mid / high, each a 4th-order section = cascade of biquads]
        low  -> [low-band compressor]  (same as COMP1)
        mid  ------------------------>
        high -> [high-band compressor] (same as COMP1)
   -> [summing module: out = w0*low + w1*mid + w2*high] -> [second gain element] -> out
```

Patent text for the order (¶[0056], FIG. 4 method): "high pass filter... first low shelf filter... first high shelf filter... first compressor... second low shelf filter... second high shelf filter... first gain element... peak/dip module... split into a low band signal, a mid band signal, and a high band signal... low band compressor... high band compressor... summing module... second gain element".

## 4.2 HPF

Verbatim ¶[0030]: "The high pass filter 111 is configured to pass through high frequencies of an audio signal... while attenuating lower frequencies, based on a predetermined frequency... In at least one embodiment, ultra-low frequency content is removed from the input audio signal, where the predetermined frequency may be selected from a range between 300 Hz and 3 kHz. The predetermined frequency however, may vary depending on the source signal, and vary in other embodiments to comprise any frequency selected from the full audible range of frequencies between 20 Hz to 20 kHz. The predetermined frequency may be tunable by a user, or alternatively be statically set."

Verbatim ¶[0040]: "the high pass filter may comprise a fourth-order filter realized as the cascade of two second-order biquad sections. The reason for using a fourth order filter broken into two second order sections is that it allows the filter to retain numerical precision in the presence of finite word length effects, which can happen in both fixed and floating point implementations."

Verbatim difference equations (¶[0040]-[0043]):

> Two memory locations are allocated, designated as d(k−1) and d(k−2)... For each input sample x(k), a quantity d(k) is calculated using the coefficients a1 and a2:
> `d(k)=x(k)−a1*d(k−1)−a2*d(k−2)`
> The output y(k) is then computed, based on coefficients b0, b1, and b2, according to:
> `y(k)=b0*d(k)+b1*d(k−1)+b2*d(k−2)`
> The above computation comprising five multiplies and four adds is appropriate for a single channel of second-order biquad section... a single channel of fourth order input high pass filter would require ten multiples, four memory locations, and eight adds.

Claim support: 10,069,471 claim 9 "said high pass filter comprises a fourth order high pass filter".

**Unspecified:** the corner frequency actually used (only the range "300 Hz and 3 kHz", which is odd for "ultra-low", and "20 Hz to 20 kHz"); the prototype (Butterworth / Linkwitz-Riley / Bessel); Q of each section; coefficient formulas. Note `d(k)=x(k)-d(k-1)-a2d(k-2)` in 10,848,867's copy of the text drops `a1`; treat as a typo.
**Inference:** a 4th-order Butterworth HPF (two sections with Q = 0.5412 and 1.3066) is the conventional reading of "4th order, two biquads". Any corner in 20-300 Hz is consistent with "remove ultra-low frequency content".

## 4.3 First and second filter modules (shelf pairs)

Verbatim ¶[0031]: "frequencies below a first frequency may be adjusted by ±X dB, while frequencies above a first frequency may be adjusted by ±Y dB... The first low shelf filter 112 and first high shelf filter 113 may both be second-order filters. In at least one embodiment, the first low shelf filter 112 attenuates content below a first frequency, and the first high shelf filter 112 boosts content above a first frequency. In other embodiments, the frequency used for the first low shelf filter 112 and first high shelf filter 112 may comprise two different frequencies. The frequencies may be static or adjustable. Similarly, the gain adjustment (boost or attenuation) may be static or adjustable."

Purpose, ¶[0044]: "the first filter module boosts the content above a first frequency by a certain amount, and attenuates the content below a first frequency by a certain amount, before presenting the signal to a compressor or dynamic range controller. This allows the dynamic range controller to trigger and adjust higher frequency material, whereas it is relatively insensitive to lower frequency material."

Inverse, ¶[0012]/[0050]: "if the first filter module boosted content above a first frequency by +X dB and attenuated content below a first frequency by −Y dB, the second filter module may then attenuate the content above the first frequency by −X dB, and boost the content below the first frequency by +Y dB. In other words, the purpose of the second filter module in one embodiment may be to 'undo' the gain adjustment that was applied by the first filter module." Also ¶[0033]: second module "may use the same frequency, for instance the first frequency".

Numbers appearing elsewhere in the same family (claims / 10,701,505), see 4.10:
- 10,701,505 ¶[0064]: second low shelf "between 100 Hz and 3000 Hz, with an attenuation of between −5 dB to −20 dB"; second high shelf "between 100 Hz and 3000 Hz, with a boost of between +5 dB to +20 dB".
- Recurring "24 dB differential between high and low frequencies" (see sections 5, 6, 7, 9): for the pre-transmission shelves of block B (9344828 ¶[0227]), block D's detector, and block F's first shelves. **Figure-only:** the stereo patent's FIG. 5 labels its detector shelves "low shelf −12dB" and "high shelf +12dB" (= 24 dB differential).

**Unspecified in A:** the corner frequency of LS1/HS1 and LS2/HS2 (the 1000 Hz of the core's claims is not restated in A's description), the gains, the shelf prototype (the core text in 9344828 ¶[0080] says "all of the shelving filters in the present invention are first-order shelving filters", yet block A's text says the shelves "may both be second-order filters"), and whether LS2/HS2 are the magnitude-complementary realization of the core.
**Internal inconsistency (flag for engineers):** 4.3's "second filter module ... undo" (¶[0012], [0033], [0050], [0065]) says LS2 boosts and HS2 cuts if module 1 cuts lows and boosts highs. But 10,701,505 ¶[0064] and 10,069,471 claim 1 give LS2 as −5…−20 dB (cut) and HS2 as +5…+20 dB (boost), the same direction as the first module. Treat the signs in ¶[0064] / the claims as unreliable; use "inverse of module 1" as the design rule.

## 4.4 Compressor (COMP1)

See section 3. Placement matters: it sees a signal with highs boosted and lows cut, so "the first compressor may, in at least one embodiment, be configured to trigger and adjust the higher frequency material, while remaining relatively insensitive to lower frequency material" (¶[0011]).

## 4.5 First processing module: gain element + peak/dip bank

Verbatim ¶[0034]: "The first gain element 117... may be configured to adjust the level of a signal by a static amount... The peak/dip module 118 is configured to shape the desired output spectrum, such as to increase or decrease overshoots or undershoots in the signal. In some embodiments, the peak/dip module may further be configured to adjust the slope of a signal, for instance for a gradual scope that gives a smoother response, or alternatively provide for a steeper slope for more sudden sounds. In at least one embodiment, the peak/dip module 118 comprises a bank of ten cascaded peak/dipping filters. The bank of ten cascaded peaking/dipping filters may further be second-order filters. In at least one embodiment, the peak/dip module 118 may comprise an equalizer, such as parametric or graphic equalizers."

Why the gain: ¶[0051] "This adjustment, for instance, may be necessary because the peak-to-average ratio was modified by the first compressor... In at least one embodiment, the first processing module comprises only the peak/dip module. In other embodiments, the first processing module comprises a gain element followed by a peak/dip module."

**Unspecified:** all ten center frequencies, Qs, gains, whether fixed or user-adjustable, the gain-element value. **Inference:** this is the same bell-filter structure as the core graphic EQ (core claims: eleven cascading second-order bell filters, 30 Hz first, 16000 Hz last, about one-octave spacing; 9350309 claims 10-13) but with ten bands instead of eleven; nothing says which band is dropped or that the centers match.

## 4.6 Band splitter

Verbatim ¶[0035]: "the signal is split into a low band signal 220, a mid band signal 221, and a high band signal 222. Each band may be the output of a fourth order section, which may be further realized as the cascade of second order biquad filters... The low, mid, and high bands may be predetermined ranges, or may be dynamically determined based on the frequency itself, i.e. a signal may be split into three even frequency bands, or by percentage. The different bands may further be defined or configured by a user and/or control mechanism."
¶[0052]: "the band splitter comprises a fourth-order band-splitting bank. In this embodiment, each of the low band, mid band, and high band is yielded as the output of a fourth-order section, realized as the cascade of second-order biquad filters."

**Unspecified:** both crossover frequencies; the filter alignment (whether the three outputs sum flat); whether mid is a band-pass or "everything minus low minus high". **Inference:** a 4th-order low-pass, a 4th-order high-pass and a 4th-order band-pass pair (Linkwitz-Riley-like) at two crossovers; since the patent sums the bands with free weights `w0..w2`, flat summation is not required to be exact.

## 4.7 Per-band compressors and sum

Mid band bypasses compression (FIG. 1A drawing shows 221 going straight to the summing module). Sum, ¶[0054]: "an output of the summing module may be calculated by: `out=w0*low+w1*mid+w2*high`. The coefficients w0, w1, and w2 represent different gain adjustments. The second processing module may further comprise a second gain element... may provide a final gain adjustment." The summing module "comprises individual gain controls for each of the incoming signals" (¶[0037]).

**Unspecified:** the weights, the per-band compressor settings, the final gain.

## 4.8 Per-patent differences inside block A

- **9,195,433 / 9,348,904 / 9,397,629:** same chain and same description. 9,348,904 claim 1 recites the shelves explicitly (first low shelf, first high shelf, compressor, second low shelf, second high shelf...). 9,397,629 claim 1 says "first filter module" / "second filter module" and puts the shelves in dependent claims 2-3. 9,195,433's own claims are the cable + core chain (claim 1: LS1, HS1, compressor, LS2, HS2, graphic equalizer, second compressor), not the 3-band chain.
- **In-line cable packaging (9,195,433 ¶[0058]-[0065]):** input connector, "audio enhancement module", output connector; "profile module 604 may be used to select from one or more predetermined audio enhancement scheme(s) and/or adjust the parameters thereof. These parameters may include but are not limited to frequency cutoffs, gain adjustments, filter adjustments, adjusting the equalizing coefficients of a graphic equalizer". With video present: "the audio signal 711 is processed by the audio processing module 620, whereas the video or other signal 712 is delayed via a delay module to keep the signals in sync." No delay value is given; **Inference:** it equals the algorithmic latency of the audio path.

## 4.9 Variants in 9,906,858 (and 10,917,722, near-identical text)

New text (¶[0009]-[0017], [0026]-[0036], [0043]):
- "the input audio signal is processed with a first gain element. In at least one embodiment the first gain signal is then filtered with the high pass filter."
- "the high pass signal from the high pass filter is then split into a first high pass signal and a second high pass signal." (splitter 140; signals 210 and 208). ¶[0012]: "the second high pass signal is then filtered through a first filter module to create a first filtered signal. In at least one embodiment, the first high pass signal is then modulated with a first compressor to create a modulated signal."
- First filter module may be "only a first low shelf filter 112" (FIG. 2D) or "only a first high shelf filter 113" (FIG. 2C); second filter module "may comprise only a high shelf filer 116".
- Band split is "at least low band signal and a high band signal, and preferably also a mid band signal" (mid optional).
- Output stage: third gain element 133 after the summing module 132 (FIG. 2A/2D).
- First processing module may take the compressor output directly: ¶[0014] "the modulated signal from the first compressor is then processed with a first processing module".

**Figure-only (FIG. 1B "Configuration #2/#3", sheet 2):** splitter 140 sends 211 two ways. One copy (208) goes through the first filter module 301, whose output 401 enters the first compressor 114. The other copy (210, unfiltered) also enters compressor 114. The compressor output (214) goes to the first processing module 303, **skipping the second filter module**. **Inference:** this is a sidechain arrangement. The shelf-filtered copy is the detector input and the unfiltered copy is what is gain-reduced, which is why no inverse shelf pair is needed. The text does not say "sidechain"; the claims of 9,906,858 and 10,917,722 do not claim it explicitly.
**Figure-only (FIG. 2D "Configuration #4"):** low shelf 112 only in module 1, high shelf 116 only in module 2.

## 4.10 Claim-only numeric values (10,069,471, 10,848,867, 11,425,499)

Verbatim claims; the description of these three patents has no new numbers (checked by diff):
- 10,069,471 claim 1: "a second low shelf filter configured to between 100 Hz to 3000 Hz and −5 dB to −20 dB... a second high shelf filter configured to between 100 Hz to 3000 Hz and +5 dB to +20 dB". Claim 2: "a 24 dB differential between high and low frequencies for the second filtered signal". Claim 3: "second low shelf filter is set to 1000 Hz and −12 dB and said second high shelf filter is set to the 1000 Hz and −24 dB". Claim 4: "said first low shelf filter is set to 500 Hz and −24 dB and said second high shelf filter is set to the 200 Hz and −24 dB". Claims 9-13: 4th-order HPF; first and second LS/HS are second-order.
- 11,425,499 claim 1: "a second low shelf filter set to 1000 Hz and −24 dB... and a second high shelf filter set to 1000 Hz and −24 dB". Claims 3-4 repeat the 24 dB differential and the 500 Hz / 200 Hz / −24 dB items.
- 10,848,867 repeats the 10,069,471 numbers (claims 1, 3-5).

These claim numbers are mutually inconsistent (for example claim 3's -12 / -24 dB cannot give a 24 dB differential if both are cuts, and "first low shelf 500 Hz / −24 dB" vs "second high shelf 200 Hz / −24 dB" pairs a low shelf with a high shelf at a lower corner). Quote them, do not trust them as a preset.

## 4.11 What block A leaves open (summary)

HPF corner and alignment; LS/HS corners and gains for both modules; the table behind `gainReduction`; every compressor's attack, release, threshold and ratio; the ten peak/dip centers, Qs and gains; the two crossover frequencies and alignment; w0..w2; the three gain-element values; stereo handling (¶[0029] only says the input "may comprise a stereo audio signal"; compressor linking is not specified).

---

# 5. Block B: broadcast / transmission split (pre- and post-transmission processing)

**Patents:** 9,344,828 (¶[0034]-[0036], [0221]-[0232], FIG. 16, claims 1-14), 9,276,542 (same text, ¶[0224]-[0235]; claims 1-36), 9,413,321 claim 9 (adds "material profile" to the post side).

Stage order (¶[0231]-[0232] of 9344828, FIG. 16 + FIG. 1 labels 101-109):

```
PRE  (at transmitter)   : input gain adjust (101) -> low shelf (102) -> high shelf (103) -> compressor (104)
                          -> [data compression: "lossy CBR, ABR, VBR, Mp3, or FLAC"] -> transmitter 1612
POST (at receiver)      : receiver 1618 (+ decode) -> low shelf (105) -> high shelf (106) -> graphic EQ (107)
                          -> compressor (108) -> output gain adjust (109)
```

Verbatim parameters / rules:
- ¶[0224]: "the filters of the post-transmission processing module include frequency values that are matched or substantially equal to the frequency values of the pre-transmission processing module, but whose gain value are inverse."
- ¶[0227]: "the shelving filters of the pre-transmission processing module 1610 of at least one embodiment may be structured and configured to create an approximate 24 dB differential between high and low frequencies. The dynamic range modification component of the pre-transmission processing module 1610 may then be configured to process the signal from the shelving filters and decrease the dynamic range thereof by compression, limiting or clipping."
- ¶[0225]: dynamic range component "may include a compressor, and in other embodiments, may include a gain amplifier."
- ¶[0226]: post module "configured to increase the dynamic range of the partially processed signal"; "the gain values of the post-transmission processing module 1620 filters may be adjusted, for example, to compensate for lost bandwidth due to lower data rates during transmission. The gain values may be pre-programmed or pre-designated so maximum audio quality is maintained at all data rates."
- ¶[0036]: "The final frequency response of the system may be substantially flat, even during heavy automatic gain control on the encoding (pre-transmission) side... the post-transmission processing module may be configured to equalize the signal in an aggressive manner to compensate for deficiencies in the playback system or environment".
- Claim 1 / 8 (9,344,828): "at least two shelving filters configured to create a 24 dB differential between high and low frequencies"; claim 7 "frequency values which are equal"; claim 12 "approximately equal". 9,276,542 claim 17: same 24 dB; claim 18 "inverse gain values".

**Unspecified:** shelf corner frequencies and per-shelf gains (the 1000 Hz / 11-band numbers are core-claim values, not restated in FIG. 16), the codec bitrate-to-gain mapping, any makeup after the codec, how the post stage knows the pre stage settings (no side-channel signalled).
**Inference:** the "24 dB differential" is most plausibly ±12 dB shelf pair (see the stereo FIG. 5 labels), with the post side exactly mirroring it. This block only matters if the Rust engine is split across a transmit/receive boundary.

---

# 6. Block C: output gain reduction above a threshold (headphone claims)

**Patents:** 9,350,309 claims 1, 16, 30; 9,793,872 claim 1 (same).

Claim 1 (9,350,309): "adjusting the gain of the second compressed signal a second time... wherein the adjustment comprises a gain reduction if the gain of the second compressed signal is above a predetermined threshold". Claim 16 gives it to the second compressor ("additionally configured to apply a gain reduction if the gain of the second filtered signal is above a predetermined threshold"); claim 30: "reducing the gain of the second compressed signal, if the second compressed signal is measured to be above a predetermined threshold". Claims also fix: first LS and HS "cutoff frequency at 1000 Hz" (claims 8-9), graphic EQ "eleven cascading second order filters"... "first of the eleven... 30 Hz... eleventh... 16000 Hz... second to tenth... approximately one octave intervals" (claims 10-13), "second low shelf filter is a magnitude-complementary low-shelf filter" (claim 14), second HS/LS "mirror image" of the first (claims 40-41). These are core-chain values, not new.

**Unspecified:** I searched the description of 9,350,309 for this behavior and found none; it is claim language only. No threshold value, no knee, no attack/release.
**Inference:** a final limiter on the output gain stage. Block G is the one patent that describes a ceiling in detail.

---

# 7. Block D: stereo field enhancement (dynamic mid/side width)

**Patents:** 9,398,394 (all paragraph IDs below), continuations 9,883,318, 10,412,533, 10,999,695 (same numbers; renumbered paragraphs, claims reworded, see 7.5).

## 7.1 Stage order (FIG. 1, FIG. 3, FIG. 5; FIG. 2 / FIG. 4 add the delay)

```
L,R in (200)
 -> [first filter 101: crossover]  -> LOW (202)  ------------------------------------------------+
 -> HIGHER (201) -> [M/S splitter 102]                                                           |
        MID  204 = R + L                                                                         |
        SIDE 203 = R + (inverse of L)  = R - L                                                   |
     MID 204 -> [detection module 103: low shelf + high shelf, 24 dB differential] -> DETECT 206 |
     SIDE 203 -> [compression module 104 = AGC, gain driven by DETECT 206,                      |
                    gain reduction capped by an adjustable ceiling] -> + makeup gain -> 207      |
 -> [processing module 105: mixer] combines MID (204, or directly from 201), 207, and LOW 202 ----+--> 208 out
 optional (FIG. 2 / 4):
 -> mix(MID, 207) = HIGHER' (250) -> [second filter 150: crossover] -> HF (251) -> [delay module 151]  -> 253
                                                                      -> MID-FREQ (252) --------------+
 -> [combination module 152] sums LOW 202 + MID-FREQ 252 + delayed HF 253 -> 208
```

## 7.2 Verbatim parameters

| Parameter | Value | Where |
|---|---|---|
| First cutoff (low / higher split) | "generally between 20 Hz and 1000 Hz", adjustable or static | ¶[0025], [0036]; claims 9, 18, 27 |
| Second cutoff (HF split for delay) | "generally between 1 kHz and 20 kHz", adjustable or static | ¶[0031], [0047]; claims 8, 28 |
| Delay | "between 1 and 999 samples"; left, right or both channels; "may comprise comb filters" | ¶[0015], [0032], [0048], [0056]; claims 7, 29 |
| Detection filters | "a low shelf filter and a high shelf filter structured to create a 24 dB differential between high and low frequencies within the middle signal" | ¶[0038], [0054]; claims 13, 22, 33 |
| Gain reduction ceiling | "generally between 0 dB and 12 dB", adjustable or static | ¶[0027], [0039], [0054]; claims 15, 23, 34 |
| Makeup gain | tied to the ceiling: "if a 8 dB of side boost is set and configured, then the compression module 104 will never decrease the gain of the side signal 203 more than 8 dB" | ¶[0040] |

Other verbatim:
- M/S, ¶[0026]: "the middle signal comprises the sum of the right channel signal and the left channel signal. In contrast, the side signal comprises the sum of the right channel signal and the inverse of the left channel signal, or in other words the right channel signal subtracting the left channel signal." (¶[0037]: "sum and difference circuit".) Claims 11-12 / 20-21 repeat it; 10,412,533 claim 13 later says "difference between the right channel signal and the left channel signal".
- Why a crossover: ¶[0011] "This allows phase relationships of the low frequency signal to be maintained... low frequencies do not need to be adjusted for stereo-width as it makes sense to share the load of reproducing them through both speakers equally."
- Behavior, ¶[0012]: "The middle signal is processed and used as a detection signal in order to dynamically modulate the side signal... the modified middle signal or detection signal determines how strongly the side signal is modulated."
- AGC controls, ¶[0054]: "threshold, ratio, attack and release", "soft and hard knees", and "a makeup gain is added to the gain-modulated side signal 207 within the AGC". Ceiling "may vary from 0 dB to 12 dB".
- Recombination, ¶[0040]/[0055]: low signal 202, middle signal (from 204 "or information directly from signal 201", FIG. 5), and gain-modulated side 207 "combined to form a final output signal"; "Regardless of the different circuit pathways, the processing module 105 is structured to recombine signal or information that was earlier split by the first filter 101 and the M/S splitter 102."
- Delay purpose, ¶[0048]: "create a comb filtering effect and acoustic phase decorrelation, which may be effective in creating a more immersive stereo field".
- **Figure-only (FIG. 5):** detection module drawn as "low shelf −12dB" and "high shelf +12dB" feeding the AGC; M/S splitter drawn with legs "A − B" and "A + B"; crossovers drawn as HP/LP diamonds; delay drawn as separate "left delay" and "right delay" boxes; mixer 105' takes the unsplit signal 201 and the AGC output 207.

## 7.3 Unspecified

Which shelf corner frequencies the detector uses; AGC threshold, ratio, attack, release, knee; the makeup/ceiling dB the product ships with (only the 8 dB example); the L/R decode after processing (the patent says "combine" but never writes L = (M+S)/2 or the equivalent, and uses unscaled sum/difference); whether the "middle" recombined is the raw one or the filtered detection copy (¶[0041] says both are possible; FIG. 5 draws the raw 201); crossover alignment (it says "frequency bypass crossover" and "audio crossover" only); which channel is delayed and by how many samples; whether the delay is applied above 1-20 kHz only (yes, per ¶[0031]) and at what sign.

## 7.4 Inference (not in the patents)

- Because the makeup gain equals the reduction ceiling and the AGC detector sees a high-tilted copy of the mid signal, the side channel sits at roughly +G dB while that detector is below threshold and falls back to roughly 0 dB when loud (tilted) mid content drives full gain reduction. That reproduces the stated goals "equal to or above the original side signal" (¶[0040]) and "consistent immersive experience" (¶[0009]). This is my reading of how the pieces fit, not a stated formula.
- With the patent's unscaled encode M = L + R, S = R − L, the exact inverse is R = (M + S')/2, L = (M − S')/2 (S' = the gain-modulated, made-up side). The patent never writes a decode, so the 1/2 scaling and the sign convention are my choice.

## 7.5 Continuations

9,883,318 / 10,412,533 / 10,999,695 add no new numbers (checked by diffing every number token in description and claims). Claims drift: 9,883,318 claim 1 ends "adjusting the gain-modulated side signal to a point at least equal to the side signal"; 10,412,533 claim 1 is generic ("obtaining a low frequency signal and a higher frequency signal... generating a detection signal at least partially with reference to the middle signal"); 10,999,695 claim 1 makes the detection signal depend "on the middle signal and the side signal" (the description still only drives it from the middle).

---

# 8. Block E: narrow-bandwidth processing (voice, auscultation, hydrophone)

**Patents:** 9,264,004 (all IDs below) and continuation 9,741,355 (same disclosure; claim 1 drops the HPF from the independent claim; no new numbers).

## 8.1 Stage order (FIG. 1)

```
in -> [HPF 111: resonant, "first frequency" f1] -> [first compressor 112] -> ([clipping module 113], optional)
   -> partially processed 213/205 -> (optional transmitter/network/receiver for FIG. 2/4/6/8)
   -> [splitter 121: two identical copies]
        copy 1 (221) -> [resonant LPF 122, "second frequency" f2 < f1]  -> 222 \
        copy 2 (221') -> [pass-through module 123: static gain, may be zero]  -> 223 /
   -> [mixer 124] -> 224 -> ([tone control 125: parametric EQ of 1, 2, 3 or 4 bands, or graphic EQ])
   -> ([second compressor 126, "similar in structure and/or configuration" to the first]) -> out 226
```

FIG. 3 is the reduced form (HPF, compressor, splitter, LPF, pass-through, mixer, out); FIG. 2 / 4 split it into pre-transmission (HPF, compressor, clipper) and post-transmission (splitter onward).

## 8.2 Verbatim parameters

| Parameter | Value | Where |
|---|---|---|
| HPF first frequency (voice) | "range between 2.5 kHz and 3.2 kHz"; "resonant high pass filter"; tunable or static | ¶[0011], [0027], [0044]; claims 6, 16, 25 |
| LPF second frequency (voice) | "range of 700 Hz to 1000 Hz"; resonant LPF; "tunable or statically set"; "counterintuitive to expected function, the second frequency is set to be lower than the first frequency" | ¶[0009], [0011], [0032], [0047]; claims 9-10, 18-19, 27-28 |
| Parametric EQ | "one, two, three, or four bands" | ¶[0036], [0049] |
| Pass-through gain | "a static adjustment"; "may also be zero"; "in voice applications only a small amount of the signal is sufficient... the gain of the second signal 221′ will be adjusted downward"; restores "sibilance" | ¶[0033], [0048] |
| Input bandwidth | "will in certain applications comprise less than half of the 20 Hz to 20 kHz audible range" | ¶[0026] |

Clipper (¶[0030]): "configured to limit or remove transients from the modulated signal 212... Transients may be generated if, for instance, the first compressor used slower attack times"; "may comprise series clippers, shunt clippers"; "soft clipping... to avoid sharp points in the transfer characteristic".
LPF purpose (¶[0009], [0032]): "This allows the resultant signal to be used to rebuild usable dynamics from the low frequencies."
Second compressor (¶[0037]): "dynamic range compression or static compression or level adjustment".

## 8.3 Unspecified

Resonant filter Q / order / resonance gain; both compressors' attack, release, threshold, ratio; clipper threshold and knee; pass-through gain value; tone-control band frequencies, Qs and gains; non-voice (hydrophone, auscultation) corner values ("appropriate frequency ranges", ¶[0012], none given).

## 8.4 Inference

With f1 (2.5-3.2 kHz) above f2 (700-1000 Hz) the LPF branch only sees whatever the compressor and clipper leave below f1 (the resonant HPF has finite skirts, plus intermodulation and clipping products). That matches the patent's claim that the LPF "rebuild[s] usable dynamics from the low frequencies", i.e. a nonlinear low-band regeneration path, with the pass-through branch carrying the sibilance/high band. Treat this reading as unverified; the patent never calls it distortion or difference-tone generation.

---

# 9. Block F: deep-diving (helium speech) processing

**Patent:** 9,564,146 (FIG. 2-5, claims 1-32).

## 9.1 Stage order (FIG. 2; FIG. 3 adds EQ; FIG. 4 adds static gain)

```
mic -> [pitch change module 20] -> [first low shelf 30] -> [first high shelf 40]
    -> [automatic gain control module 50: compressor and/or limiter] -> [second low shelf 60] -> [second high shelf 70]
    -> ([equalization module 80: bell filter(s)]) -> ([static gain control module 90]) -> output / transmitter
```
¶[0033]: "the order is not to be interpreted as limiting".

## 9.2 Verbatim parameters

| Parameter | Value | Where |
|---|---|---|
| Pitch change | "within the range of an increase of one octave to a decrease of two octaves, inclusively" / "−2 to +1 octaves" | ¶[0034], [0049]; claims 2, 18 |
| First shelf pair | "a differential of 24 decibels between its high and low frequencies"; "first center frequency and a first filter gain value" | ¶[0037]-[0038]; claims 3, 19 |
| Second shelf pair | same center frequency as the first, gain "the inverse of one another" | ¶[0016], [0042]; claims 1, 17 |
| AGC | "adjust the gain of the filtered signal by an amount within the range of 0 decibels and 20 decibels, inclusively"; "compressor and/or a limiter" | ¶[0039]; claims 4-6, 20-22 |
| Equalization | "adjust the gain of the signal by an amount within a range of 0 to 25 decibels inclusively"; "equalize frequencies of the signal below 200 Hertz"; "at least one bell filter" | ¶[0044]; claims 9-11, 25-27 |
| EQ selection inputs | "the diver's gender, the diver's age, the tonality of the diver's voice, a depth at which the diver is diving, and the type of gas the diver is breathing" | ¶[0045]; claims 12-13, 28-29 |
| Static gain | adjusts "signal-to-noise ratio... and/or... a desired headroom" | ¶[0047] |
| Audio band | "approximately 20 Hz to 20 kHz" | ¶[0031] |

## 9.3 Unspecified

The pitch-shifting algorithm (no PSOLA / phase vocoder / granular method named; no latency; formant handling), the shelf center frequency and individual shelf gains, the AGC threshold/ratio/attack/release and the meaning of "adjust the gain by 0-20 dB" (a fixed boost? a maximum gain reduction? a make-up range?), EQ center frequency / Q / table mapping diver parameters (gender, age, depth, gas) to settings.
**Inference:** "0 to 20 dB" most plausibly bounds AGC gain-adjust range; "equalize below 200 Hz, 0-25 dB" suggests restoring chest resonance after pitch-down. Neither is stated.

---

# 10. Block G: "safe listening" ceiling (profile value minus safety parameter)

**Patent:** 9,281,794 (claims 1-21; ¶[0009]-[0012], [0033], [0226]-[0230]).

Verbatim:
- ¶[0033]: "adjusting the gain of the received signal based upon a profile or coefficient value minus a safety parameter, passing the signal through a first compressor that sets a peak signal threshold based upon the profile minus the safety parameter, and passing the process signal through a second compressor that also sets a peak threshold based upon the profile minus the safety parameter."
- ¶[0227]: "a safety setting, in at least one embodiment, may be toggled 'on' and 'off' as desired, although in other embodiments, it may be set permanently 'on' or 'off' via the software."
- ¶[0229]: "the peak signal threshold... may be set (via a profile value minus the safety threshold) at the first input gain controller, at the first AGC or compressor, and at the second AGC or compressor. Working in band-split mode, the peak signal threshold may be set at the first input gain controller, at the first AGC or compressor, and then at the low and high AGC or compressors before the output... This ceiling cannot be adjusted by the hardware volume control."
- ¶[0230]: "In one embodiment, the peak signal threshold is set or adjusted only at the end of the process, e.g., via the second compressor and/or via the low and high AGC/compressor... the safety parameter may but need not be between 20 and 30 dB, e.g., 23 dB."
- ¶[0228]: "after the graphic equalizer (107), in one embodiment wherein the system and method implement a band-split made [sic], the low frequencies of the signal are processed via a low AGC or compressor, the high frequencies of the signal are processed via a high AGC or compressor, the mid-range frequencies pass though".
- Claim 1: "a second compressor that sets a peak signal threshold, the peak signal threshold being defined by a profile value minus a safety parameter".

**Unspecified:** what "profile value" is numerically (the profile holds coefficients per device/environment, not given); the dB reference for "profile value"; compressor ratio at the ceiling (hard limit vs. soft); how input gain is lowered (same dB as the safety parameter?).
**Inference:** in an engine, this is a global offset subtracted from the input-gain and compressor thresholds when a "safe" flag is on, with 23 dB as the only example.

---

# 11. Block H: material-profile EQ for non-loudspeaker transducers

**Patents:** 9,413,321 (¶[0044], [0247]-[0251], claims 1-22) plus continuations 10,158,337, 10,666,216, 10,848,118, 11,431,312 (no new description beyond boilerplate).

Verbatim:
- ¶[0044]: "the output signal of the present invention may be sent to a transducer for vibrating materials, e.g. surfaces including but not limited to appliances, walls, furniture, windows. The transducer contemplated is not a loudspeaker. Select frequencies of the signal will be gain adjusted based at least in part on a material profile. The material profile may comprise the material type, area, density, rigidity, and any other characteristics determinative of its acoustic resonance."
- ¶[0249]: "a dense and non-rigid material such as certain plastics may be able to reproduce low frequencies well, but are resistant to vibrating at higher frequencies. Accordingly, a large increase in high frequency power is required... glass is a dense and rigid material which tends to reproduce higher frequency sounds better than lower frequency sounds, and would therefore require increases in the amount of low frequency gain".
- ¶[0250]: "Each material profile may comprise a plurality of filter equalizing coefficients used to configure the filters of the graphic equalizer."

**Unspecified:** every number. No example profiles, no coefficient table. This is "load a graphic-EQ preset keyed by material", not a DSP algorithm.

---

# 12. Non-DSP or already-core items found

- 9,615,189 and 10,701,505: HRTF generator (tragus/antihelix structures, microphone, preamp). The only DSP in 10,701,505 is block A (¶[0059]-[0071], including the ¶[0064] second-shelf ranges). 9,615,189's claims contain no DSP chain.
- 9,621,994 / 9,906,867 / 9,998,832 (surface acoustic transducer) and 9,615,813 / 10,639,000 / 10,820,883 (auscultation devices): hardware; seen only as titles in Google's cited-by tables, pages not read. 11,211,043 (hearing protection; claims read): hardware ("reduce ambient noise levels by at least 50 decibels", microphone manifolds, "a minimum resolution of 20 degrees"); its DSP is only named, not specified.
- 8,705,765 (ringtone): a profile of "filter equalizing coefficients" loaded into the core LS / HS / compressor chain. Core-era; no new block.
- 9,413,321 / 9,350,309 / 9,281,794: the long core text and C listing (9344828 ¶[0247]-[0317]) are the core researcher's scope.

---

# 13. Summary table: blocks, audible effect, effort, spec completeness

Effort: S = a day or less on top of the core primitives; M = days; L = a week or more or requires design decisions the patents do not make. "Spec" = how much of the numbers and topology the patent text actually supplies.

| Block | Patent(s) | What it does audibly | Effort | Spec completeness |
|---|---|---|---|---|
| A. HPF + 3-band multiband back-end (HPF, compressor, 10-band peak/dip, 3-band split, per-band compressors, weighted sum) | 9195433, 9348904, 9397629, 9906858 (+ variants) | Cleans sub-bass, adds bass/treble dynamic control, "clarity" in highs; the shelf-bracketed compressor pumps less on bass | M-L | **Topology complete; parameters absent.** Missing HPF/shelf corners, all compressor times, the 10 EQ bands, crossovers, weights. Only claim-level ranges (shelf 100-3000 Hz, ±5-20 dB, sign conflicts) |
| A'. Sidechain / 2-band variants | 9906858, 10917722 | Same, with compression detected on the shelf-tilted signal but applied to the raw signal; no inverse shelves needed | S on top of A | Topology only from drawing FIG. 1B; text ambiguous |
| B. Pre/post-transmission split | 9344828, 9276542 | Encoder-side tilt + compression survives lossy coding; decoder undoes tilt, boosts, EQs | S on top of core (a split, not new DSP) | Topology complete; shelf values only "24 dB differential"; matched frequencies, inverse gains |
| C. Output limiter above threshold | 9350309, 9793872 | Peak control at the output | S | Claim text only; no values |
| D. Stereo field enhancement | 9398394 family | Wider, more consistent stereo image on small speakers; side lifts when mid is quiet | M | **Best-specified new block:** all ranges (20-1000 Hz, 1-20 kHz, 1-999 samples, 0-12 dB, 24 dB), FIG. 5 ±12 dB; missing AGC times, decode scaling |
| E. Narrow-bandwidth chain | 9264004, 9741355 | Intelligible voice/body-sound output from poor sources: HF emphasis, low-band regeneration, sibilance restored | M | Topology complete; voice ranges (2.5-3.2 kHz, 700-1000 Hz, EQ 1-4 bands); missing resonance, times, clip level |
| F. Deep-diving voice | 9564146 | Lowers helium pitch and clarifies voice | L (pitch shifter design is open) | Topology and ranges (−2 to +1 oct, 24 dB, 0-20 dB, 0-25 dB below 200 Hz); algorithm absent |
| G. Safe-listening ceiling | 9281794 | Hard volume ceiling without muffling low-volume audio | S | Rule given (profile value − safety parameter, 20-30 dB, e.g. 23 dB); profile values absent |
| H. Material-profile EQ | 9413321 family | Frequency-compensated drive for vibrating panels | S (preset lookup) | No numbers at all |
| I. In-line cable / profile module | 9195433 | Delivery vehicle: connectors, profile select, A/V delay | S | Block-level only; no numbers (video delay unspecified) |

## 14. Suggested inclusion order (inference, for the engineers deciding scope)

1. **D (stereo)**: self-contained, fully ranged, high audible payoff; implement with the 8 dB example and 24 dB detector as defaults, make AGC timings tunable.
2. **A as a skeleton** (HPF, 3-band split, per-band compressor reusing the core compressor, weighted sum) plus the 10-band peak/dip as the core graphic EQ minus one band; set parameters by ear since no values exist. Include the 9906858 sidechain variant only if the plan keeps the compressor/EQ split.
3. **G (safety ceiling)** and **C (output limiter)**: small and complementary; one threshold offset and one limiter.
4. **E (narrow bandwidth)**: include only if voice/telecom is a target.
5. **B, F, H**: skip unless the product needs a transmit/receive split, diver voice, or vibration transducers.

## 15. Open data that no patent supplies (do not invent; needs listening / reverse engineering of a product)

HPF corner and alignment; LS/HS corners and signs for both shelf modules (conflict between description and claims noted in 4.3); all attack / release / threshold / ratio values; `gainReduction` table; the 10 peak/dip centers/Qs/gains; the two band crossovers and w0..w2; stereo AGC times and decode scaling; narrow-band resonance and clip level; pitch-shift algorithm.
