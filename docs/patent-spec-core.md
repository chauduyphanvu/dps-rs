# Bongiovi "Digital Power Station" (DPS) core signal chain: spec derived from the public patents

Purpose: let engineers reimplement the chain in Rust from the patents alone. No source code is public; the patents are the only source of truth.

Scope note: the user-facing name "DPS" never appears in these patents. I am treating the nine-stage "digital signal processing method 100" of US 8,284,955 / US 8,229,136 as the core chain, with US 7,254,243 / US 8,472,642 as the analog ancestor and US 8,462,963 as a sibling embodiment. That mapping is my inference.

---

## 0. How to read this document

### 0.1 Content tags (used everywhere)

| Tag | Meaning |
|---|---|
| **[V]** | Verbatim patent content (text, equation, code) or a number stated in the patent. Equations are linearised to plain text but kept symbol-for-symbol. |
| **[N]** | My own numerical check: I simulated the patent's formulas/code in Python (no numpy; impulse responses and DTFT) and report what they do. |
| **[I]** | My inference or derivation from patent text. Not patent text. |
| **[R]** | My recommendation of a conventional choice where the patent is silent or inconsistent. Not patent text. |

### 0.2 Citation convention

- **"col. N"** = column of the printed patent, taken from the patent PDF (the "Download PDF" link on the Google Patents page; text layer for columns, page images for equations). Line numbers are given only as approximate ranges ("ll.") where I read them off the page-image margin ticks (+/- 3 lines).
- **"[GP ¶n]"** = the n-th description paragraph in Google Patents' machine text, counted by me from the start of the description. Google's text for granted patents carries no official paragraph numbers, so these are my counts. They are reproducible by counting `description-paragraph` blocks on the Google Patents page. For US 9,276,542 / 9,350,309 / 8,160,274 I only use [GP ¶n] (no PDF fetched).
- Where the Google machine text and the printed page differ, the printed page (viewed as an image) wins and I say so.

### 0.3 What I actually read

| Patent | Read | How |
|---|---|---|
| US 7,254,243 B2 | Full description + claims. FIG. 2 schematic viewed as an image (labels only; component values not transcribed). | Google Patents text + PDF |
| US 8,284,955 B2 | Full description + claims. FIG. 1, 7, 9, 10 viewed as images; equations in cols. 9-14 verified against page images. | Google Patents text + PDF |
| US 8,462,963 B2 | Full description + claims. Col. 7-8 page image viewed to confirm the numeric ranges. | Google Patents text + PDF |
| US 8,472,642 B2 | Full description + claims. Schematic FIGS. 2A-2E not examined (circuits). | Google Patents text + PDF |
| US 8,229,136 B2 | Full description + claims. Diffed against 8,284,955 (DSP method text and code are essentially identical; differences listed in section 9). Cols. 13-14 page image viewed. | Google Patents text + PDF |
| US 8,160,274 B2, US 9,350,309 B2 | Description compared sentence-by-sentence against 8,284,955 for new numeric content (none found on the core chain); 9,350,309 claims read. | Google Patents text |
| US 9,276,542 B2 | Only the "profile" passages ([GP ¶24, 35, 37, 57, 190-221]) and claim 1 start. Not read in full. | Google Patents text |

Everything fetched came from `patents.google.com/patent/<id>/en` (robots.txt allows `/patent/`; I used 3-6 s delays and a generic `Mozilla/5.0 (compatible; research-client)` User-Agent) plus the PDF files that the Google page links on `patentimages.storage.googleapis.com` (no robots.txt exists there). No CAPTCHA or access error occurred. No personal details were sent.

### 0.4 What I could NOT read or verify

- Component values in the analog schematics (US 7,254,243 FIG. 2; US 8,472,642 FIGS. 2A-2E). I only read the block labels and the ten band labels (section 5.6).
- Figures other than FIG. 1, 7, 9, 10 of 8,284,955 (e.g. FIG. 2, 4, 5, 6, 8, 11-13 frequency-response plots/diagrams) were not examined as images; I rely on their text descriptions.
- US 9,276,542 beyond the profile passages; the many later family members (US 9,413,321, 9,793,872, 10,158,337, etc.) were not opened.

---

## 1. Summary of what the patents give, and what they do not

**Stage order (confirmed, section 2):** input gain (101) -> low shelf 1 (102) -> high shelf 1 (103) -> compressor 1 (104) -> low shelf 2 (105) -> high shelf 2 (106) -> 11-band graphic EQ (107) -> compressor 2 (108) -> output gain (109).

**Numbers actually stated for the 9-stage chain (US 8,284,955 / 8,229,136):**

| Item | Value | Where |
|---|---|---|
| Shelf corner frequency, all four shelves | 1000 Hz ("1 kHz") | cl. 8, 9; col. 10 ll. 39-41; col. 21 |
| Shelf order | first order | col. 8, top [GP ¶53] |
| Graphic EQ | 11 cascaded second-order bell filters; first at 30 Hz, eleventh at 16000 Hz, others "approximately one octave" | cl. 10-13; col. 3; col. 8 |
| Distinct fc / Q values in coefficient table | eleven fc, sixteen Q | col. 16 [GP ¶117] |
| Fixed coefficients for the bank | 22 (k1, k2 per band) | col. 10 ll. 28-31 |
| Compressor level time constants | alpha_att = exp(1/(Fs Tatt)), alpha_rel = exp(1/(5 Fs Trel)) (as printed; sign problem, see 4.5) | col. 13 |
| Compressor GR table | 100 integer points x 6 ratios = 600 words | col. 14 ll. ~43-47 |
| Filter architecture | direct form type 1 (DF1), first/second order | col. 14 |

**Not specified anywhere in the 9-stage chain:** sample rate; shelf gains (dB); compressor threshold, ratio, attack, release values; knee; makeup gain; lookahead; per-band EQ Q values; per-band EQ gain range; input/output gain values; numeric formats beyond `float`; stereo/linking behaviour; profile contents beyond "coefficients". Details and recommendations are in section 8.

**Numbers that exist only in sibling embodiments** (not the 9-stage chain; do not silently mix): 7,254,243 tilt EQ (-10 dB @ 100 Hz, +8 dB @ 8 kHz); 8,462,963 shelves "up to about 12 dB", 10-band parametric EQ (+18 / -12 dB, Q 0.4 to 8), split-band compressor ranges. See sections 3.5, 4.7, 5.7.

---

## 2. Stage order

### 2.1 Patent text [V]

US 8,284,955, col. 6 [GP ¶46]:

> "FIG. 1 illustrates an example digital signal process flow of a method 100 according to one embodiment of the present invention. Referring now to FIG. 1, method 100 includes the following steps: input gain adjustment 101, first low shelf filter 102, first high shelf filter 103, first compressor 104, second low shelf filter 105, second high shelf filter 106, graphic equalizer 107, second compressor 108, and output gain adjustment 109."

- FIG. 1 (sheet 1 of 13), viewed as an image, shows boxes in this order: G1 "input gain adjust" (101), H1(z) "low shelf 1" (102), H2(z) "high shelf 1" (103), C1 "compressor 1" (104), H3(z) "low shelf 2" (105), H4(z) "high shelf 2" (106), GEQ(z) "11-band graphic equalizer" (107), C2 "compressor 2" (108), G2 "output gain adjust" (109). Input 110, output 111.
- The reference program in col. 26 [GP ¶190] calls the functions in the same order: `scale` (input gain), `low_shelf`, `high_shelf`, `compressor`, `low_shelf`, `high_shelf`, eleven `bell`, `compressor`, `scale` (output gain).
- Claim 1 and claim 16 of 8,284,955 recite the same order; so do claims 1 and 11 of 8,229,136 (col. 30) and claim 1 of 9,350,309.
- Order is not mandatory in the text. Col. 6 [GP ¶48]: "In one embodiment, steps 101-109 may be completed in numerical order, though they may be completed in any other order. In one embodiment, steps 101-109 may exclusively be performed, though in other embodiments, other steps may be performed as well. In one embodiment, each of steps 101-109 may be performed, though in other embodiments, one or more of the steps may be skipped."
- Placement in a host device, col. 6 [GP ¶47]: "digital signal processing method 100 may be performed on the audio signal after it has passed through the source selector, but before it passes through the volume control."
- Input gain purpose, col. 7 [GP ¶49]: "input gain adjustment 101 provides a desired amount of gain in order to bring input audio signal 110 to a level that will prevent digital overflow at subsequent internal points in digital signal processing method 100." No value given.

**Conclusion:** the nine-stage order given in the task is confirmed as written.

### 2.2 Later-family addition [V]

US 9,350,309 claim 1 ends the chain with: "adjusting the gain of the second compressed signal a second time, ... wherein the adjustment comprises a gain reduction if the gain of the second compressed signal is above a predetermined threshold". So the output gain stage may also act as a threshold-triggered gain reducer. No threshold or law is given. [R] Treat as an optional output limiter/ceiling.

### 2.3 Sibling chains with different structure [V]

- **US 7,254,243 / 8,472,642 (analog origin):** three blocks only: equalizer 11 (bass down, treble up) -> compressor 12 -> "mirror image" equalizer 13 (bass up, treble down); a 10-band graphic equalizer 21 "disposed at the output of mirror image equalizer 13" (7,254,243 col. 2; 8,472,642 col. 3 [GP ¶15]). 7,254,243 col. 2: "The equalizer 11 and the 'mirror image' equalizer 13 are ideally chosen so that they have equal and opposite affects."
- **US 8,462,963 "method 400", col. 8 [GP ¶51]:** "method 400 includes the following stages: a signal conditioning stage 405, first low-filtering high shelve filtering stage 410, a first compressor stage 415, a second low-high shelve filtering stage 420, an equalizing 425, a second compressor stage 430, and an output gain adjustment stage 435." Same skeleton as the nine-stage chain with the low/high shelves merged into one stage each. Its "method 200" (cols. 5-8) adds a pre-input high-pass filter and a loudness normaliser around the chain (section 3.5, 4.7).

---

## 3. Shelf filters (stages 102, 103, 105, 106)

### 3.1 Parameters stated [V]

- Corner frequency: all four are 1000 Hz. Claims 8 and 9 (first low shelf, first high shelf: "a cutoff frequency at 1000 Hz"); col. 10 ll. 39-41 [GP ¶74]: "As with the bell filters, the shelving filters are at fixed corner frequencies (in fact, all of them have 1 kHz as their corner frequency) and the bandwidth is also fixed."; col. 21 [GP ¶158]: "all four of the shelving filters in digital signal processing method 100 have a corner frequency of 1 kHz".
- Order: col. 8, top [GP ¶53]: "all of the shelving filters in the present invention are first-order shelving filters, which means they can usually be represented by a first-order rational transfer function: H(z) = (b0 + b1 z^-1)/(1 + a1 z^-1)."
- Gain: not stated numerically. Col. 7 [GP ¶50]: "each of the low-shelf filters 102, 105 is a filter that has a nominal gain of 0 dB for all frequencies above a certain frequency termed the corner frequency. For frequencies below the corner frequency, the low-shelving filter has a gain of ±G dB, depending on whether the low-shelving filter is in boost or cut mode, respectively." Col. 21 [GP ¶158]: "The value of G is different for each of the four shelving filters."
- Parameterisation, col. 10 ll. 44-52 [GP ¶74]: four shelves "completely described simply by H1(z) ... fixed k1, variable G1; H2(z) ... fixed k2, variable G2; H3(z) ... fixed k3, variable G3; H4(z) ... fixed k4, variable G4" (here k^i is the first-order allpass parameter, i.e. alpha; the superscripts in the print are indices).
- Mirror relation, col. 10 ll. 55-60 [GP ¶75]: "Digital signal processing method 100 relies on the filters H1(z) and H3(z) being the mirror images of one another and the same holds for H2(z) and H4(z). This led to the use of a special filter structure for the boosting shelving filters, one that leads to perfect magnitude cancellation for H1, H3 and H2, H4, as shown in FIG. 8. This type of frequency response is known as magnitude complementary."
- High shelf definition, col. 8 [GP ¶55]: "each of the high-shelf filters 103, 106 is nothing more than the mirror image of a low-shelving filter. That is, all frequencies below the corner frequency are left unmodified, whereas the frequencies above the corner frequency are boosted or cut by G dB. The same caveats regarding steepness and asymmetry apply to the high-shelving filter."
- Corner convention problem the patent wants to avoid, col. 7 [GP ¶52]: "It is standard to specify a low-shelving filter in cut mode to have a response that is at -3 dB at the corner frequency, whereas a low-shelving filter in boost mode is specified such that the response at the corner frequency is at G-3 dB-namely, 3 dB down from maximum boost. ... This leads to a certain amount of asymmetry, where for almost all values of boost or cut G, the cut, and boost low-shelving filters are not the mirror images of one another."
- Summary/claims also say the second low shelf "is a magnitude-complementary low-shelf filter" (claims 14, 20 of 8,284,955; col. 3 [GP ¶11]).

Is the second pair the mirror of the first? Yes, in the magnitude-complementary sense: H3 = 1/H1 and H4 = 1/H2 in magnitude [V, col. 10]. Which of the four is "cut" and which is "boost" is not stated per stage. [I] From the chain's purpose (bass down / treble up before compression, mirrored after; section 2.3) and the code convention in 3.3, the first pair would be: low shelf 1 = cut mode (bass reduced), high shelf 1 = boost mode (treble raised); second pair: low shelf 2 = boost mode, high shelf 2 = cut mode, each pair using the same G as its partner.

### 3.2 First-order allpass and shelf equations [V]

All from US 8,284,955, cols. 11-12 [GP ¶76-88] (identical in 8,229,136 cols. 12-13; the page image for 8,284,955 cols. 11-12 was checked):

```
A(z) = (alpha - z^-1) / (1 - alpha z^-1)
alpha = (1 - sin(2 pi fc / Fs)) / cos(2 pi fc / Fs)
        where fc is the desired corner frequency and Fs is the sampling frequency.

Cut mode (low shelf):
H(z) = (1+G)/2 { 1 + (1-G)/(1+G) A(z) }.
(col. 11: "(A high-shelving filter can be obtained by changing the term (1-G) to (G-1))")

Inverse:
1/H(z) = 2 / ( (1+G) ( 1 + (1-G)/(1+G) A(z) ) )
(col. 11: "This equation is problematic because it contains a delay-free loop ...
 Fontana and Karjalainen (IEEE Signal Processing Letters, Vol. 10, No. 4, April 2003)
 show that each step can be 'split' in time into two 'sub-steps.'")

Boost mode, as printed (cols. 11-12; identical equation printed twice):
y[k] = 1/(1 + alpha(1-G)/(1+G)) { 2/(1+G) x[k] + (1-G)/(1+G) l0[k] }
     = 2/((1+G) + alpha(1-G)) { x[k] + (1-G)/2 l0[k] }

col. 12: "when the shelving filters of digital signal processing method 100 are in 'boost' mode,
the following equation can be used with the same value of G as used in 'cut' mode"
col. 12: "(Note: Equation 16 can be changed to make a high-shelving filter by changing
the sign on the (1-G)/2 term)."
```

Sub-step description (col. 11, FIG. 9): "During the first sub-step (labeled 'subsample 1'), feed filter A(z) with zero input and compute its output, l0[k]. During this same subsample, calculate the output y[k] using the value of l0[k] ... Next, the switches are thrown to the 'subsample 2' position and the only thing left to do is update the internal state of the filter A(z)." FIG. 9 (viewed): x[k] is summed with 0.5[1-G] times l_g[k], multiplied by 2[(1+G)+a(1-G)]^-1, giving y[k], which is held and fed to A(z).

Difference equation of the allpass, col. 20 [GP ¶146-150]: `y[k] = alpha x[k] - x[k-1] + alpha y[k-1]`. (8,229,136 [GP ¶171] prints the rearranged form correctly as `y[k]=allpass_coef(x[k]+y[k-1])-x[k-1]`; the 8,284,955 print has a misplaced parenthesis.)

Units: in all equations G is a **linear amplitude factor**. The prose says "G dB". The two usages are reconciled by G_linear = 10^(G_dB/20) [I].

### 3.3 Reference code and coefficient layout [V]

From 8,284,955 cols. 20-22 [GP ¶155-169] (formatting normalised; `\\` comment markers are as printed):

```c
void low_shelf(float *xv, float *yv, float *wpt, float *input, float *output)
{
    float l;
    int i;
    for (i = 0; i < NSAMPLES; i++)
    {
        if (wpt[2] < 0.0) \\ cut mode, use conventional realization
        { \\ allpass_coef = alpha
            yv[0] = ap_coef * (*input) + (ap_coef * ap_coef - 1.0) * xv[0];
            xv[0] = ap_coef * xv[0] + *input;
            *output++ = 0.5 * ((1.0 + wpt[0]) * (*input++) + (1.0 - wpt[0]) * yv[0]);
        }
        else \\ boost mode, use special realization
        {
            l = (ap_coef * ap_coef - 1.0) * xv[0];
            *output = wpt[1] * ((*input++) - 0.5 * (1.0 - wpt[0]) * l);
            xv[0] = ap_coef * xv[0] + *output++;
        }
    }
}
```
(The first printing of the boost branch at [GP ¶156] shows `* 1);` where the later reprint at [GP ¶166] shows `* l);`; the variable is `l`.)

```
wpt[0]=G
wpt[1]=2[(1+G)+alpha(1-G)]^-1
wpt[2]=-1 when cutting, 1 when boosting
```
col. 21 [GP ¶158]: "wpt is of length three ... Because the shelving filters are all first-order filters, the state-variable arrays are only of length one. There are distinct 'x' and 'y' state variables for each shelving filter ... The value of alpha is the same for all shelving filters because it is determined solely by the corner frequency".

Equations the patent says the code implements (col. 21 [GP ¶164, 166]):

```
cut:   y[k] = alpha*in[k] + (alpha^2 - 1)*x[k] ;  x[k] = a*x[k] + in[k] ;  out[k] = 1/2((1+G)*in[k] + (1-G)*y[k])
boost: l0[k] = (alpha^2 - 1)*x[k] ;  out[k] = 2[(1+G)+alpha(1-G)]^-1 * (in[k] - 1/2(1-G)*l0[k]) ;  x[k] = alpha*x[k-1] + out[k]
```

High shelf, cols. 21-22 [GP ¶168-169]:

```c
void high_shelf(float *xv, float *yv, float *wpt, float *input, float *output)
{
    float l;
    int i;
    for (i = 0; i < NSAMPLES; i++)
    {
        if (wpt[2] < 0.0) \\ cut mode, use conventional realization,
        { \\ allpass_coef = alpha
            yv[0] = allpass_coef * (*input) + (allpass_coef * allpass_coef - 1.0) * xv[0];
            xv[0] = allpass_coef * xv[0] + *input;
            *output++ = 0.5 * ((1.0 + wpt[0]) * (*input++) - (1.0 - wpt[0]) * yv[0]);
        }
        else \\ boost mode, use special realization
        {
            l = (allpass_coef * allpass_coef - 1.0) * xv[0];
            *output = wpt[1] * ((*input++) + 0.5 * (1.0 - wpt[0]) * l);
            xv[0] = allpass_coef * xv[0] + *output++;
        }
    }
}
```
[GP ¶169]: "Implementing the high-shelving filter is similar to implementing the low-shelving filter. Comparing the two functions above, the only substantive difference is in the sign of a single coefficient. Therefore, the program flow is identical."

`working_table` slots (col. 26 [GP ¶190]): low_shelf 1 at `&working_table[0]`, high_shelf 1 at `[3]`, low_shelf 2 at `[11]`, high_shelf 2 at `[14]`. (Compressor 1 at `[6]`, bells at `[17]`..`[47]`, compressor 2 at `[50]`; see 4.4, 5.4.)

### 3.4 What my numerical check found about the shelves [N] and [I]

I transcribed the cut and boost code and swept 20 Hz to 20 kHz (Fs = 48 kHz, fc = 1 kHz, alpha = 0.87697646).

1. **Cut mode matches the printed H(z).** Low shelf cut: gain G at DC, 1.0 at Nyquist. High shelf cut: 1.0 at DC, G at Nyquist. Response at the corner is sqrt((1+G^2)/2) [N]; for G = -12 dB that is -2.74 dB, close to the "-3 dB at the corner" convention but not the half-gain point.
2. **The boost branch of the *code* is the exact inverse of the cut branch** for the low shelf (cascade cut -> boost gives a unit impulse to 1e-15; boost response = -1 x cut response in dB at every frequency). [N] This is the "perfect mirror" property.
3. **The printed boost equation and FIG. 9 use "+ (1-G)/2 l0[k]"; the code (and [GP ¶166]) use "- 0.5(1-G) l".** With the plus sign the boost branch is not the mirror of the cut branch (for G = -6 dB it gives -3.1 dB at 20 Hz instead of +6.0 dB). [N] Derivation [I]: with l0 = (alpha^2 - 1) x_state (allpass fed zero input, as the code defines it), the exact inverse of `H = (1+G)/2 + (1-G)/2 A` is `y = 2/((1+G)+alpha(1-G)) (x - (1-G)/2 l0)`, i.e. a minus. The code is right; the printed equation (and FIG. 9 as drawn) carry a sign inconsistency. [R] Follow the code.
4. **High-shelf boost cannot reuse the low-shelf `wpt[1]`.** For the high shelf (`H = (1+G)/2 - (1-G)/2 A`) the exact inverse needs `wpt[1]' = 2/((1+G) - alpha(1-G))` (minus alpha). With the low-shelf value `2/((1+G)+alpha(1-G))` the high-shelf cut + boost pair does not cancel (residual -3 to -11 dB depending on frequency, G = -6 / -12 dB). With the `-alpha` form the cascade cancels to a unit impulse. [N] The patent says only that the high-shelf differs "in the sign of a single coefficient" ([GP ¶169]) and defines `wpt[1]` once for the low shelf. [I] The `-alpha` form is almost certainly what the real implementation used for high-shelf boost; the listing does not say.
5. **Closed-form direct-form coefficients** (my derivation, verified equal to the structure to 1e-16) [I][N]. With y[k] = b0 x[k] + b1 x[k-1] - a1 y[k-1] and a1 = -alpha:
   - low shelf, cut: `b0 = 0.5[(1+G) + alpha(1-G)]`, `b1 = -0.5[alpha(1+G) + (1-G)]`
   - high shelf, cut: `b0 = 0.5[(1+G) - alpha(1-G)]`, `b1 = 0.5[-alpha(1+G) + (1-G)]`
   - boost = exact inverse of the corresponding cut filter with the same G (stable: its pole is the cut filter's zero, 0.959 and 0.657 for G = -10 dB).
6. **Test vectors** [N] (Fs = 48 kHz, fc = 1 kHz): alpha = 0.87697646; for G = -10 dB (G_lin = 0.31622777): low-shelf cut b0 = 0.95793996, b1 = -0.91903650, a1 = -0.87697646, boost wpt[1] = 1.04390676; high-shelf cut b0 = 0.35828781, b1 = -0.23526427, high boost wpt[1]' = 2.79105229. For Fs = 44.1 kHz, alpha = 0.86678844.

### 3.5 Sibling-embodiment shelf numbers (not the 9-stage chain) [V]

- US 7,254,243 col. 2: first equalizer "Ideally the low audible bass portion (taken at 100 Hz) is reduced by about 10 dB, the high audible treble portion (taken at 8 kHz) is increased by about 8 dB, and the portions in between are reduced or enhanced in a linearly proportional manner, i.e. the increase is a substantially linear function of frequency." Mirror equalizer: "Preferably the low audible bass portion (100 Hz) is increased about 10 dB and high audible treble portion (10 kHz) is reduced about 8 dB. The intermediate portions of the signal are reduced as a substantially linear function of frequency." Claim 6 (col. 4) instead says the mirror reduces treble "by about 8 dB at 8 kHz": the 10 kHz vs 8 kHz mismatch is in the patent itself. Same in 8,472,642 col. 2 and claims 5, 7, 13, 15.
- US 8,462,963 col. 6 [GP ¶39]: shelving set "Each of the filters can boost or reduce the signal up to about 12 dB. In one embodiment, the crossover point between the high and low frequency filters is located at about 1000 hertz." Col. 7 [GP ¶41]: "the low audible bass portion of the raw audio signal (taken at 100 Hz) is reduced by about 10 dB. Similarly, the high audible treble portion (taken at 8 kHz) is increased by about 8 dB". Col. 7 [GP ¶43]: second set is "a mirror image of the low-high shelving filter set in step 215", "the second low-high filter set has a crossover point at 1000 Hz. Each of the filters can boost or reduce the sisal [sic] level by 12 dB."

[I] Taken together these suggest the shelf gains are around -10 dB (bass) and +8 dB (treble) targets with a 12 dB limit, but the nine-stage patents do not state values, so this is a guess about the product's tuning, not patent content.

---

## 4. Compressors (stages 104 and 108)

### 4.1 Description [V]

US 8,284,955 col. 12 [GP ¶89]:

> "Each of the compressors 104, 108 is a dynamic range compressor designed to alter the dynamic range of a signal by reducing the ratio between the signal's peak level and its average level. A compressor is characterized by four quantities: the attack time, Tatt, the release time, Trel, the threshold, KT, and the ratio, r. In brief, the envelope of the signal is tracked by an algorithm that gives a rough 'outline' of the signal's level. Once that level surpasses the threshold, KT, for a period of time equal to Tatt, the compressor decreases the level of the signal by the ratio r dB for every dB above KT. Once the envelope of the signal falls below KT for a period equal to the release time, Trel, the compressor stops decreasing the level."

FIG. 10 (viewed): static curve, slope 1 below threshold KT; above it "output level increases by 1/r dB for every 1 dB increase in input level". Both axes labelled in dB; threshold K_T drawn on both axes.

### 4.2 Level detector [V]

Col. 13 ll. ~20-36 [GP ¶97-98]:

> "The most important part of the compressor algorithm is determining a meaningful estimate of the signal's level. This is accomplished in a fairly straightforward way: a running 'integration' of the signal's absolute value is kept, where the rate at which the level is integrated is determined by the desired attack time. When the instantaneous level of the signal drops below the present integrated level, the integrated level is allowed to drop at a rate determined by the release time."

```
L[k] = (1 - alpha_att)|x[k]| + alpha_att L[k-1]   for |x[k]| >= L[k-1]
       (1 - alpha_rel)|x[k]| + alpha_rel L[k-1]   for |x[k]| <= L[k-1]
where alpha_att = exp( 1 / (Fs Tatt) )   and   alpha_rel = exp( 1 / (5 Fs Trel) )
```
(Exactly as printed on the page image of 8,284,955 col. 13 and of 8,229,136 col. 14: no minus sign inside the exp(), and a factor 5 in the release exponent. 8,229,136 prints the branch conditions as `>=` and `<` instead of `>=` and `<=`.)

Detector type, from the above: a one-pole smoother of the **rectified (absolute) input**, with separate attack and release coefficients selected by comparing |x[k]| with the previous level. It is not an RMS detector and has no peak-hold. (The analog 2004 circuit used an RMS detector: 7,254,243 col. 2 says the "components designated VCA, RMS, A1, A2 and A3 in block 12 ... is available from That Semiconductor Corp. under the product designation IC 4200".)

### 4.3 Gain computer [V]

Col. 13 ll. ~44-60 [GP ¶99-102]:

```
G_excess = L[k] K_T^-1.
(If the excess gain is less than one, the input signal is not changed and passed through to the output.)
When the excess gain exceeds one:
G_R = (G_excess)^((1-r)/r) = (L[k] K_T^-1)^((1-r)/r)
output[k] = G_R x[k].
"Through this procedure, an output signal whose level increases by 1/r dB for every 1 dB increase in the input signal's level is created."
```
Earlier log-domain derivation (col. 12-13, [GP ¶91-96]) as printed, including the patent's typographic slips (`K` for `R` in the last line; `gR = g_excess / R`):
```
g_excess = 20 log10(L) - 20 log10(K_T)
gR = g_excess / R = (1/R) . (20 log10(L) - 20 log10(K_T))
g = (10^log10(x))^((1/R).(log10(L) - log10(K_T)))
y = x^((1/K).(log10(L) - log10(K_T))) = x^((1/K).(log10(L/K_T)))
```
These intermediate equations are not used by the code; the code uses the G_R formula above.

Interpretation [I]: the ratio r is the usual r:1 downward compression ratio on the level (output excess = excess^(1/r)), threshold K_T is a **linear amplitude** (the code compares `level*invT > 1.0` with `level` a linear |x| average), the curve has a **hard knee**, and the gain G_R is applied **to the current sample** (feed-forward; no lookahead, no separate gain smoothing). Note the prose in [GP ¶90] says the compressor "attenuates the input signal by r dB for every dB by which the level L exceeds KT" and [GP ¶99] says the input is "scaled by an amount that is proportional to the amount by which the level exceeds the threshold. The constant of proportionality is equal to the compressor ratio, r"; the equations (and FIG. 10) are the standard r:1 law, so I treat those prose lines as loose wording.

### 4.4 Table-lookup approximation and reference code [V]

Col. 14 ll. ~12-47 [GP ¶103-106]: because division and exponentiation are slow on the target DSPs, K_T^-1 is pre-computed ("a pre-computed table of K_T^-1 values") and G_R is read from tables: "a list of, say, 100 values of GR, pre-computed at integer values of GR from GR=1 to GR=100 can be created for every possible value of ratio r." Interpolation (printed cleanly in 8,229,136 col. 14, which uses floor brackets):
```
interp = G_excess - floor(G_excess)
G_R,0 = floor(G_excess)^((1-r)/r)        and   G_R,1 = floor(1 + G_excess)^((1-r)/r)
G_R ~= G_R,0 + interp * (G_R,1 - G_R,0)
```
(8,284,955 prints this with the floor brackets and the `*` lost: `interp-G_excess (G_excess)` and `G_R ~= G_R,0+interp-(G_R,1-G_R,0)`; read it as the 8,229,136 version.) Col. 14: "In one embodiment, tables for six different values of ratio, r, and for 100 integral points of Gexcess may be stored in memory. In such an embodiment, the entire memory usage is only 600 words of memory".

Code, cols. 23-25 [GP ¶178-186]:

```c
void compressor(float *input, float *output, float *wpt, int index)
{
    static float level;
    float interp, GR, excessGain, L, invT, ftempabs;
    invT = wpt[2];
    int i, j;
    for (i = 0; i < NSAMPLES; i ++)
    {
        ftempabs = fabs(*input++);
        level = (ftempabs >= level)? wpt[0] * (level - ftempabs) + ftempabs
                                   : wpt[1] * (level - ftempabs) + ftempabs;
        GR = 1.0;
        if (level * invT > 1.0)
        {
            excessGain = level *invT;
            interp = excessGain - trunc(excessGain);
            j = (int) trunc(excessGain) - 1;
            if (j < 99)
            {
                GR = table[index][j] + interp * (table[index][j+1] - table[index][j]);
                // table[ ][ ] is the exponentiation table
            }
            else
            {
                GR = table[index][99];
            }
        }
        *output++ = *input++ * GR;
    }
}
```
```
wpt[0] = attack constant alpha_att, wpt[1] = release constant alpha_rel, wpt[2] = 1/K_T
table[index][j] = (j)^((1-index)/index)            (as printed, col. 25 [GP ¶186])
```
[GP ¶181]: "The other elements of the wpt array include the attack time, the release time, and the compressor ratio." (The code only uses wpt[0..2]; the working-table layout reserves five slots per compressor: compressor 1 at `&working_table[6]`, next block at `[11]`; compressor 2 at `&working_table[50]`.) The first compressor and the second compressor are both called with `ratio1Index` in the main program (col. 26 [GP ¶190]).

### 4.5 Defects and inconsistencies in the compressor text [I][N]

1. **Sign of the time-constant exponent.** As printed, alpha_att = exp(+1/(Fs Tatt)) > 1. The smoother `L[k] = (1-alpha)|x| + alpha L[k-1]` with alpha > 1 has a pole outside the unit circle and cannot track a level. Every conventional design uses exp(-1/(Fs T)). The patent text and both printings (8,284,955, 8,229,136), plus 9,350,309, carry the missing minus, so it is a printed-text defect, not a one-off OCR loss. [R] Use exp(-1/(Fs T)).
2. **Factor 5 in the release.** With the minus restored, alpha_rel = exp(-1/(5 Fs Trel)) = exp(-1/(Fs tau)) with tau = 5 Trel, while alpha_att = exp(-1/(Fs Tatt)) has tau = Tatt. [I] So as printed the attack time is the smoother's time constant and Trel is one fifth of the release time constant. The patent gives no explanation for the 5; whether "Trel" meant a time constant or something else is unknown.
3. **Shared state.** `static float level;` is a function-level static, so compressor 1, compressor 2 and every channel would share one level variable if the listing were compiled literally. The patent text says the level "needs to be tracked continuously, for the entire duration of the program". [R] Keep one level per compressor instance per channel.
4. **Pointer advance.** `ftempabs = fabs(*input++)` and `*output++ = *input++ * GR;` advance `input` twice per sample (and read the next sample as the signal). Typo in the listing; intended: output[k] = x[k] * GR[k].
5. **Table indexing.** `j = trunc(excessGain) - 1` indexes entries 0..99 for excess gain 1..100, but the printed table is `(j)^((1-index)/index)`, which at j = 0 is 0^exponent (infinite for r > 1). Consistent contents would be `table[index][j] = (j+1)^((1-r)/r)`. Also `index` doubles as the ratio value in the table formula and as a table-row index in the call. Which six ratios the six tables hold is not stated.
6. `L` and `interp` unused/unclear; the `j < 99` branch is a clamp at excess gain 100 (no further compression slope beyond 100x = +40 dB over threshold: GR frozen at table[index][99]).

### 4.6 What is NOT specified for the compressors [V]-absent

Threshold values (K_T) for either compressor; ratio values (six tables, values unknown); attack and release times; whether compressor 1 and 2 differ; knee shape (the code is a hard knee); detector type beyond the rectified one-pole smoother; makeup gain (none in the chain; the output gain stage 109 follows); lookahead (none; zero latency); sidechain filtering; stereo linking (only one `level` variable); behaviour at silence (level initial value).

Target for the overall dynamic range, from the 2004 patents [V]: US 7,254,243 col. 2: "Preferably the dynamic range is reduced to 10 dB or less." and claim 3: "average noise floor in excess of about 80 dB and said compressor circuit can compress the dynamic range of said equalized audio signal to less than about 10 dB." Noise floor 80 dB and a 110 dB ceiling are quoted in 7,254,243 col. 1-2, 8,472,642 col. 1-2, 8,229,136 col. 3 [GP ¶14]. US 8,462,963 col. 9 [GP ¶55]: "Preferably the dynamic range is reduced to 10 dB or less."

### 4.7 Sibling compressor specification (US 8,462,963 method 200, not the 9-stage chain) [V]

Col. 8, ll. ~1-8 [GP ¶45] (page image confirmed):

> "In step 235, the output signal from step 230 is further compressed. In one embodiment a split band compressor is used. The split band compressor may comprise a low frequency compressor that operates in the range of 0 to 500 Hz and a high frequency compressor that operates in the range of 10 k to 20 k Hz. In one specific embodiment, each compressor has the following specifications: threshold of 0 to -52 dB; ratio of 1:1 to 800.1; make up gain of +12 dB to 90 dB; attack time of 0 to 99 ms; and release time of 0 to 495 ms. In one embodiment, one of the main functions of step 235 is to prevent the speaker or headset to operate in a range that exceed its physical design limitations."

These are **parameter ranges** for a different embodiment (a speaker/headset protection compressor), printed with odd values ("800.1", "+12 dB to 90 dB"). They do not define the 9-stage chain's compressors. They are the only numeric compressor ranges in the five patents. Same patent, col. 6-7 [GP ¶37, 42, 46-50]: input conditioning uses "a leaky integrator, which generally calculates a slow-moving envelop based on an integrated version of the rectified signal", an expander (below a reference loudness) or limiter (above) to hold constant loudness, and "the gain add or subtracted from the audio signal in step 315 is subtracted or added back in by an amount that is exactly inverse to the amount taken or added".

---

## 5. Graphic equaliser (stage 107)

### 5.1 Structure and bands [V]

- Col. 8 [GP ¶58]: "graphic equalizer 600 consists of a cascaded bank of eleven second-order filters, F1(z), F2(z), . . . , F11(z)."
- Col. 3 [GP ¶11] / claims 10-13 / col. 8 [GP ¶56]: "each of the second order filters achieves a bell-shaped boost or cut at a fixed centerfrequency, with F1(z) centered at 30 Hz, F11(z) centered at 16000 Hz, and the other filters in between centered at roughly one-octave intervals." Claim 13: "the second to tenth filters are centered at approximately one octave intervals from each other."
- Fixed Q per band; gain G per band is the only run-time variable: col. 9-10 [GP ¶70]: "in a typical graphic EQ, the gain G and quality factor Q remain constant and only G is allowed to vary. A(z) does not depend in any way on the gain, G and that if Q and the center-frequency fc remain fixed (as they do in a graphic EQ filter), then k1 and k2 remain fixed regardless of G."
- Col. 10 [GP ¶74]: "the entire graphic equalizer bank depends on a total of 22 fixed coefficients that need to be calculated only once and stored in memory. The 'tuning' of the graphic equalizer is accomplished by adjusting the parameters G1,G2, . . . , G11."
- Q definition, col. 8 [GP ¶57]: "The quality factor is defined as the ratio of the filter's center frequency to its 3-dB bandwidth, B, where the 3-dB bandwidth is illustrated as in the figure: the difference in Hz between the two frequencies at which the filter's response crosses the -3 dB point."
- Coefficient table size, col. 16 [GP ¶117]: "Since there are eleven specific values of fc and sixteen specific values of Q in the algorithm, and the filter operates at a single sampling frequency, Fs, and only k2 depends on both fc and Q, the overall storage requirements for the k1 and k2 coefficient set is quite small (11x16x2 words at worst)." So the product design offers up to 16 Q values selectable per fc, but the values are not listed. This sentence also shows that the author meant **k2 to carry the Q dependence** (see 5.3).
- Gain range: none given for the 11-band EQ.

### 5.2 Mitra-Regalia bell: equations as printed [V]

Cols. 9-10 [GP ¶62-72] (and repeated cols. 16-17 [GP ¶113-116]; page image of cols. 9-10 checked):

```
H(z) = (b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2)         (bell, centre fc, quality factor Q, sampling Fs)

"Ancillary quantities k1, k2 can be defined by"
k1 = (1 - tan(pi fc / (Q Fs))) / (1 + tan(pi fc / (Q Fs)))
k2 = -cos(2 pi fc / Fs)

A(z) = (k2 + k1(1+k2) z^-1 + z^-2) / (1 + k1(1+k2) z^-1 + k2 z^-2)         "A(z) can be verified to be an allpass filter."

H(z) = 1/2 (1+G) A(z) + 1/2 (1-G)                                             "This is the crux of the Mitra-Regalia realization."

Gain-dependent quantities updated at run time: 1/2 (1+G) and 1/2 (1-G).
```
FIG. 7 (viewed): the allpass branch is multiplied by "0.5*(1+G)", the direct input branch by "0.5*(1-G)", then summed. Symmetric form for the allpass (col. 16-17 [GP ¶118-120]):
```
A(z) = (z^-2 + geq_b1 z^-1 + geq_b0) / (1 + geq_b1 z^-1 + geq_b0 z^-2)
geq_b0 = k2 ;  geq_b1 = k1(1+k2)
y[k] = geq_b0 x[k] + geq_b1 x[k-1] + x[k-2] - geq_b1 y[k-1] - geq_b0 y[k-2]
     = geq_b0 (x[k] - y[k-2]) + geq_b1 (x[k-1] - y[k-1]) + x[k-2]
```

### 5.3 Two printed-text problems in the bell equations [N][I]

**(a) The names k1 and k2 are swapped in the printed definitions.** With k1 = (1 - tan(...))/(1 + tan(...)) and k2 = -cos(2 pi fc/Fs) the allpass denominator `1 + k1(1+k2) z^-1 + k2 z^-2` has `z^-2` coefficient -cos(...) (about -0.99): poles on the real axis, not a resonator. Simulated (fc = 1 kHz, Q = 2, Fs = 48 kHz, G = +12 dB), that filter gives +10.6 dB at 100 Hz, +11.9 dB at 500 Hz, +12.0 dB at 2 kHz and 10 kHz and about +4 dB at Nyquist, i.e. a broad shelf-like boost, not a bell. [N] Swapping the two definitions:
```
k1 = -cos(2 pi fc / Fs)
k2 = (1 - tan(pi fc / (Q Fs))) / (1 + tan(pi fc / (Q Fs)))
geq_b0 = k2 ;  geq_b1 = k1 (1 + k2)
```
gives, for fc = 1000 Hz, Q = 2, Fs = 48 kHz, G = +12 dB: gain exactly 12.000 dB at fc, 0.00 dB at DC and Nyquist, 4 dB at 500 Hz and 2 kHz (a proper bell), and -12.000 dB at fc for G = -12 dB. For a near-zero G (notch), the -3 dB band is 781.9 to 1279.0 Hz: B = 497 Hz against fc/Q = 500 Hz, so the patent's "Q = fc / 3-dB bandwidth" is met to within the usual bilinear warp. Same check for 30 Hz / Q 1 (B = 29.8 Hz) and 16 kHz / Q 2. [N] Two independent hints in the patent point to the swapped version: [GP ¶117] says "only k2 depends on both fc and Q" (true only if k2 holds the tan(pi fc/(Q Fs)) term), and the code comment says `geq_b0 = wpt[1]; \\ k2` with `geq_b1 = wpt[2]; \\ k1(1+k2)` (consistent with A(z)'s structure, not with the printed definitions). [R] Use the swapped definitions.

**(b) The printed H(z) and the code disagree on which branch carries (1+G).** Printed: `H = 1/2 (1+G) A(z) + 1/2 (1-G)` (and FIG. 7). Code: `output = 0.5*(1.0-gain)*ap_output + 0.5*(1.0+gain)*input`, i.e. `H = 1/2 (1-G) A(z) + 1/2 (1+G)`. [N] The two have identical magnitude response at all frequencies (max difference 1.6e-4 from truncation), but the printed form has phase -175 deg at fc (output sign-inverted at the centre, gain -G), while the code form has 0 deg at fc and the textbook Regalia-Mitra form. [R] Use the code form. For a cascade of eleven bells, the printed form would put alternating phase flips into the sum of the paths; the code form avoids it.

### 5.4 Reference code [V]

Cols. 22-23 [GP ¶170-176]:

```c
void bell(float *xv, float *yv, float *wpt, float *input, float *output)
{
    float geq_gain = wpt[0]; \\ G
    float geq_b0 = wpt[1]; \\ k2
    float geq_b1 = wpt[2]; \\ k1(1+k2)
    float ap_output;
    int i;
    for (i = 0; i < NSAMPLES; i++)
    {
        ap_output = geq_b0 * (*input - yv[0]) + geq_b1 * (xv[1] - yv[1]) + xv[0];
        xv[0] = xv[1]; \\ update
        xv[1] = *input; \\ update
        yv[0] = yv[1]; \\update
        yv[1] = *output; \\update
        *output++ = 0.5 * (1.0-gain) * ap_output + 0.5 * (1.0+gain) * (*input++);
    }
}
```
[GP ¶172]: "wpt (which contains the three graphic EQ parameters G, k2, and k1(1+k2))"; [GP ¶175] explains `yv[1]=*output;` as "shifts the value of output[k], the output of the allpass filter, to y[k-1]" and `xv[0]=xv[1]` as "shifts the value stored in x[k-1] to x[k-2]". State: xv[0] = x[k-2], xv[1] = x[k-1], yv[0] = y[k-2], yv[1] = y[k-1] ([GP ¶121]).

Issues [N][I]: (i) the listing's variable is `geq_gain` but the formula line uses `gain` (inconsistent naming); (ii) `yv[1] = *output` stores the bell output, but the explanation says it should be the **allpass output**; I simulated the literal version (stores the previous bell output) and it diverges (unstable), while storing `ap_output` gives the correct bell. [R] Store ap_output. The earlier single-bell snippets ([GP ¶122-126], cols. 16-17) have the same two layouts (`allpass()` helper stores `*output`, which there is the allpass output; the combined routine at [GP ¶126] repeats the `yv[1] = *output` slip).

Call sequence (col. 26 [GP ¶190]): eleven calls `bell(xv1_geq, yv1_geq, &working_table[17], ...)` through `bell(xv11_geq, yv11_geq, &working_table[47], ...)`, three words per band (G, k2, k1(1+k2)), each band with its own xv/yv arrays. Runtime tuning = change `wpt[0]` (G) of one band only.

### 5.5 What the bell tuning data would look like [I]

Per band: three floats (G_lin, geq_b0 = k2, geq_b1 = k1 (1 + k2)), with the last two fixed by (fc, Q, Fs). Test vector [N], Fs = 48 kHz: fc = 1000 Hz, Q = 2: k2 = 0.936602, k1 = -0.991445, geq_b1 = k1(1+k2) = -1.920034. fc = 30 Hz, Q = 1: k2 = 0.996081, geq_b1 = -1.996065. fc = 16000 Hz, Q = 2: k2 = 0.267949, geq_b1 = 0.633975.

### 5.6 Analog ancestor: band labels [V, read from drawing image; low confidence]

US 7,254,243 FIG. 2, block 21 ("10-band graphic equalizer 21 for a specific speaker assembly", col. 2): the ten sliders are labelled 30 Hz, 60 Hz, 120 Hz, 250 Hz, 500 Hz, 1 kHz, 2 kHz, 4 kHz, 8 kHz, 16 kHz (read from the schematic at 250 dpi; potentiometers VR9-VR18). This is a ten-band octave EQ whose endpoints match the 30 Hz / 16000 Hz endpoints of the digital eleven-band EQ. [I] The digital EQ may have been that layout plus one extra band; the patents do not say. The same schematic labels the equalizer pots "HF (10 kHz)" and "LF (100 Hz)" in blocks 11 and 13, and the compressor block 12 has pots labelled THRESHOLD, RATIO and a DC OFFSET trimmer around the VCA/RMS IC (read from the drawing; not transcribed beyond labels).

### 5.7 Sibling EQ (US 8,462,963 method 200 step 230) [V]

Col. 7 [GP ¶44]: "In one embodiment, the equalizer is a 10 band fully parametric equalizer. Each of the filters in the equalizer can boost or reduce the signal level by 18 dB or -12 dB, respectively. In one specific embodiment, the 'Q' of the band can be individually adjusted from 0.4 to 8." (Printed as "18 dB or -12 dB, respectively"; read it as +18 dB boost / -12 dB cut.) Settings are "manually set by an audio engineer" and the unit "may be preloaded with various equalizer settings for a variety of speaker and headset types" (cols. 5, 7 [GP ¶30, 44]). Not the nine-stage chain.

---

## 6. Sample rate, numeric format, block structure, stereo

### 6.1 Sample rate [V]-absent
No numeric sampling rate is given in any of the five patents. Fs appears only as a symbol in `alpha`, `k1`, `k2` and the compressor constants. The hardware FIG. 13/14 description (8,229,136 cols. 17-18 [GP ¶142-157]) names ADCs, a DSP and PWM amplifiers but no rate.

### 6.2 Numeric format [V]
- Reference code uses C `float` arrays (cols. 20-26). Col. 14 [GP ¶107]: filter realisations "have trade-offs in terms of complexity, speed of throughput, coefficient sensitivity, stability, fixed-point behavior, and other numerical considerations." That is the only mention of fixed point; no word length, Q-format or scaling is given. [GP ¶49] says input gain exists to "prevent digital overflow".
- Coefficients are "pre-computed values that are stored in memory" in `working_table` ([GP ¶191, 193]): "the real-time loop consists of simple multiplications and additions ... the coefficient computation, which requires complicated transcendental functions ... can be pre-computed before real-time processing takes place."

### 6.3 Architecture statement vs code [V][I]
Col. 14 [GP ¶107]: "In a specific embodiment, a simple architecture known as a direct-form architecture of type 1 (DF1) may be used ... All of the digital filters in digital signal processing method 100 are of either first or second order." The bell code is DF1-like on the *allpass* (it keeps x[k-1], x[k-2], y[k-1], y[k-2]); the shelf code keeps a single state (canonical form of the first-order allpass: `s' = alpha s + in`, `ap = alpha in + (alpha^2 - 1) s`) rather than literal DF1. [I] Any structure with the same transfer function is acceptable.

Sign-convention conflict [V]: the generic difference equation at col. 5 [GP ¶34] reads `y[k] = b0 x[k] + ... + bN x[k-N] + a1 y[k-1] + a2 y[k-2] + ... + aN y[k-N]` (plus), while [GP ¶109, 128] use `y[k] = b0 x[k] + b1 x[k-1] + b2 x[k-2] - a1 y[k-1] - a2 y[k-2]` with `H(z) = (b0 + b1 z^-1 + b2 z^-2)/(1 + a1 z^-1 + a2 z^-2)` (minus, which is the one consistent with H(z)). The generic a_i are not used by the final structures.

### 6.4 Block processing [V]
Cols. 25, 27 [GP ¶187, 194]: each function "is going to be dealing with arrays of input and output data rather than a single sample at a time"; "the algorithm as detailed in this section is written in block form". NSAMPLES is not given; state persists between blocks. Processing is in place (`floatBuffer` in and out).

### 6.5 Stereo / channels [V]-limited
- No statement about stereo processing or linking of compressor detectors.
- The main program (col. 26 [GP ¶190]) processes one `floatBuffer`. Shelf stages 3 and 4 are called with state arguments named `xv3_ap_left, yv3_ap_left, xv3_ap_right, yv3_ap_right` and `xv4_ap_left ... xv4_ap_right` (seven arguments against the five-argument definition), while stages 1 and 2 and all eleven bells have a single state set and the compressor has one function-static level. This is not a consistent stereo design; it looks like a listing assembled from different versions. [I]
- Hardware text: US 8,229,136 col. 4 [GP ¶20]: "provide precision tuning for all 4 independently controlled channels"; [GP ¶146]: PWM amplifier "may contain two sets of two individual amplifiers, i.e., four amplifiers total"; [GP ¶166]: "two vertically mounted cards ... two DSP boards with two channels on each board". So processing is per channel with four independent channels in the vehicle product; linking is not described.
- US 8,284,955 [GP ¶135]: "the amplifier 1308 may include a multi-channel amplification section, such as a stereo amplifier."

---

## 7. Profiles

### 7.1 What the patents say a profile is

**US 8,284,955 / 9,350,309 (the 9-stage chain):**
- Claims: "creating a sound profile for a vehicle where the personal audio listening device will be used" (claims 4, 19); "profile generation circuitry configured to allow a user to create a sound profile for an area by listening to music in the area and adjusting the audio system" (claim 21); 9,350,309 claims 28 and 38 similar.
- Cols. 18-19 [GP ¶143-144]: "the manufacturer or user can create profiles to suit the tastes of their customer and the vehicles they use or purchase ... when a user listens to music using the headphones or other personal listening device, they might adjust the processing of the music by adjusting a first low shelf filter, a first compressor, or a graphic equalizer. They might also change the processing of the music signal by adjusting a second compressor and adjust the gain of the compressed signal after the second compressor. In some examples, the user may adjust an amplifier that increases the amplitude of the signal into an input of headphone drivers."
- Cols. 26-27 [GP ¶191]: coefficients "can be specifically computed for each audio device in which digital signal processing method 100 is to be used ... computed separately for each vehicle ... to account for unique acoustic properties in each vehicle such as speaker placement, passenger compartment design, and background noise."
- Col. 27 [GP ¶192]: standing-wave/room anomalies: "The frequency and amount of adjustment performed, in a further embodiment, may be configured in advance and stored for use in graphic equalizer 107 to reduce anomalous responses for future presentation in the listening environment."
- So, in the nine-stage patents, a profile = the set of pre-computed `working_table` entries (shelf G / wpt triples, compressor alpha_att, alpha_rel, 1/K_T, ratio index, eleven bell triples, gains) tuned per device/vehicle. [I] That is my reading; no schema is given.

**US 9,276,542 (same family, adds profile detail) [GP ¶24, 35, 37, 192-201, 203-205, 212-220]:**
- "A profile comprises a plurality of filter equalizing coefficients (e.g., filter equalizing coefficient modifiers) which may be used to configure the filters of the graphic equalizer" ([GP ¶194]); "In some embodiments, a profile may be directed to a particular type or model of hardware (e.g., speaker), particular listening environment (e.g., noisy or quiet), and/or audio content (e.g., voice, music, or movie)."
- Graphic EQ: "may comprise the 11-band graphic equalizer 107 ... any number of bands" ([GP ¶192]); filters "in parallel with each other" and "second order bell filters" ([GP ¶193]).
- Profile loading: "the plurality of filter equalizing coefficients ... configure the filters; the coefficients of the filters may remain fixed ... Although the filters ... may not be reconfigured with new filter equalizing coefficients, the filters may be periodically adjusted with a gain value (e.g., the gain variable). Computing the gain value ... may be accomplished by varying simple quantities as previously discussed." ([GP ¶199]) So per-profile data can set the fixed Mitra-Regalia coefficients; run-time adjustment is the G term.
- "In some embodiments, the profile may comprise one or more shelf filter coefficients ... (e.g., low shelf 1 102, high shelf 1 103, low shelf 2 105, and high shelf 2 106 of FIG. 1)" ([GP ¶201]); "The equalizing module 1306 or another module may also configure one or more other filters with other coefficients contained within the profile." ([GP ¶205])
- Sources: "previously stored on the digital device ..., retrieved from firmware, or downloaded from a communication network" ([GP ¶204]); "The profile, in some embodiments, may comprise a text file" and "may also be encrypted" ([GP ¶215]). Profiles may be combined ("headphones" + "music" buttons, [GP ¶217-220]) or be single combined profiles. A first and second "gain" may be applied after configuring ([GP ¶207, 210]).
- Examples named, no values: built-in speaker, desktop speaker, headphones; music, movie; cellular telephones, music players, headsets, earpieces, microphones; noisy environments; hearing-impaired listeners ([GP ¶194, 197, 217-218]).

**US 8,462,963 [GP ¶30, 44]:** "audio processing unit 170 is preloaded with various equalizer settings for a variety of speaker and headset types"; "the user may enter a specific code for a particular headset to calibrate the equalizer ... The code can be included along with the operational manual of unit 170 or can be look up via the manufacturer of unit 170 website."

### 7.2 Example profile values given
None. No dB values, frequencies, ratios or Q values for any actual device or vehicle appear in any patent read.

### 7.3 Candidate profile schema [R] (not patent text)
Per profile: input gain; four shelves {G_dB, mode}; compressors {threshold, ratio, attack, release} x2; eleven bands {gain_dB, optional Q index}; output gain; optional output gain-reduction threshold (9,350,309). Keep fc grid, Fs and the allpass/shelf alpha outside the profile (they are fixed by the chain).

---

## 8. Ambiguities and gaps, with recommended choices

Everything in the "Recommendation" column is **[R]**, not patent text.

| # | Gap or conflict | What the patent gives | Recommendation [R] |
|---|---|---|---|
| 1 | Sample rate | Fs symbol only | Make Fs a parameter; precompute all coefficients per Fs; 48 kHz / 44.1 kHz are the reference test rates here. Re-derive alpha, k2 on rate change. |
| 2 | Shelf gains | none for the chain; 12 dB cap and 10 dB/8 dB targets in sibling patents | Use G_lin = 10^(G_dB/20) with G in (0, 1]; pick first low shelf as a cut of about -10 dB DC gain and first high shelf as a boost of about +8 dB (mirror on the second pair), then tune by ear. Note that for a 1 kHz first-order shelf, -10 dB at 100 Hz requires about -10.4 dB at DC; -8 dB at 8 kHz requires about -8.3 dB at Nyquist (numerical solve, [N]). |
| 3 | Which shelf is cut vs boost | Not stated per stage | Low1 cut, High1 boost, Low2 boost, High2 cut, each partner pair sharing the same G (inference from 7,254,243 and the mirror claim). |
| 4 | High-shelf boost coefficient | Only low-shelf `wpt[1]` is defined | Use `2/((1+G) - alpha(1-G))` and the "+" sign on the `l` term, as in the printed high_shelf code (verified exact inverse, 3.4). |
| 5 | Boost-mode sign in printed equation/FIG. 9 | "+" in equation, "-" in code | Follow the code ("-" for low shelf, "+" for high shelf). |
| 6 | Shelf corner definition | alpha = (1 - sin theta)/cos theta, theta = 2 pi fc/Fs | Keep as printed; this puts the response at about -2.7 dB (cut, G = -12 dB) at fc, i.e. "-3 dB at the corner" is approximate. |
| 7 | k1/k2 names in the bell | Printed definitions give a non-resonant allpass | Use k1 = -cos(2 pi fc/Fs), k2 = (1 - tan(pi fc/(Q Fs)))/(1 + tan(pi fc/(Q Fs))), geq_b0 = k2, geq_b1 = k1(1+k2). |
| 8 | Bell branch weights | Printed H(z) and FIG. 7 put (1+G) on the allpass; code puts it on the direct path | Use the code: `0.5(1-G) ap + 0.5(1+G) x` (0 deg at fc). |
| 9 | Bell state update | Listing stores bell output instead of allpass output | Store the allpass output. |
| 10 | Band centres | 30 Hz and 16000 Hz endpoints, nine others "roughly" one octave | Choose log-uniform spacing of 11 points: 30, 56.4, 106, 199, 375, 705, 1327, 2497, 4698, 8839, 16000 Hz (ratio 1.88, 0.91 octave). Alternative: analog ten-band set 30, 60, 120, 250, 500, 1k, 2k, 4k, 8k, 16k plus an added 11th. Pick one and make it a table. |
| 11 | Band Q | "sixteen" Q values exist, none listed | Constant-Q: Q = 1/(sqrt(r) - 1/sqrt(r)) with r the neighbour ratio, about 1.4 to 1.6 for r = 2 to 1.88. Offer a Q table of 16 values; default one value. |
| 12 | Band gain range | none for the 11-band EQ (8,462,963's +18/-12 dB is a different EQ) | +/-12 dB per band is a conventional slider range; store G in dB, convert to linear. |
| 13 | Compressor level time constants | printed exp(+1/(Fs T)) | alpha = exp(-1/(Fs T)). Keep the factor 5 on the release only if you want to match the printed release law. |
| 14 | Compressor threshold / ratio / times | not given | Make these profile parameters. Threshold is linear (K_T = 10^(dB/20)); ratio r:1 with r >= 1; attack 1 to 10 ms-class and release 50 to 500 ms-class are conventional (not patent values). |
| 15 | Knee | none; code is hard knee (`level*invT > 1`) | Hard knee, as the code. |
| 16 | Gain computer | G_R = (L/K_T)^((1-r)/r), table with 100 points x 6 ratios | Compute with `powf` directly (or `exp2/log2`); the table exists only to save DSP cycles. If you keep a table, index it as `(j+1)^((1-r)/r)` and interpolate between neighbours; clamp above 100x. |
| 17 | Detector type | rectified one-pole smoother (digital); RMS (analog 2004) | Use the printed rectified one-pole smoother. |
| 18 | Lookahead and makeup | none | Zero lookahead, zero makeup in the chain; use output gain (stage 109) as the only makeup. |
| 19 | Compressor state sharing | function-level static | One level state per compressor per channel. |
| 20 | Stereo | not defined (one buffer; left/right state names only for shelves 3 and 4) | Process each channel with independent filter states; compressor detectors independent per channel by default, with an optional linked mode (max or average of channel levels) if image stability is a concern. |
| 21 | Numeric format | `float`; fixed point only mentioned | f32 for processing (f64 for coefficient computation). If fixed-point is required, treat it as a new design: the near-unit-circle allpass poles at low band centres (k2 = 0.996 at 30 Hz) are coefficient-sensitive. |
| 22 | Input/output gain | purpose only (avoid overflow); values none; 9,350,309 gain reduction above threshold | Input gain: a profile parameter in dB; output gain likewise; optional output ceiling per 9,350,309. |
| 23 | Block size | NSAMPLES unnamed | Any block size; results must be independent of block size (state carried between blocks). |
| 24 | Which compressor ratio index | both calls use `ratio1Index` | Give each compressor its own ratio. |
| 25 | Sign convention a1 | P34 vs P109 conflict | Use `y = b0 x + b1 x1 - a1 y1` with `H = (b0+b1 z^-1)/(1+a1 z^-1)`; for the shelves a1 = -alpha. |
| 26 | 7,254,243 mirror treble frequency | description 10 kHz vs claim 8 kHz | Not needed for the digital chain; ignore. |

Items that are internally inconsistent in the patent (3-5, 7-9, 13, 19, 25, 26) are flagged because a literal transcription of the printed text or code would produce wrong audio; items that are merely absent (2, 10-12, 14, 20-22) need design decisions or profile data.

---

## 9. Differences between family members that matter

- **8,229,136 vs 8,284,955:** the "digital signal processing method 100" description, equations and code are essentially identical (differences are layout, a few typos). 8,229,136's print of the compressor equations is cleaner (floor brackets present, `*` present); its level equation prints `|x[k]|` as on the page (the Google text shows `x[k]` without bars; the page image has the bars). 8,229,136 adds hardware FIG. 13/14 (separate device between head unit and speakers, ADC -> DSP -> PWM amplifier; 12 V supply; "Power supply 1428 provides both -3 volts and +30 volts"; "four amplifiers total") and a claim set about a "separate digital processing device located between a radio head unit and a speaker". 8,284,955 changes the output to a "personal audio listening device" driver and adds wireless/headphone content.
- **9,350,309** (continuation of 8,284,955 application): same description; claim 1 groups "first low shelf filter and a first high shelf filter" and "second low shelf filter and a second high shelf filter", and adds the output gain-reduction-above-threshold limitation.
- **8,462,963 / 8,472,642:** see sections 2.3, 3.5, 4.7, 5.7. 8,472,642 is the continuation of 7,254,243 with circuit FIGS. 2A-2E and "changing" wording in claims (e.g. claim 1 "changing an amplitude" in place of "reducing"/"increasing"), same numbers.

---

## 10. Test vectors for an implementation [N]

All computed from the formulas in this document (Fs = 48 kHz unless stated). Use as unit-test targets for a Rust port; they are not patent values.

**Shelves, fc = 1000 Hz** (alpha = 0.87697646 at 48 kHz, 0.86678844 at 44.1 kHz). With G = -10 dB (0.31622777): low-shelf cut `b0 = 0.95793996, b1 = -0.91903650, a1 = -0.87697646`; low-shelf boost `wpt1 = 1.04390676` (state update `s = alpha s + out`, `out = wpt1 (x - 0.5(1-G) l)`, `l = (alpha^2-1) s`); high-shelf cut `b0 = 0.35828781, b1 = -0.23526427`; high-shelf boost `wpt1' = 2.79105229` (`out = wpt1' (x + 0.5(1-G) l)`). Cascade cut -> boost with the same G must give a unit impulse (residual < 1e-12).

**Bell, fc = 1000 Hz, Q = 2:** `geq_b0 = 0.936602, geq_b1 = -1.920034`. With G = 12 dB: |H| = 12.000 dB at 1 kHz, 0.00 dB at DC and Nyquist, about 4 dB at 500 Hz and 2 kHz, phase 0 at 1 kHz. G -> 0: -3 dB edges near 782 Hz and 1279 Hz.

**Compressor static curve** (steady-state level equal to |x|, K_T = 0.1, r = 4): level 0.05 -> GR 1 (out 0.05); 0.10 -> 1 (0.10); 0.20 -> 0.59460 (0.11892); 0.40 -> 0.35355 (0.14142); 0.80 -> 0.21022 (0.16818). Output level = K_T (level/K_T)^(1/r).

**Smoother time constant sanity check:** alpha_att = exp(-1/(Fs Tatt)) = 0.99792 for Tatt = 10 ms at 48 kHz; the printed exp(+1/(Fs Tatt)) = 1.0000208 > 1.

---

## 11. Minimal reference algorithm [R] (my reconstruction, not patent text)

Rust-flavoured sketch of the chain with the corrections from sections 3.4, 4.5, 5.3. Not compiled.

```text
// per channel
input_gain (linear)
low1  = LowShelf  { fc=1000, G1, mode=Cut   }      // alpha = (1 - sin(2*pi*fc/fs)) / cos(2*pi*fc/fs)
high1 = HighShelf { fc=1000, G2, mode=Boost }
comp1 = Compressor{ kt, r, t_att, t_rel }
low2  = LowShelf  { fc=1000, G1, mode=Boost }      // same G as low1
high2 = HighShelf { fc=1000, G2, mode=Cut   }      // same G as high1
geq   = [Bell{fc_i, Q_i, G_i}; 11]                 // k2=(1-t)/(1+t), t=tan(pi fc/(Q fs)); k1=-cos(2 pi fc/fs)
comp2 = Compressor{ kt, r, t_att, t_rel }
output_gain (linear)
// order: input_gain -> low1 -> high1 -> comp1 -> low2 -> high2 -> geq[0..10] -> comp2 -> output_gain

Shelf::tick(x):
  cut:   y = a*x + (a*a-1)*s ; s = a*s + x ; return 0.5*((1+G)*x + sgn*(1-G)*y)    // sgn=+1 low, -1 high
  boost: l = (a*a-1)*s ; out = w1*(x + bsgn*0.5*(1-G)*l) ; s = a*s + out ; return out  // bsgn=-1 low, +1 high
         w1 = 2/((1+G) + a*(1-G)) low ;  2/((1+G) - a*(1-G)) high

Bell::tick(x):
  ap = b0*(x - y2) + b1*(x1 - y1) + x2        // b0=k2, b1=k1*(1+k2)
  x2=x1; x1=x; y2=y1; y1=ap
  return 0.5*(1-G)*ap + 0.5*(1+G)*x

Compressor::tick(x):
  ax = |x|
  level = if ax >= level { ax + a_att*(level-ax) } else { ax + a_rel*(level-ax) }
  e = level / kt
  gr = if e > 1 { e.powf((1-r)/r) } else { 1 }
  return x*gr                                   // a_att=exp(-1/(fs*t_att)), a_rel=exp(-1/(5*fs*t_rel)) to match printed form
```

---

## 12. Provenance log

Fetched with HTTP GET, generic User-Agent `Mozilla/5.0 (compatible; research-client)`, 3-6 s between requests:
`https://patents.google.com/patent/{US7254243B2,US8284955B2,US8462963B2,US8472642B2,US8229136B2,US8160274B2,US9350309B2,US9276542B2}/en` and the five main patents' PDFs from the "Download PDF" links on those pages (`patentimages.storage.googleapis.com/.../US7254243.pdf`, `US8284955.pdf`, `US8462963.pdf`, `US8472642.pdf`, `US8229136.pdf`). `patents.google.com/robots.txt` allows `/patent/`. No CAPTCHA, login, or rate-limit response was encountered.

The extraction tooling (HTML/MathML to text, PDF column splitter) and the Python verification scripts were scratch work and are not part of this repository.
