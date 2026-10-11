# M1 manual acceptance: live W1AW copy

Not part of CI (design doc §8). Run this yourself against real rig audio
before flipping CLAUDE.md's Status line to "M1 implemented".

## W1AW code practice schedule

W1AW's CW code practice runs on a published weekday/weekend schedule across
multiple HF bands (check <http://www.arrl.org/code-and-emergency-comms> or
ARRL's current bulletin for the live schedule -- it's shifted over the
years, don't assume a fixed time). Pick a session at a comfortable
copying speed for a first pass.

## Steps

1. Connect your rig's RX audio output to your computer's audio input
   (sound card line-in, USB audio interface, or a rig with built-in USB
   audio CODEC).
2. Tune to a W1AW code-practice frequency/time slot, confirm you can hear
   clean CW in your normal audio monitoring path first.
3. `cargo run --release -p manta-cli -- listen --device <your interface name>`
   (omit `--device` to use the system default input; run
   `cargo run -p manta-cli -- listen --device nonexistent` first if
   unsure what device names are visible -- the error message won't list
   them today, so cross-check via your OS's audio settings panel).
4. Watch the terminal. Decoded text arrives on stderr, one line per
   track, for example `[track 3 0.7 kHz 18 WPM] CQ CQ DE W1AW ...` (this
   runbook runs without `--dial-freq-hz`, so frequencies are audio
   offsets). A line appears at the first word gap after 32 characters, or
   when the track closes. Expected: readable text tracking the code
   practice transmission (callsigns, "CQ", punctuation-adjacent prosigns)
   --- not necessarily perfect, but clearly *recognizable*, matching
   ROADMAP's M1 bar.
5. Let it run at least several minutes to also eyeball basic stability
   (no panic, no runaway CPU/memory in Activity Monitor / htop).
6. Record the result (date, band, rough accuracy impression, any issues)
   in this file's "Runs" section below, or in a follow-up commit's message.

## Runs

(append entries here as you run this)
