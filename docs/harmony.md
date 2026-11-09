# Hearing harmony and rhythm: design

Status: phases 1–4 implemented on branch `hearing-harmony`, 2026-11-07. The
proposal is kept below; [As built](#as-built) lists where the implementation
differs and why.

## Goal

Give the agent musical perception of what actually sounds, so it can help with
composition: key, chords, bass line, rhythm, register, clashes between parts and
fit with the set's scale. This is about *rendered* audio. Synth patches, chord
and arpeggiator devices, pitch effects, samples and the sum of all layers are
invisible in MIDI clip data.

Non-goals: audio-to-MIDI transcription, exact melody extraction, and research-grade
chord recognition on full mixes. Those can come later as an on-request tool (see
[Later](#later)).

## Principles

1. **The plugin measures, the server interprets.** The Ear emits fixed-size,
   low-level features per beat. All labels (key, chords, clashes) are computed in
   the server. Interpretation can then improve without users reinstalling the
   plugin, can be tested offline, and is shared by `listen`, `analyze_file` and
   MIDI clips.
2. **Labels come with evidence.** Every chord or key comes with a confidence and
   the pitch classes behind it. A model reasons well from "A, C, E strong, G weak"
   and is misled by a confident wrong label.
3. **Compact, bar-aligned output.** One short entry per bar, plus a section
   summary such as `Am7 | F | C G | G`. Tokens are the agent's budget.
4. **Real-time safety is unchanged.** Nothing new allocates on the audio thread,
   and `tests/no_alloc.rs` covers the new code.

```
 Ear plugin (audio thread, no alloc)                 MCP server
 ┌─────────────────────────────────────┐   TCP     ┌──────────────────────────────────┐
 │ BarAnalyzer                         │  JSON     │ harmony::analyze                 │
 │  └─ SignalMeter  → BarReport  ──────┼──────────▶│  tuning → chroma12 → key, chords,│
 │ BeatAnalyzer                        │           │  pitch classes, rhythm, register,│
 │  ├─ PitchMeter   ┐                  │           │  scale fit; harmony::ensemble:   │
 │  └─ OnsetMeter   ┴→ BeatReport ─────┼──────────▶│  together, clashes, crowding     │
 └─────────────────────────────────────┘           │         ▲              ▲         │
                                                   │ analyze_file      read_clip      │
                                                   │ (same meters)   (notes→chroma)   │
                                                   └──────────────────────────────────┘
```

## As built

Where the implementation departs from the proposal below, and why. Most changes
came from running real and synthesized audio through the whole path.

**Plugin (`crates/analysis`: `pitch.rs`, `onset.rs`, `spectrum.rs`, `beats.rs`)**

- `BeatAnalyzer` is separate from `BarAnalyzer` and follows the transport itself,
  so the bar code and its API are unchanged. It costs about 0.05% of a core at
  48 kHz (`examples/throughput.rs`) and never allocates (`tests/no_alloc.rs`).
- `BeatReport` stores chroma as `[[u8; 3]; 12]` (per pitch class: a third of a
  semitone flat, in tune, sharp), because serde derives only arrays up to 32.
- The octaves are Live's octaves 0–6 (C0 = MIDI 24 … B6), and note names
  everywhere follow Live (C3 = MIDI 60).
- The treble chroma starts at MIDI 52 (E2 in Live, 165 Hz), not 48, so most bass
  fundamentals stay out of the chord evidence. The bass chroma is tilted towards
  low notes (weight 1 at MIDI 24, falling to 0.3 at MIDI 59), so that the lowest
  note of a voicing beats chord tones above it in the bass range.
- Onsets come from their own short full-rate STFT (~21 ms window, ~5 ms hop), not
  from the band filters. The STFT is cheaper and detects attacks better. Flux is
  averaged over 30 Hz–6 kHz only (pitched attacks were diluted by many empty high
  bins) and placed on the *nearest* 16th: flux peaks as an attack enters the
  window, ~10 ms before the frame centre, which floored onto the previous 16th.

**Server (`crates/server/src/harmony/`)**

- Key profiles are Temperley's (Kostka–Payne). Krumhansl–Kessler called a
  Dm7–G7–Cmaj7–A7 loop G major with 0.9 confidence, because it barely penalises
  the F natural. A key also needs at least three pitch classes.
- Tuning comes from parabolic interpolation of the peaks in the 36 treble bins,
  then a circular mean. A plain circular mean over all bins was too flat after log
  compression to say anything.
- Folding to 12 pitch classes assigns each sub-bin to its nearest semitone.
  Interpolating between two semitones smeared spectral leakage onto neighbours
  and gave a single sine a key.
- Each beat's chroma has its median taken out, and a beat counts as pitched only
  if its strongest pitch class is at least 1.6 × the mean. Swept 808 kicks and
  noise otherwise came out as "Dm7b5". Subtracting the mean instead of the median
  pushed triads towards looking like single notes.
- In audio, a root's harmonics resemble a major triad. So a single note must beat
  every chord by 0.08 before a beat is called "N", and power chords are
  handicapped by 0.03. MIDI input stays exact.
- Audio onsets below 12 (of 255) are ignored: steady tones flutter a little.
- `pcs` is a compact string ("A 1.0 C 0.8 E 0.7"), ordered by weight; a JSON
  object would come back sorted by name.
- `read_clip` on a Drum Rack track reports rhythm only. A clip's bars count from
  its own beat 0.
- `listen` takes `focus` ("all", "mix", "music"). Multi-track listens add a
  `music` section for the counted tracks together (`ensemble.rs`): their key and
  progression, `clashes`, and `crowded` octaves. The combined bass line is the
  lowest note of any track within 20 dB (after faders) of the loudest pitched one,
  so a pulled-down fader cannot decide the inversion.
- `analyze_file` takes `music` (default true). Without a bpm, chords are per
  quarter of a section and no rhythm grid is given. Snapshots keep their musical
  reading and beat reports, and `compare` adds key, progression and per-bar chord
  (with where each chord starts) and rhythm differences. For part of a snapshot,
  key, tuning and rhythm are heard again from just the selected bars.

**Bass line (protocol 3).** Chroma cannot name bass notes: a low note's harmonics
outweigh it, and a third of a semitone at 40 Hz is narrower than any usable FFT bin.
`bassline.rs` tracks the fundamental with YIN on the mid signal low-passed at
350 Hz and decimated to about 3 kHz (128-sample window, a frame every ~11 ms,
30–340 Hz). Each `BeatReport` carries, per 16th, the winning note, its cents and a
confidence (`bass_notes`, `bass_cents`, `bass_confidence`; empty from protocol 2
plugins). That roughly doubles the beat analysis cost, to about 0.09% of a core.
The server drops unsure 16ths, fixes octave slips (where a kick lands on a new
note the tracker often reads an octave low), and uses the notes for the bar's
`bass`, for inversions, for the key profile, and for the `pcs` and `range` of bass
lines. Each bar gets a `bass_line` on the 16th grid: a note where one starts, `-`
held, `.` none. A repeated note on a weak onset reads as held, because pluck decays
also produce weak flux. MIDI clips get the same field from their lowest sounding
notes. A beat with a confident bass line and less than 0.22 of its strongest
octave's energy above C3 (real bass loops: median 0.01–0.18; chordal loops from
about 0.7) is a bass line, with no chord; neighbouring beats vote, so labels don't
flicker. On the master, kicks and low pads overlap the bass, so its line gets
sparser there than on the bass track's own Ear.

Re-recording the bass loops through the new Ear showed three more things, now
handled. Chords and bright tones make YIN report the residue pitch, the period the
partials share, an octave or two below the lowest real note (a guitar's A read as
A0, its B as B-1). Notes are lifted while their octave holds almost no energy and
the one above holds more. Bass notes are re-rounded against the section's tuning.
And correlation with key profiles cannot name the tonic of a bass line that leaves
out the deciding note (a D minor line with no F fits G minor as well), so the
pitch-class profile weights bass notes by metric position, and each key gets a
bonus for the share of the bass on the first beat of each bar that sits on its
tonic. On ten bass and bass-like loops: the bass notes now match the labelled keys'
tonics, loops of a single repeated note get no key (correctly), and 3 of the 7
loops with several notes get the labelled key, up from 1 of 10 before the tracker.

**Validation.** Unit tests on synthesized signals, including plucked sawtooth
bands with kick and hats at +20 cents (Am–F–C–G) and in tune (Dm7–G7–Cmaj7–A7).
End-to-end tests for `read_clip`, `listen` (single and multi-track, clashes) and
`analyze_file` with `compare`.

On real audio, 36 labelled loops from Live's Loopmasters Mixtape pack were played
through the Ear (the pack's files decode only inside Live), recorded with
`record_beats`, and scored offline (now with `harmony/testset.rs`):

- Chords: 38 of 40 right on the ten string loops whose names list their chords.
  Misses were Am heard as Cmaj7 (a held B over an A bass, at confidence 0.25) and
  D as Bm/D.
- Keys: right for chordal loops (the string loops, guitars, piano, a pad, a lead
  with chords), mostly wrong for single lines, bass lines and one-note hooks,
  where the loop alone underdetermines the key (the label is the construction
  kit's key). Confidence mostly tracks this: in these runs every wrong key was at
  0.61 or below and every key at 0.9 or above was right, but the bass-loop re-run
  below has a wrong key at 0.93.
- Telling pitched beats from drums needed the in-tune peak measure (see
  `audio.rs`): chroma contrast was the same for drum loops and for pads and
  pianos.
- Bass loops, re-run with the bass-line tracker (nine loops, four of them from
  the earlier runs, which gave the same results again). The bass line was right
  on every one, including a walking line tracked note for note. Keys: one-note
  loops get no key (correctly), and a loop labelled D# that plays only a square-wave
  Ab0 gets none either (the label is the kit's key). Of the five with several
  notes, one gets the labelled tonic and one has it as the alternative. The others
  are wrong: G major at 0.45 and E minor at 0.84 for two A minor lines, and
  Bb major at 0.93 for an F walking bass whose Eb fits Bb major's scale, though F
  is on every downbeat.
- Multi-track: a one-note G bass under the Am–C–G–F string loop turns the combined
  chords into G inversions (Cmaj7/G, G7) at 0 dB; at −30 dB the combined reading
  is the strings' own (Am/C | C | Gsus4 G | F Fsus2), with no clash.

**The test set.** `crates/server/testdata/hearing-set.json` lists 147 labelled
loops, and `harmony/testset.rs` scores recordings of them (how to record and
score is at the top of that file). 115 come from Live's packs: the loops above,
with fuller chord labels from their file names, and 75 more across piano, organ
and electric piano, synths, pads, guitars, strings, choirs and vocals, horns,
bells, bass, drums (labelled "no key") and ten single chords, 13 of them in major
keys. A key from a file name is marked as the construction kit's, which the loop
may not be in. 32 are rendered: MIDI progressions written by `testset.rs` and
played through eight of Live's instruments, so their chords, keys and bass are
exact. They cover major and minor keys, sevenths, sus and diminished chords,
inversions, two chords per bar, sustained chords, stabs and arpeggios, 3/4, 6/8,
5/8, 7/8 and 3/8, and three without a bass part. Each loop is in a "tune" or a
"holdout" split; the earlier loops, which the interpreter was tuned on, are all
"tune". The same rendered loops read from their MIDI give a ceiling for what can
be heard of them.

First scores (2026-11-09), per beat for chords and per 16th for the bass:

| | loops | key | chord root | root and third | bass line |
| --- | --- | --- | --- | --- | --- |
| Pack loops | 115 | 21/52 where the label is the loop's own; 14/43 the kit's | 87% | 77% | - |
| Rendered, heard | 32 | 24/30 | 89% | 87% | 46% pitch class, 26% exact note |
| Rendered, from MIDI | 32 | 22/30 | 100% | 100% | 100% |

What this shows:

- Keys are the weakest part, even on exact input: from their MIDI, 8 of 30
  rendered progressions get the wrong key (C G Am F is heard in F major, Eb Cm Ab
  Bb in Ab major). The pack loops get 40% where the label is the loop's own key.
- Chords are good on sustained sounds: pads, electric piano, guitar and brass are
  at 88–100% of beats by root. They fail on a drawbar organ, whose harmonics add
  sevenths and move roots (C/E heard as Em7, Bdim as Bm7), and on a pluck
  arpeggio in the bass tracker's range, which is taken for a bass line and given
  no chord. Low chords on a piano and palm-muted guitar chugs are read as bass
  lines too, and a chord that has decayed (piano, pluck) reads as "N".
- The bass line under chords is poor, though a bass loop alone is tracked well:
  octave errors, a note heard at two thirds of its frequency (A1 as D1), and many
  16ths with no note when the bass decays or is short.
- Sus chords are called too readily in sparse textures, and loops tuned a quarter
  tone off (±48 cents) fall between two keys. Of four drum loops, three get no
  key; one with tuned kicks gets F# minor at 0.31.

Known weak spots: heavily filtered or pitch-bending synths, chord changes where
sustained notes overlap, and single-note hooks, where the loop alone does not
determine the key. Keys from a bass line alone remain weak: the downbeat bonus
does not outweigh a strong scale fit, as with the F walking bass heard in Bb
major at 0.93. The test set adds organ-like timbres, decaying chords, mid-register
arpeggios and the bass line under chords.

Meters with an odd number of eighths (3/8, 5/8, 7/8): pitch is measured per
quarter note, so a bar line inside a quarter puts that quarter in both bars. The
later bar keeps its notes, but the earlier one may also list them in `pcs`,
`range` and its chords. The bass line and rhythm grid are per 16th and split exactly.

## 1. Plugin features (`crates/analysis`)

Two new meters, `pitch.rs` (`PitchMeter`) and `onset.rs` (`OnsetMeter`), driven by
`BarAnalyzer` next to `SignalMeter`. They share its transport handling: position,
jump detection and flush on stop.

### Pitch path

- Mid signal (L+R)/2 → LR4 low-pass at ~4.5 kHz (`Cascade2`) → decimate by
  `D = max(1, round(sr / 12 kHz))`. That is 4 at 44.1/48 kHz and 8 at 96 kHz.
- Every `H = 1024` decimated samples (~85 ms): Hann window, `N = 4096` (~340 ms,
  ~2.9 Hz bins), real FFT with `realfft` and scratch buffers allocated in `new()`.
- Log-frequency filterbank, precomputed sparse triangular weights, to
  1/3-semitone bins from MIDI 28 (E1) to 96 (C7), with log compression
  `ln(1 + γ·mag)`.
- From those bins:
  - **treble**: MIDI 48–96 folded to 36 bins (3 per semitone, the centre bin at
    equal temperament with A = 440 Hz);
  - **bass**: MIDI 28–60 folded the same way;
  - **octaves**: energy per octave, C1…C7 (7 values), for register;
  - **tonal**: 1 − spectral flatness over 100 Hz–4 kHz, from 0 (noise or drums)
    to 1 (clearly pitched).
- Each frame belongs to the beat that contains its centre, i.e. the sample position
  minus `N·D/2`. Per beat, frames are summed weighted by energy.

Resolution: at ~2.9 Hz the bins separate semitones from about A1 (55 Hz) upward,
and third-semitones (needed for tuning) from about A3. Lower bass notes are
detected through their harmonics, which fall in the bass range. A longer window
would blur chord changes at normal tempos, so 340 ms is the compromise.

### Onset path

There is no extra FFT. `SignalMeter` already runs five band filters on the mid
signal. `OnsetMeter` takes their outputs over 5 ms sub-blocks and computes log
band energy, then the positive difference summed over bands. That is multi-band
spectral flux. Each beat keeps the maximum per 16th-note slot. This needs a small
refactor so `SignalMeter::feed` hands per-sample band values to the onset meter.

### `BeatReport`

```rust
pub struct BeatReport {
    /// 0-based beat (quarter note) from the song start; bar mapping uses the bar's start_beat.
    pub beat: i64,
    pub coverage: f32,
    /// RMS of the mid signal during the beat, dBFS.
    pub level_db: f32,
    /// Energy-weighted mean tonalness: 0 = noise/drums, 1 = clearly pitched.
    pub tonal: f32,
    /// Chroma shapes, 3 bins per semitone starting at C, normalised so the beat's maximum is 255.
    pub treble: [u8; 36],
    pub bass: [u8; 36],
    /// Energy per octave C1..C7, maximum = 255.
    pub octaves: [u8; 7],
    /// Onset strength per 16th, on a fixed log scale so beats and tracks are comparable.
    pub onsets: [u8; 4],
}
```

The struct is `Copy`, ~100 bytes, and roughly 450 bytes as JSON. That is under
1 KB/s per Ear at 120 BPM. Quantising to `u8` keeps messages small, and
1/255 is ample precision for chroma shapes.

A beat is reported once the frame whose centre lies past the beat's end has been
analysed, about 250 ms after the beat ends. On stop or jump, pending beats are
flushed with partial coverage, as bars are now.

CPU: ~12 FFTs of 4096 points per second plus a sparse filterbank. That should
stay well under 0.1% of a core. Extend `examples/throughput.rs` to check, with a
budget of under 1% of a core for the whole Ear at 48 kHz.

## 2. Protocol

- Add `EarMessage::Beat { tag, report: BeatReport }` and raise
  `EAR_PROTOCOL_VERSION` to 2.
- Today the server silently drops a connection whose protocol doesn't match
  (`crates/server/src/ear.rs:99`). Change it to accept protocols 1 and 2. A v1
  Ear keeps working for the mix measurements, and `ears` reports
  `"harmony": false` with a hint to reinstall the plugin.
- `listen` already waits until every wanted bar is complete before stopping.
  Its completion check must also wait for that bar's beat reports, which adds
  ~0.25 s.

## 3. Interpretation (server, `crates/server/src/harmony/`)

These are pure functions over `&[BeatReport]` (plus bar boundaries and optional
context), so they are unit-testable and shared by every source.

1. **Tuning.** Circular mean of the treble bins' position within the semitone,
   weighted by energy, gives an offset in cents in [−50, 50). Fold 36 → 12 bins
   shifted by that offset. Report it when |offset| > 10 cents.
2. **Gating.** A beat counts as pitched if `tonal ≥ 0.3` and `level_db ≥ −50`
   (both to be tuned). Otherwise its chord is `N` (no chord).
3. **Key.** Sum the energy-weighted chroma over the section, with bass weighted
   somewhat higher, and correlate it with the 24 rotated key profiles. Use Albrecht &
   Shanahan or Temperley, whichever tests better. Confidence comes from the margin
   between the best and second-best key. Report relative major/minor as ambiguous
   when they're close. For local keys, use an 8-bar sliding window and report a
   modulation only when another key wins clearly for at least 4 bars.
4. **Chords.** Vocabulary: maj, min, dim, aug, sus2, sus4, 5, 7, maj7, m7, m7b5,
   and N. The per-beat score is the cosine of chroma against each template, plus a
   bonus when the strongest bass pitch class is the root. A non-root chord tone in
   the bass gives a slash chord. Smooth over beats with Viterbi: a high probability
   of staying on the same chord, a slight preference for changes on strong beats,
   and a weak preference for chords in the detected key (weak, so borrowed chords
   still show). Merge beats into segments, each with a confidence (softmax over
   template scores) and alternatives within a small margin.
5. **Pitch-class evidence.** Per bar, the pitch classes at ≥ 25% of the bar's
   maximum, with weights, spelled to suit the key (sharps or flats).
6. **Rhythm.** Per bar, a 16th grid (`beats × 4` slots) as a string: `X`
   strong, `x` medium, `.` none, with thresholds taken from percentiles of the
   section's onsets. Also density (onsets per beat), a syncopation score
   (onset strength on weak metrical positions, Longuet-Higgins & Lee weights), and
   repetition (`A A A B`) from similarity between bar grids.
7. **Register.** Per bar, the octave range carrying the energy (e.g. `C2–C4`).
   With several tracks, crowded octaves are those where three or more tonal tracks
   carry ≥ −6 dB of the octave, the same idea as `CARRY_DB` in `masking.rs`.
8. **Scale fit.** Read the set's scale from Live 12 (`live_set` `root_note`,
   `scale_name`, `scale_intervals`; check them with `describe`, and skip this
   step when they're missing). Report out-of-scale pitch classes above a weight
   threshold and the bars they occur in, and whether the detected key agrees with
   the set's scale.
9. **Clashes** (multi-track `listen`). Count tracks the way `masking.rs` does
   (weighted by fader gain, muted or solo-silenced tracks left out). Per beat, for
   every pair of tracks: pitch class *p* strong in one (≥ 0.5 of its beat maximum)
   and *p ± 1* strong in the other, both above the level threshold, in overlapping
   octaves, and not both in that beat's chord template. Exclude tritones, since
   they are normal in dominant chords. Merge consecutive beats and report e.g.
   `bars 5–6: Pad F against Bass E`.

## 4. `listen` output

Add a `focus` parameter: `"all"` (default), `"mix"` (today's output) or
`"music"`. With harmony, each bar gains a few fields and the section gains a
`music` summary:

```json
{
  "music": {
    "key": {"name": "A minor", "confidence": 0.83, "alt": "C major"},
    "scale_fit": {"set_scale": "A Minor", "outside": []},
    "tuning_cents": 12,
    "progression": "Am7 | Fmaj7 | C G | G",
    "rhythm": {"pattern": "A A A B", "density": 2.1, "syncopation": 0.34}
  },
  "bars": [
    {"bar": 1, "lufs": -14.2, "...": "...",
     "chord": "Am7", "confidence": 0.71, "alt": ["C6"],
     "bass": "A", "pcs": {"A": 0.9, "C": 0.8, "E": 0.7, "G": 0.4},
     "rhythm": "X..x..x.x...x...", "tonal": 0.8, "range": "A1–E4"},
    {"bar": 3, "chords": [{"beat": 1, "chord": "C"}, {"beat": 3, "chord": "G/B"}], "...": "..."}
  ]
}
```

A bar whose chord changes lists `chords` with beat positions in place of `chord`.
Multi-track `listen` adds per-track `music`, `clashes` and crowded `register`.
Above a bar limit (like `MAX_LISTED_BARS`), per-bar music detail is summarised.
The aim is at most ~60 extra tokens per bar.

## 5. Snapshots and `compare`

`Snapshot` stores `beats: Vec<BeatReport>` next to `bars`. `compare` reports
musical differences: key or tuning changes, chords that differ per bar, rhythm
grid changes, and new or resolved clashes. This makes "before vs after my edit"
work for composition, not only for mixing.

## 6. `analyze_file`

Run the same meters over the decoded file. With `bpm`, use the synthetic
transport that file analysis already builds for `BarAnalyzer`. Without it, use
0.5 s slots as "beats", report sections in seconds and leave out the rhythm grid.
Reference tracks are full mixes with drums, so chord confidence will be lower;
the output shows that rather than hiding it.

## 7. MIDI clips (`read_clip`)

Build the same per-beat input directly from notes: chroma weighted by velocity
and duration, exact octaves, `tonal = 1`, onsets from note starts. Then run the
same interpreter. The agent gets the same vocabulary for "what's written" and
"what's heard", and mismatches mean something, e.g. a patch adding a fifth, or a
chord device. MIDI input is also exact, which makes it the best test bed for the
interpreter.

## 8. Testing

- **Meters:** synthesised signals in unit tests, following the patterns in
  `bars.rs`. Cases: sawtooth triads and sevenths at known pitches, an inversion
  with a separate bass, +30 cent detune, a chord change mid-bar, noise bursts on
  16ths for onsets, white noise (tonal ≈ 0), tempo changes and loop jumps.
- **Realtime:** extend `tests/no_alloc.rs` to the new meters, and
  `examples/throughput.rs` for CPU.
- **Interpreter:** hand-built `BeatReport`s and MIDI clips with known keys,
  progressions and clashes.
- **End to end:** `listen_e2e.rs` already fakes an Ear; add `Beat` messages to it.
- **Real material:** a handful of your own loops with hand-written labels, and an
  `xtask` command that runs `analyze_file` and scores the results. Rough targets:
  the right key on ≥ 90% of tonal loops, and the right chord per bar on ≥ 75% of
  isolated-instrument loops. Record the misses rather than tuning them away.

## 9. Phases

1. Interpreter plus MIDI analysis in `read_clip`. Exact input, no plugin changes.
2. Plugin pitch and onset meters, protocol v2, single-track `listen` output.
3. Multi-track: clashes, register, scale fit.
4. `analyze_file` and `compare`.

## Later

- **Separating harmonic from percussive sound** in the plugin, by median-filtering
  the spectrum over time and frequency, for better chords on the master and on
  drum-heavy buses. It adds ~0.35 s to when features arrive, but no audio latency.
- **Tempo and beat tracking** for files without `bpm`, from the onset function.
- **Basic Pitch on request**, for the exact notes of one track or file, e.g. to
  write a counter-melody against a vocal sample.

## Open questions

- The Live 11 fallback when the scale properties are missing: skip scale fit, or
  use the detected key.
- Whether the interpreter belongs in the server or in a new `live-mcp-harmony`
  crate, if anything else should reuse it.
- Spelling notes (sharps vs flats) in ambiguous keys and in chromatic passages.
- Default values for the tonal, level and clash thresholds. Tune them against real
  loops in phase 2.
