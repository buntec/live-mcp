# Listening

The LiveMCP Ear is a VST3 plugin that measures the audio passing through it and
streams the results to `live-mcp serve`. `listen` plays part of the set and
returns what the Ears heard; `analyze_file` measures an audio file the same way.

## Setup

`live-mcp install` installs the Ear. If Live doesn't list it, open Settings →
Plug-Ins, turn on **Use VST3 Plug-In System Folders**, and click **Rescan**. The
plugin appears under Plug-Ins → live-mcp → LiveMCP Ear, and `load_device` loads
it with the path `["plugins", "live-mcp", "LiveMCP Ear"]`. After installing a
new version, restart Live: it keeps the old binary loaded.

Put an Ear wherever you want to listen: on the master track to hear the whole
mix, or on a single track. `ears` shows which tracks have one and whether each
is connected.

## What `listen` reports

`listen` plays `bars` bars from `start_bar` in real time and returns, for each
bar:
- loudness (LUFS, ITU-R BS.1770);
- peak and RMS (dBFS), and crest factor;
- energy in five bands (dBFS): low <120 Hz, low_mid 120–500 Hz, mid 500 Hz–2 kHz,
  high_mid 2–6 kHz, high >6 kHz;
- stereo correlation and width, and clipped samples;
- key, chords, bass, pitch classes, rhythm and range.

It also returns a summary of the whole section. `focus` narrows the output:
`"mix"` for levels, bands and stereo only, `"music"` for the musical fields
only. [Hearing harmony and rhythm](harmony.md) explains how those are detected.

`analyze_file` returns the same measurements for an audio file, plus gated
integrated loudness and loudness range.

## Snapshots and `compare`

Every `listen` and `analyze_file` result is kept for the server session as a
snapshot (`s1`, `s2`, …). `compare` reports how two of them differ, optionally
for given bar ranges ("my bars 9–16" against "the reference's bars 33–40"):
- loudness, peak, crest (dynamics), width and correlation;
- **tonal balance with levels matched** on the median band, so a change in one
  band shows only in that band;
- plain-language notes on the differences that matter;
- bar-by-bar differences when both selections have the same length.

## Several tracks and masking

`listen` with `tracks` (a list, or `["all"]`) listens through several Ears in
one playback. It returns each track's bars, each kept as its own snapshot, plus
a **masking analysis**:
- each track's share of every frequency band;
- the bands that two or more tracks each carry at least a quarter of (−6 dB),
  such as bass and kick both in the low band.

An Ear at the end of a track's device chain hears the signal before the fader,
so each track's level is corrected by its volume and by the volume of every
track its audio passes through on the way to Main: its groups, or a track it is
routed to. A track routed straight to Main skips its group's fader. Tracks are
left out, each with the reason, when they or a track they play through are muted
or silenced by solo (as Live reports it, whatever the solo mode), or when their
output does not reach Main.

No signal is counted twice. A group track's Ear gives way to its tracks' Ears
when they cover everything audible in the group; otherwise the group's Ear
counts, and the Ears of tracks playing through it do not. An ordinary track that
others are routed into always counts in place of theirs, since it can play clips
of its own.

Sends reach the mix through return tracks: put an Ear on a return to count its
reverb or delay as a track of its own. Pan is not taken into account yet: bands
are measured on the mid signal, as if every track were centred, so tracks panned
apart mask each other less than the analysis reports.

## How it works

- The plugin measures audio on the audio thread without allocating (about 0.12%
  of one core). It streams reports to `live-mcp serve`, which listens on
  127.0.0.1:19419 and writes the actual port to `~/.live-mcp/ear.json`. Only the
  first server to start gets the port.
- The plugin's **Tag** parameter is set by the server through the Live API, and
  tells the server which track each plugin instance is on. Leave it alone.

## Live's transport, as measured on 12.4

- `start_playing` always starts at the song start.
- `continue_playing` resumes where playback last *stopped*. A position set while
  stopped is shown, but ignored.
- Setting `current_song_time` while playing jumps there.
- All of these take effect on a later tick: `is_playing` and `current_song_time`
  read stale until then.

So `listen` and `transport` start playback at a position by starting playback,
waiting a tick, then jumping. Up to one tick (about 0.1 s) of audio from the
song start can be heard first.
