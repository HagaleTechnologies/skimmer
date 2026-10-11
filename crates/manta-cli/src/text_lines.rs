//! `run`'s plain-text decoded output, grouped into one line per track
//! (MAN-123). Every track's characters are buffered separately and printed
//! as one labelled line, so hundreds of concurrent tracks no longer splice
//! into a single stream.

use manta_decode::events::DecoderEvent;
use std::collections::BTreeMap;

/// A line is printed at the first word gap once it holds this many characters.
pub(crate) const LINE_MIN_CHARS: usize = 32;
/// A track that sends no word gap is cut here, mid-word.
pub(crate) const LINE_MAX_CHARS: usize = 64;

#[derive(Default)]
struct PendingLine {
    text: String,
    chars: usize,
    freq_hz: Option<f64>,
    wpm: Option<f32>,
}

/// Per-track line buffers. I/O-free: `run` decides where the lines go.
#[derive(Default)]
pub(crate) struct TrackLines {
    tracks: BTreeMap<u32, PendingLine>,
}

impl TrackLines {
    /// Feeds one event; returns the line it completes, if any. An event only
    /// ever touches its own track, so at most one line comes back.
    pub(crate) fn ingest(&mut self, ev: &DecoderEvent) -> Option<String> {
        match ev {
            // No state: a bare promotion never gets a TrackClosed (MAN-19's
            // has_emitted filter), so an entry made here would never be freed.
            DecoderEvent::TrackPromoted { .. } => None,
            DecoderEvent::TrackClosed { track_id, .. } => {
                let line = self.tracks.remove(track_id)?;
                render(*track_id, &line)
            }
            DecoderEvent::CharDecoded {
                track_id, glyph, ..
            } => {
                let line = self.tracks.entry(*track_id).or_default();
                line.text.push(glyph.text_char()?);
                line.chars += 1;
                if line.chars >= LINE_MAX_CHARS {
                    return take(*track_id, line);
                }
                None
            }
            DecoderEvent::WordBoundary { track_id, .. } => {
                let line = self.tracks.entry(*track_id).or_default();
                if line.chars == 0 || line.text.ends_with(' ') {
                    return None;
                }
                if line.chars >= LINE_MIN_CHARS {
                    return take(*track_id, line);
                }
                line.text.push(' ');
                line.chars += 1;
                None
            }
            DecoderEvent::SpeedUpdate { track_id, wpm } => {
                self.tracks.entry(*track_id).or_default().wpm = Some(*wpm);
                None
            }
            DecoderEvent::TrackMeta {
                track_id, freq_hz, ..
            } => {
                self.tracks.entry(*track_id).or_default().freq_hz = Some(*freq_hz);
                None
            }
        }
    }

    /// Every pending line, in track order, and forgets all tracks. For the
    /// end of the decode loop: a source read error returns from `listen`
    /// without closing its tracks.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        std::mem::take(&mut self.tracks)
            .into_iter()
            .filter_map(|(id, line)| render(id, &line))
            .collect()
    }

    #[cfg(test)]
    fn pending_tracks(&self) -> usize {
        self.tracks.len()
    }
}

/// Renders the track's line and empties its buffer; the label fields stay.
fn take(track_id: u32, line: &mut PendingLine) -> Option<String> {
    let out = render(track_id, line);
    line.text.clear();
    line.chars = 0;
    out
}

/// `[track <id>[ <kHz> kHz][ <wpm> WPM]] <text>`, or `None` for no text.
fn render(track_id: u32, line: &PendingLine) -> Option<String> {
    let text = line.text.trim_end();
    if text.is_empty() {
        return None;
    }
    let mut label = format!("track {track_id}");
    if let Some(hz) = line.freq_hz {
        label.push_str(&format!(" {:.1} kHz", hz / 1000.0));
    }
    if let Some(wpm) = line.wpm {
        label.push_str(&format!(" {wpm:.0} WPM"));
    }
    Some(format!("[{label}] {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_decode::decoder::events_to_text;
    use manta_decode::events::ClosureKind;
    use manta_decode::tree::{Glyph, Prosign};

    fn ch(id: u32, ts: u64, c: char) -> DecoderEvent {
        DecoderEvent::char_decoded(id, ts, Glyph::Char(c), 1.0)
    }

    fn wb(id: u32, ts: u64) -> DecoderEvent {
        DecoderEvent::word_boundary(id, ts)
    }

    /// The char/word-boundary sequence for `s`: one `WordBoundary` per
    /// space, none before the first word or after the last.
    fn text(id: u32, s: &str) -> Vec<DecoderEvent> {
        s.chars()
            .enumerate()
            .map(|(i, c)| {
                if c == ' ' {
                    wb(id, i as u64)
                } else {
                    ch(id, i as u64, c)
                }
            })
            .collect()
    }

    fn meta(id: u32, freq_hz: f64) -> DecoderEvent {
        DecoderEvent::TrackMeta {
            track_id: id,
            sample_ts: 0,
            snr_2500_db: 20.0,
            freq_hz,
        }
    }

    fn speed(id: u32, wpm: f32) -> DecoderEvent {
        DecoderEvent::SpeedUpdate { track_id: id, wpm }
    }

    fn closed(id: u32) -> DecoderEvent {
        DecoderEvent::TrackClosed {
            track_id: id,
            closure: ClosureKind::SignalEnded,
        }
    }

    fn promoted(id: u32, freq_hz: f64) -> DecoderEvent {
        DecoderEvent::TrackPromoted {
            track_id: id,
            sample_ts: 0,
            freq_hz,
        }
    }

    /// Every line `events` completes, in order.
    fn feed(lines: &mut TrackLines, events: &[DecoderEvent]) -> Vec<String> {
        events.iter().filter_map(|ev| lines.ingest(ev)).collect()
    }

    #[test]
    fn interleaved_tracks_come_out_as_separate_lines() {
        let mut one = vec![meta(1, 14_060_582.6), speed(1, 20.0)];
        one.extend(text(1, "CQ CQ DE W1AW W1AW K CQ CQ DE W1AW"));
        let mut two = vec![meta(2, 14_061_301.8), speed(2, 24.0)];
        two.extend(text(2, "CQ CQ DE K9ABC K9ABC K CQ CQ DE K9ABC"));
        let mut events = Vec::new();
        for i in 0..one.len().max(two.len()) {
            events.extend(one.get(i).cloned());
            events.extend(two.get(i).cloned());
        }
        events.push(closed(1));
        events.push(closed(2));

        let mut lines = TrackLines::default();
        let out = feed(&mut lines, &events);
        // Track 1's only word gap past 31 characters never comes (the line
        // is 34 long and ends without one); track 2's last gap falls at 31.
        // So each track's whole text arrives at its close, as one line.
        assert_eq!(
            out,
            vec![
                "[track 1 14060.6 kHz 20 WPM] CQ CQ DE W1AW W1AW K CQ CQ DE W1AW",
                "[track 2 14061.3 kHz 24 WPM] CQ CQ DE K9ABC K9ABC K CQ CQ DE K9ABC",
            ]
        );
        assert!(out
            .iter()
            .all(|l| !(l.contains("W1AW") && l.contains("K9ABC"))));
        assert_eq!(lines.pending_tracks(), 0);
    }

    #[test]
    fn a_line_is_printed_at_the_first_word_gap_after_32_characters() {
        // Three 9-letter words and a 1-letter word with single spaces:
        // 9 + 1 + 9 + 1 + 9 + 1 + 1 = 31 characters at the word gap.
        const SHORT: &str = "AAAAAAAAA BBBBBBBBB CCCCCCCCC D";
        assert_eq!(SHORT.len(), LINE_MIN_CHARS - 1);
        let mut lines = TrackLines::default();
        let mut events = text(4, SHORT);
        events.push(wb(4, 100));
        assert!(
            feed(&mut lines, &events).is_empty(),
            "a word gap at 31 characters must not print"
        );

        // One more word: the line is 33 at the next word gap.
        let out = feed(&mut lines, &[ch(4, 101, 'E'), wb(4, 102)]);
        assert_eq!(out, vec!["[track 4] AAAAAAAAA BBBBBBBBB CCCCCCCCC D E"]);
        assert!(!out[0].ends_with(' '));

        // The buffer starts over: the next word is a fresh line, no
        // leading space.
        feed(&mut lines, &text(4, "FG"));
        assert_eq!(lines.ingest(&closed(4)).as_deref(), Some("[track 4] FG"));
    }

    #[test]
    fn a_line_without_word_gaps_is_cut_at_64_characters() {
        let mut lines = TrackLines::default();
        let events: Vec<_> = (0..70).map(|i| ch(6, i, 'T')).collect();
        let out: Vec<_> = events
            .iter()
            .enumerate()
            .filter_map(|(i, ev)| lines.ingest(ev).map(|l| (i, l)))
            .collect();
        assert_eq!(out.len(), 1);
        let (at, line) = &out[0];
        assert_eq!(*at, LINE_MAX_CHARS - 1, "the 64th character cuts the line");
        assert_eq!(line, &format!("[track 6] {}", "T".repeat(LINE_MAX_CHARS)));
        assert_eq!(
            lines.ingest(&closed(6)),
            Some(format!("[track 6] {}", "T".repeat(6)))
        );
    }

    #[test]
    fn a_closed_track_prints_its_partial_line_and_its_state_is_freed() {
        let mut lines = TrackLines::default();
        let mut events = vec![meta(7, 14_060_582.6), speed(7, 20.0)];
        events.extend(text(7, "CQ DE W1AW"));
        assert!(feed(&mut lines, &events).is_empty());
        assert_eq!(
            lines.ingest(&closed(7)).as_deref(),
            Some("[track 7 14060.6 kHz 20 WPM] CQ DE W1AW")
        );
        assert_eq!(lines.pending_tracks(), 0);
        assert!(lines.finish().is_empty());
    }

    #[test]
    fn a_track_closed_without_text_prints_nothing() {
        let mut lines = TrackLines::default();
        assert!(feed(&mut lines, &[meta(3, 14_060_000.0), speed(3, 22.0)]).is_empty());
        assert_eq!(lines.pending_tracks(), 1);
        assert_eq!(lines.ingest(&closed(3)), None);
        assert_eq!(lines.pending_tracks(), 0);
    }

    #[test]
    fn track_promoted_creates_no_state() {
        // MAN-19: a bare promotion never receives a TrackClosed, so state
        // created here would leak for the life of the process.
        let mut lines = TrackLines::default();
        assert_eq!(lines.ingest(&promoted(5, 14_000_000.0)), None);
        assert_eq!(lines.pending_tracks(), 0);
        assert!(lines.finish().is_empty());
    }

    #[test]
    fn finish_flushes_every_pending_line_in_track_order() {
        let mut lines = TrackLines::default();
        feed(&mut lines, &text(9, "DE N0XYZ"));
        feed(&mut lines, &text(3, "CQ K9ABC"));
        assert_eq!(
            lines.finish(),
            vec!["[track 3] CQ K9ABC", "[track 9] DE N0XYZ"]
        );
        assert_eq!(lines.pending_tracks(), 0);
        assert!(lines.finish().is_empty());
    }

    #[test]
    fn prosigns_are_dropped_and_repeated_word_gaps_collapse() {
        let events = vec![
            wb(8, 0),
            ch(8, 1, 'C'),
            ch(8, 2, 'Q'),
            wb(8, 3),
            wb(8, 4),
            DecoderEvent::char_decoded(8, 5, Glyph::Prosign(Prosign::Ar), 1.0),
            ch(8, 6, 'D'),
            ch(8, 7, 'E'),
            wb(8, 8),
            DecoderEvent::char_decoded(8, 9, Glyph::Prosign(Prosign::Sk), 1.0),
            wb(8, 10),
        ];
        let mut lines = TrackLines::default();
        assert!(feed(&mut lines, &events).is_empty());
        let line = lines.ingest(&closed(8)).expect("text was decoded");
        assert_eq!(line, "[track 8] CQ DE");
        assert_eq!(
            line.strip_prefix("[track 8] ").unwrap(),
            events_to_text(&events)
        );
    }

    #[test]
    fn label_uses_the_latest_frequency_and_speed_and_omits_unknown_fields() {
        // 32 characters, so the word gap after it prints the line.
        const CALL: &str = "CQ CQ CQ DE W1AW W1AW W1AW PSE K";
        assert_eq!(CALL.len(), LINE_MIN_CHARS);
        let mut lines = TrackLines::default();

        let mut events = text(5, CALL);
        events.push(wb(5, 100));
        assert_eq!(feed(&mut lines, &events), vec![format!("[track 5] {CALL}")]);

        let mut events = vec![meta(5, 14_060_582.6), speed(5, 19.6)];
        events.extend(text(5, CALL));
        events.push(wb(5, 200));
        assert_eq!(
            feed(&mut lines, &events),
            vec![format!("[track 5 14060.6 kHz 20 WPM] {CALL}")]
        );

        // A later meta() moves the frequency the next line shows.
        feed(&mut lines, &[meta(5, 14_061_049.0)]);
        feed(&mut lines, &text(5, "K"));
        assert_eq!(
            lines.ingest(&closed(5)).as_deref(),
            Some("[track 5 14061.0 kHz 20 WPM] K")
        );

        // Only the frequency is known: the speed is left out.
        feed(&mut lines, &[meta(11, 7_030_000.0)]);
        feed(&mut lines, &text(11, "TEST"));
        assert_eq!(
            lines.ingest(&closed(11)).as_deref(),
            Some("[track 11 7030.0 kHz] TEST")
        );
    }
}
