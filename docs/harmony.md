# Hearing harmony and rhythm: design

Status: proposal, 2026-11-07.

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
 │ BarAnalyzer                         │  JSON     │ harmony::interpret               │
 │  ├─ SignalMeter  → BarReport  ──────┼──────────▶│  tuning → chroma12 → key, chords,│
 │  ├─ PitchMeter   ┐                  │           │  pitch classes, rhythm, register,│
 │  └─ OnsetMeter   ┴→ BeatReport ─────┼──────────▶│  scale fit, clashes              │
 └─────────────────────────────────────┘           │         ▲              ▲         │
                                                   │ analyze_file      read_clip      │
                                                   │ (same meters)   (notes→chroma)   │
                                                   └──────────────────────────────────┘
```

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
