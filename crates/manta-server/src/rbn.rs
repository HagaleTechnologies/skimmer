//! RBN-format ("DX de ...") line rendering for the telnet cluster server.
//! ARCHITECTURE §7.
//!
//! MAN-88: the line is a true fixed-column AK1A layout, not just fields in
//! the right order -- every field boundary is anchored to an absolute
//! column, matching a live RBN capture from `telnet.reversebeacon.net:7000`
//! byte-for-byte (see `LIVE_RBN_CAPTURE` in the tests below and
//! `docs/DECISIONS/2026-09-06-man88-ak1a-column-layout.md`).

use manta_spot::{Spot, SpotType};

/// MAN-102 / decision D3 (2026-09-06 broad review): RBN and CW Skimmer
/// quote spot SNR in a 500 Hz reference bandwidth; manta's detector
/// measures it in 2500 Hz (SPEC §2.3). `10*log10(2500/500) = 6.9897 dB`.
/// Applied ONLY here, at the telnet/uplink wire boundary -- the JSON
/// stream keeps the native 2500 Hz value plus an explicit `snrRefHz` (see
/// `spot_message.rs`), and nothing inside the decode pipeline changes.
/// Also used by `manta bench sensitivity` to quote its SNR axis in 500 Hz (MAN-116).
pub const RBN_REF_BW_CORRECTION_DB: f32 = 6.989_7;

fn spot_type_label(spot_type: SpotType) -> &'static str {
    match spot_type {
        SpotType::Cq => "CQ",
        SpotType::De => "DE",
        SpotType::Beacon => "BEACON",
        SpotType::Unknown => "",
    }
}

/// Which wire layout `format_line` renders.
///
/// Both variants share the fixed-column AK1A geometry MAN-88 measured
/// against a live RBN capture; they differ only in whether the 6-column
/// mode field is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineFormat {
    /// The RBN relay layout: mode column present, time at column 71.
    /// What downstream loggers see on telnet.reversebeacon.net today, and
    /// manta's default.
    #[default]
    Rbn,
    /// The CW-Skimmer-native layout: no mode column, time at column 67.
    /// For operators running manta behind W3OA's Aggregator, which
    /// consumes CW Skimmer's own line rather than the RBN relay's.
    ///
    /// CAVEAT (MAN-88, Decision 2): this byte layout is *derived*, not
    /// measured. The only primary source for CW Skimmer's own line
    /// (a pdftotext dump of the CW Skimmer manual, made during the
    /// 2026-09-05 review) was a scratch artifact and is not preserved in
    /// this repo or the thoughts pool. What every surviving source agrees
    /// on is qualitative -- "no mode field; CQ/DE/blank" -- so this
    /// variant deletes the 6-wide mode field and substitutes the layout's
    /// standard 2-space separator, shifting everything after the callsign
    /// column left by 4. MAN-86 (the `SKIMMER/SETT` handshake) is where a
    /// real Aggregator/CW Skimmer capture will land; correcting this is a
    /// one-constant change plus a test-vector update.
    Skimmer,
}

/// The frequency field's last character lands on this 1-indexed column,
/// matching the live RBN capture. Equivalent to the classic AK1A 10-wide
/// spotter field: `DX de ` (6) + a base callsign + `-#:` (3), padded out
/// so an 8-character 5-digit-MHz frequency ends here.
///
/// This is an anchor, not a guarantee: `format_line` keeps a mandatory
/// one-space separator (Decision 3), so the anchor holds only while
/// `identity + freq` is at most 23 columns. Two combinations exceed it: a
/// 7-character base callsign at an 8-character frequency (identity 16 +
/// freq 8 = 24, e.g. `VE3ABCD` on 20 m), and a 6-character base callsign
/// at a 9-character 6-digit-MHz frequency (identity 15 + freq 9 = 24,
/// e.g. `DL8LAS` on 2 m). Both need that separator, so the frequency ends
/// at column 25 -- but the drift stops there: `CALL_START_COL` and
/// `MODE_START_COL` below are absolute anchors, so the callsign, mode,
/// SNR, WPM, type and time fields all stay on their RBN columns. See
/// `docs/DECISIONS/2026-09-06-man88-ak1a-column-layout.md` Decision 3,
/// pinned by `a_seven_character_spotter_drifts_only_the_frequency_field`
/// and `a_six_character_spotter_on_two_metres_keeps_every_later_column`
/// below.
const FREQ_END_COL: usize = 24;

/// The callsign column's first 1-indexed column (columns 27-41), reached
/// by a two-space separator from a frequency that ended on
/// `FREQ_END_COL`. An absolute anchor: a frequency field that overran its
/// own anchor is absorbed here, down to the mandatory one-space separator.
const CALL_START_COL: usize = 27;

/// The mode field's first 1-indexed column, i.e. the end of the 15-wide
/// callsign column (27-41). Also an absolute anchor, so an over-long
/// callsign -- or an already-shifted frequency -- is absorbed here rather
/// than moving the SNR, WPM, type and time fields.
///
/// A callsign that ends exactly on column 41 fills the column rather than
/// overrunning it, so the gap before the mode field is zero and this
/// anchor still holds (`a_callsign_that_fills_the_column_exactly_keeps_
/// the_mode_column_at_42`). Only a call that runs past column 41 gives up
/// the anchor, and then only down to one mandatory separating space
/// (`a_callsign_one_column_past_the_field_keeps_its_mandatory_separator`).
const MODE_START_COL: usize = 42;

/// The remaining four anchors are expressed as offsets from the column the
/// mode field ends on, not as absolute columns, because the mode field is
/// the one whose width the layout selects: it is 6 wide in the RBN relay
/// layout (ending at column 47) and 2 wide in the CW-Skimmer layout
/// (ending at 43), and everything after it therefore sits 4 columns
/// further left in `LineFormat::Skimmer`. Anchoring on the mode field's
/// actual end keeps both layouts described by one set of constants, and
/// keeps an over-long callsign shifting the whole tail as a block rather
/// than tearing it apart.
///
/// SNR is right-justified so its last char lands 2 columns past the mode
/// field (RBN column 49), then a literal ` dB`.
const SNR_END_OFFSET: usize = 2;
/// WPM is right-justified so its last char lands 9 columns past the mode
/// field (RBN column 56), then a literal ` WPM`.
const WPM_END_OFFSET: usize = 9;
/// The 6-wide, left-justified spot-type field starts 16 columns past the
/// mode field (RBN columns 63-68).
const TYPE_START_OFFSET: usize = 16;
/// The `HHMMZ` time field starts 24 columns past the mode field -- RBN
/// column 71, the column MAN-88 exists to restore.
const TIME_START_OFFSET: usize = 24;

/// Renders one spot as a fixed-column AK1A `DX de` cluster line, e.g.
/// `DX de W3XYZ-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      0312Z`.
///
/// `unix_ts_secs` is the spot's wall-clock time (UTC); converting from the
/// decoder's sample-count timestamp happens at the caller, not here (see
/// `manta_spot::validator::Spot`'s doc comment on why `Spot` itself carries
/// no wall-clock time). `snr` is `Spot.snr_db` (SPEC §2.3's native 2500 Hz
/// measurement) converted to the 500 Hz RBN/CW Skimmer reference bandwidth
/// -- see `RBN_REF_BW_CORRECTION_DB`.
///
/// Every field after the spotter identity is anchored to an absolute
/// column (MAN-88): the identity and callsign fields are MINIMUM widths,
/// never truncated -- an oversized value (e.g. MAN-28's Watch List
/// bypasses callsign-grammar validation) shifts the field that follows it
/// right rather than corrupting a field or forging a shorter identity, and
/// keeps one mandatory separating space so a whitespace-splitting parser
/// still sees two tokens. A value that fills its column exactly is a fit,
/// not an overflow: it keeps the next field on its own anchor and takes a
/// zero-width gap, which is how the layout's fixed columns survive a
/// 15-character callsign.
///
/// Each anchor is computed from the column the *previous* field actually
/// ended on, so an overrun is absorbed by the next separator instead of
/// cascading down the line: a 2 m frequency behind a six-character
/// spotter (`DX de DL8LAS-#: 144110.00`) costs the frequency field its
/// own column-24 anchor, and nothing else.
pub fn format_line(
    spot: &Spot,
    spotter_call: &str,
    unix_ts_secs: i64,
    line_format: LineFormat,
) -> String {
    let freq_khz = spot.freq_hz / 1000.0;
    let secs_of_day = unix_ts_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;

    let identity = format!("DX de {spotter_call}-#:");
    // `{:.2}` is a *minimum* width: a 2 m frequency (`144110.00`, 9 chars)
    // widens the field rather than losing a digit. Where the identity's
    // own padding cannot absorb that extra column, the callsign gap below
    // does, so only the frequency field itself moves.
    let freq = format!("{freq_khz:.2}");
    // Decision 3: anchor the frequency's last char to FREQ_END_COL, but
    // never let the two fields abut -- truncating an operator's own
    // callsign would forge a wrong spotter ID, and abutting would corrupt
    // the spotter token for whitespace-splitting parsers.
    let identity_end_col = identity.chars().count();
    let freq_gap = FREQ_END_COL
        .saturating_sub(identity_end_col + freq.chars().count())
        .max(1);
    let freq_end_col = identity_end_col + freq_gap + freq.chars().count();
    // Re-anchor on an absolute column rather than a fixed two-space
    // separator, so a frequency that overran FREQ_END_COL (a 7-character
    // spotter, or a 6-character one on 2 m) does not push the callsign
    // and everything after it one column right.
    let call_gap = CALL_START_COL.saturating_sub(freq_end_col + 1).max(1);
    let call_end_col = freq_end_col + call_gap + spot.callsign.chars().count();
    // Same rule for the mode column, with one refinement the review of PR
    // #114 asked for: the callsign column is exactly 15 wide (27-41), so a
    // call that *fills* it is a fit, not an overflow -- the gap before the
    // mode field is then legitimately zero and the mode column keeps its
    // own anchor at 42. Forcing a separator there would have inserted a
    // 16th callsign column and walked the mode, SNR, WPM, type and time
    // fields one column right, which is precisely what a fixed-column
    // parser cannot survive. The mandatory one-space separator is reserved
    // for an actual overrun: MAN-28's Watch List bypasses the callsign
    // grammar, so an allowlisted entry longer than the column can reach
    // the renderer, and that value must not abut the mode token.
    let mode_gap = if call_end_col < MODE_START_COL {
        MODE_START_COL - call_end_col - 1
    } else {
        1
    };
    // The mode field's own trailing padding is the separator to the SNR
    // field -- there is no extra gap in the RBN layout (columns 42-47).
    let mode = match line_format {
        LineFormat::Rbn => "CW    ",
        LineFormat::Skimmer => "  ",
    };
    let mode_end_col = call_end_col + mode_gap + mode.chars().count();

    // MAN-88 review finding: the SNR and WPM fields are MINIMUM widths too.
    // `Demod::snr_2500_db` is passed through unclamped by the validator, so
    // a track whose keying rails converge can reach roughly -14 dB, and
    // `{:>2}` then renders three characters. Anchor each following field on
    // the column its predecessor actually ended on -- exactly as the
    // frequency and callsign fields above do -- so a wide SNR (or a wide
    // WPM) widens only its own field instead of walking the type and time
    // fields off the columns a fixed-column parser reads.
    let snr = format!(
        "{}",
        (spot.snr_db + RBN_REF_BW_CORRECTION_DB).round() as i32
    );
    let snr_gap = SNR_END_OFFSET.saturating_sub(snr.chars().count());
    let snr_end_col = mode_end_col + snr_gap + snr.chars().count();
    // ` dB` is part of the SNR field, not a separator.
    let db_end_col = snr_end_col + " dB".len();

    let wpm = format!("{}", spot.wpm.round() as i32);
    let wpm_gap = (mode_end_col + WPM_END_OFFSET)
        .saturating_sub(db_end_col + wpm.chars().count())
        .max(1);
    let wpm_end_col = db_end_col + wpm_gap + wpm.chars().count();
    // ` WPM` is part of the WPM field, not a separator.
    let wpm_unit_end_col = wpm_end_col + " WPM".len();

    // The type field is left-justified in 6 columns; `BEACON` fills it
    // exactly, so nothing in `spot_type_label` overruns it today, but the
    // time anchor below is computed from its rendered width regardless.
    let ctx = format!("{:<6}", spot_type_label(spot.spot_type));
    let type_gap = (mode_end_col + TYPE_START_OFFSET)
        .saturating_sub(wpm_unit_end_col + 1)
        .max(1);
    let type_end_col = wpm_unit_end_col + type_gap + ctx.chars().count();
    let time_gap = (mode_end_col + TIME_START_OFFSET)
        .saturating_sub(type_end_col + 1)
        .max(1);

    format!(
        "{identity}{:freq_gap$}{freq}{:call_gap$}{call}{:mode_gap$}{mode}\
         {:snr_gap$}{snr} dB{:wpm_gap$}{wpm} WPM{:type_gap$}{ctx}\
         {:time_gap$}{hour:02}{minute:02}Z",
        "",
        "",
        "",
        "",
        "",
        "",
        "",
        call = spot.callsign,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbn_reference_bandwidth_correction_is_ten_log_five() {
        assert!((RBN_REF_BW_CORRECTION_DB as f64 - 10.0 * 5f64.log10()).abs() < 1e-4);
    }

    /// The one live RBN line available as evidence, captured from
    /// telnet.reversebeacon.net:7000 on 2026-09-06 02:36Z and quoted verbatim
    /// in the 2026-09-05 lens-2 operations review. MAN-88 Scenario 1: manta's
    /// line must be byte-identical to this.
    const LIVE_RBN_CAPTURE: &str =
        "DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ      0236Z";

    /// The capture's SNR is quoted in RBN's 500 Hz reference bandwidth;
    /// `Spot.snr_db` is manta's native 2500 Hz measurement (MAN-102/D3), so
    /// the spot carries 14 dB(2500), which renders as 21 dB(500).
    fn capture_spot() -> Spot {
        Spot {
            callsign: "N8II".to_string(),
            freq_hz: 14_011_900.0,
            snr_db: 14.0,
            wpm: 25.0,
            spot_type: SpotType::Cq,
            confidence: 0.9,
            track_id: 1,
            sample_ts: 0,
        }
    }

    /// Returns the 1-indexed column `needle` starts at.
    fn col_of(line: &str, needle: &str) -> usize {
        line.find(needle).expect("field missing from line") + 1
    }

    #[test]
    fn matches_the_live_rbn_capture_byte_for_byte() {
        let line = format_line(&capture_spot(), "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert_eq!(line, LIVE_RBN_CAPTURE);
    }

    #[test]
    fn frequency_carries_two_decimals_of_khz_and_ends_at_column_24() {
        let mut spot = capture_spot();
        spot.freq_hz = 14_011_910.0; // the 10 Hz RBN now carries
        let line = format_line(&spot, "S53A", 0, LineFormat::Rbn);
        assert!(line.contains("14011.91"), "line was: {line}");
        assert_eq!(col_of(&line, "14011.91") + "14011.91".len() - 1, 24);
    }

    #[test]
    fn time_lands_at_column_71_for_every_realistic_spotter_length() {
        for spotter in ["W4X", "W5AU", "W3XYZ", "DL8LAS"] {
            let line = format_line(
                &capture_spot(),
                spotter,
                2 * 3600 + 36 * 60,
                LineFormat::Rbn,
            );
            assert_eq!(col_of(&line, "0236Z"), 71, "spotter {spotter}: {line}");
        }
    }

    #[test]
    fn callsign_column_is_15_wide_so_the_mode_field_starts_at_column_42() {
        for call in ["A1A", "N8II", "JA1ABC", "VK9/W3XYZ/QRP1"] {
            let mut spot = capture_spot();
            spot.callsign = call.to_string();
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            assert_eq!(col_of(&line, "CW"), 42, "call {call}: {line}");
            assert_eq!(col_of(&line, "0236Z"), 71, "call {call}: {line}");
        }
    }

    #[test]
    fn every_spot_type_label_fits_the_six_wide_type_field() {
        // BEACON is exactly 6 characters -- the width's binding constraint.
        // The expected text is asserted alongside the column, so the test
        // cannot pass with two labels swapped or every label emptied.
        for (spot_type, label) in [
            (SpotType::Cq, "CQ"),
            (SpotType::De, "DE"),
            (SpotType::Beacon, "BEACON"),
            (SpotType::Unknown, ""),
        ] {
            let mut spot = capture_spot();
            spot.spot_type = spot_type;
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            assert_eq!(col_of(&line, "0236Z"), 71, "{spot_type:?}: {line}");
            // The type field is columns 63-68, left-justified in 6 columns.
            assert_eq!(
                &line[62..68],
                format!("{label:<6}"),
                "{spot_type:?}: {line}"
            );
        }
    }

    /// Decision 3's one documented column of drift: a 7-character base
    /// callsign makes `identity + freq` exactly 24 columns, so the mandatory
    /// separator pushes the frequency's last char to column 25. The drift
    /// stops there -- the callsign column re-anchors at 27, so mode and
    /// time stay on their RBN columns.
    #[test]
    fn a_seven_character_spotter_drifts_only_the_frequency_field() {
        let line = format_line(
            &capture_spot(),
            "VE3ABCD",
            2 * 3600 + 36 * 60,
            LineFormat::Rbn,
        );
        assert_eq!(
            col_of(&line, "14011.90") + "14011.90".len() - 1,
            25,
            "line was: {line}"
        );
        assert_eq!(col_of(&line, "N8II"), 27, "line was: {line}");
        assert_eq!(col_of(&line, "CW"), 42, "line was: {line}");
        assert_eq!(col_of(&line, "0236Z"), 71, "line was: {line}");
        // Never abuts: exactly one space separates identity from frequency,
        // and one more separates the frequency from the callsign column.
        assert!(
            line.starts_with("DX de VE3ABCD-#: 14011.90 N8II"),
            "line was: {line}"
        );
    }

    /// MAN-88 review finding: the cross-product the per-field tests missed
    /// -- a valid six-character spotter (`DL8LAS`) on 2 m, where the
    /// identity is 15 columns and `144110.00` is 9, so the two together
    /// already fill the frequency's anchor. The frequency field alone gives
    /// up its column-24 anchor to keep its mandatory separator; every later
    /// column a fixed-column parser reads must still be exact.
    #[test]
    fn a_six_character_spotter_on_two_metres_keeps_every_later_column() {
        let mut spot = capture_spot();
        spot.freq_hz = 144_110_000.0;
        let line = format_line(&spot, "DL8LAS", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert!(
            line.starts_with("DX de DL8LAS-#: 144110.00 N8II"),
            "line was: {line}"
        );
        assert_eq!(col_of(&line, "N8II"), 27, "line was: {line}");
        assert_eq!(col_of(&line, "CW"), 42, "line was: {line}");
        assert_eq!(col_of(&line, "21 dB"), 48, "line was: {line}");
        assert_eq!(col_of(&line, "25 WPM"), 55, "line was: {line}");
        assert_eq!(col_of(&line, "0236Z"), 71, "line was: {line}");
    }

    /// The same cross-product across the spotter lengths RBN actually
    /// carries, at every band manta channelizes. Only the fields whose own
    /// anchor a mandatory separator makes unreachable may move: the mode
    /// column at 42 and the time at 71 are exact for all 25 combinations,
    /// and the callsign column is exact for every spotter that leaves room
    /// for it (a 7-character spotter on 2 m needs identity 16 + freq 9 +
    /// two separators, which reaches column 27 on its own).
    #[test]
    fn every_spotter_length_and_band_keeps_the_later_columns_anchored() {
        for spotter in ["W4X", "W5AU", "W3XYZ", "DL8LAS", "VE3ABCD"] {
            for freq_hz in [
                1_822_500.0,
                3_573_600.0,
                14_011_900.0,
                50_110_000.0,
                144_110_000.0,
            ] {
                let mut spot = capture_spot();
                spot.freq_hz = freq_hz;
                let line = format_line(&spot, spotter, 2 * 3600 + 36 * 60, LineFormat::Rbn);
                let ctx = format!("spotter {spotter} at {freq_hz} Hz: {line}");
                assert_eq!(col_of(&line, "CW"), 42, "{ctx}");
                assert_eq!(col_of(&line, "0236Z"), 71, "{ctx}");
                let call_col = col_of(&line, "N8II");
                let expect_call_col = if spotter.len() == 7 && freq_hz >= 100_000_000.0 {
                    28
                } else {
                    27
                };
                assert_eq!(call_col, expect_call_col, "{ctx}");
            }
        }
    }

    #[test]
    fn low_and_high_bands_keep_the_frequency_field_anchored() {
        for freq_hz in [1_822_500.0, 3_573_600.0, 50_110_000.0, 144_110_000.0] {
            let mut spot = capture_spot();
            spot.freq_hz = freq_hz;
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            assert_eq!(col_of(&line, "0236Z"), 71, "{freq_hz} Hz: {line}");
        }
    }

    /// MAN-28's Watch List bypasses the callsign grammar entirely
    /// (`manta_spot::validator`), so an allowlisted entry longer than the
    /// 15-wide column can reach the renderer. It must shift the rest of the
    /// line right, never truncate and never abut the mode field.
    #[test]
    fn an_over_long_callsign_shifts_right_but_never_abuts_the_mode_field() {
        let mut spot = capture_spot();
        spot.callsign = "SOMEVERYLONGWATCHLISTCALL".to_string();
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert!(
            line.contains("SOMEVERYLONGWATCHLISTCALL CW"),
            "line was: {line}"
        );
    }

    /// MAN-88 PR #114 review finding: a callsign that fills the 15-wide
    /// column exactly (columns 27-41) is a *fit*, not an overflow. The
    /// mode column must still start on column 42 and every field after it
    /// on its own RBN column; a mandatory separator here would have made
    /// the callsign column 16 wide and pushed mode/SNR/WPM/type/time to
    /// 43/49/56/64/72. Reachable because MAN-28's Watch List bypasses the
    /// callsign grammar entirely, so an allowlisted 15-character entry
    /// reaches the renderer. The cost is that this one exact fit abuts the
    /// mode token -- the fixed columns are what the ticket asks for, and
    /// this is the only length at which the two rules conflict.
    #[test]
    fn a_callsign_that_fills_the_column_exactly_keeps_the_mode_column_at_42() {
        let mut spot = capture_spot();
        spot.callsign = "VK9/W3XYZ/QRP12".to_string();
        assert_eq!(
            spot.callsign.chars().count(),
            15,
            "test vector must be an exact fit"
        );
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        let ctx = format!("line was: {line}");
        assert_eq!(col_of(&line, "VK9/W3XYZ/QRP12"), 27, "{ctx}");
        assert_eq!(col_of(&line, "CW"), 42, "{ctx}");
        assert_eq!(col_of(&line, "21 dB"), 48, "{ctx}");
        assert_eq!(col_of(&line, "25 WPM"), 55, "{ctx}");
        assert_eq!(&line[62..68], "CQ    ", "{ctx}");
        assert_eq!(col_of(&line, "0236Z"), 71, "{ctx}");
    }

    /// The exact-fit rule must not leak into a genuine overrun: one column
    /// past the callsign field, the mode column gives up its anchor rather
    /// than let the two tokens run together.
    #[test]
    fn a_callsign_one_column_past_the_field_keeps_its_mandatory_separator() {
        let mut spot = capture_spot();
        spot.callsign = "VK9/W3XYZ/QRP123".to_string();
        assert_eq!(
            spot.callsign.chars().count(),
            16,
            "test vector must overrun by one"
        );
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert!(line.contains("VK9/W3XYZ/QRP123 CW"), "line was: {line}");
        // The whole tail moves as a block: one column for the extra
        // character, one for the separator the overrun now needs.
        assert_eq!(col_of(&line, "CW"), 44, "line was: {line}");
        assert_eq!(col_of(&line, "0236Z"), 73, "line was: {line}");
    }

    /// The exact-fit rule crossed with the wide-SNR rule: a 15-character
    /// Watch List entry and a three-character SNR together must still
    /// leave the WPM, type and time columns where a fixed-column parser
    /// reads them.
    #[test]
    fn an_exact_fit_callsign_and_a_wide_snr_still_keep_the_later_columns() {
        let mut spot = capture_spot();
        spot.callsign = "VK9/W3XYZ/QRP12".to_string();
        spot.snr_db = -21.0; // 2500 Hz; renders as -14 dB(500)
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        let ctx = format!("line was: {line}");
        assert_eq!(col_of(&line, "CW"), 42, "{ctx}");
        assert_eq!(col_of(&line, "-14 dB"), 48, "{ctx}");
        assert_eq!(col_of(&line, "25 WPM"), 55, "{ctx}");
        assert_eq!(&line[62..68], "CQ    ", "{ctx}");
        assert_eq!(col_of(&line, "0236Z"), 71, "{ctx}");
    }

    /// The same exact fit in the CW-Skimmer layout: the mode field is 2
    /// wide there, so the callsign column ends on 41 and the tail follows
    /// four columns left of the RBN one -- time on column 67.
    #[test]
    fn an_exact_fit_callsign_keeps_the_skimmer_time_column_at_67() {
        let mut spot = capture_spot();
        spot.callsign = "VK9/W3XYZ/QRP12".to_string();
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Skimmer);
        assert_eq!(col_of(&line, "0236Z"), 67, "line was: {line}");
    }

    /// Decision 3: an identity too long for columns 1-16 shifts the line right
    /// with exactly one separating space rather than truncating the operator's
    /// own callsign or running into the frequency.
    #[test]
    fn an_over_long_spotter_identity_shifts_right_but_never_abuts_the_frequency() {
        let line = format_line(
            &capture_spot(),
            "K5ARH/QRP",
            2 * 3600 + 36 * 60,
            LineFormat::Rbn,
        );
        assert!(
            line.contains("DX de K5ARH/QRP-#: 14011.90"),
            "line was: {line}"
        );
    }

    /// MAN-88 review finding: `{snr:>2}` is a minimum width, and
    /// `Demod::snr_2500_db` reaches roughly -14 dB when a track's keying
    /// rails converge -- neither the validator nor this renderer clamps it.
    /// A three-character SNR must widen its own field only; the WPM, type
    /// and time columns a fixed-column parser reads stay put. Inputs are
    /// 2500 Hz values; the expected text is the 500 Hz wire value
    /// (MAN-102/D3, +6.9897 dB).
    #[test]
    fn a_wide_snr_widens_its_own_field_but_moves_no_later_column() {
        for (snr_db, wire) in [(-21.0, "-14"), (-16.5, "-10"), (98.0, "105"), (93.0, "100")] {
            let mut spot = capture_spot();
            spot.snr_db = snr_db;
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            let ctx = format!("snr {snr_db}: {line}");
            let snr_text = format!("{wire} dB");
            // The SNR field itself still starts on its own column 48.
            assert_eq!(col_of(&line, &snr_text), 48, "{ctx}");
            assert_eq!(col_of(&line, "25 WPM"), 55, "{ctx}");
            assert_eq!(&line[62..68], "CQ    ", "{ctx}");
            assert_eq!(col_of(&line, "0236Z"), 71, "{ctx}");
            // Never abuts the field before it.
            assert!(line.contains(&format!(" {snr_text}")), "{ctx}");
        }
    }

    /// The same rule one field further along: WPM is a minimum width too,
    /// so a three-digit WPM (or a wide SNR *and* a wide WPM together) may
    /// not move the type or time columns.
    #[test]
    fn a_wide_wpm_widens_its_own_field_but_moves_no_later_column() {
        // 2500 Hz inputs: 21 dB(500) and -14 dB(500) on the wire.
        for (snr_db, wpm) in [(14.0, 100.0), (-21.0, 100.0), (-21.0, 8.0)] {
            let mut spot = capture_spot();
            spot.snr_db = snr_db;
            spot.wpm = wpm;
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            let ctx = format!("snr {snr_db}, wpm {wpm}: {line}");
            assert_eq!(&line[62..68], "CQ    ", "{ctx}");
            assert_eq!(col_of(&line, "0236Z"), 71, "{ctx}");
            // Both variable-width fields keep their mandatory separator.
            assert!(
                line.contains(&format!("dB  {} WPM", wpm.round() as i32))
                    || line.contains(&format!("dB {} WPM", wpm.round() as i32)),
                "{ctx}"
            );
        }
    }

    /// The skimmer layout's tail re-anchors on the same rule, 4 columns
    /// left of the RBN one -- a wide SNR must not walk its time field off
    /// column 67 either.
    #[test]
    fn the_skimmer_layout_keeps_its_time_column_under_a_wide_snr() {
        // 2500 Hz inputs: 21, -14 and 105 dB(500) on the wire.
        for snr_db in [14.0, -21.0, 98.0] {
            let mut spot = capture_spot();
            spot.snr_db = snr_db;
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Skimmer);
            assert_eq!(col_of(&line, "0236Z"), 67, "snr {snr_db}: {line}");
        }
    }

    #[test]
    fn wire_snr_is_referenced_to_500hz() {
        // D3/MAN-102: the pipeline measures SNR in 2500 Hz; RBN and CW
        // Skimmer quote 500 Hz. 23 dB(2500) + 10*log10(5) rounds to
        // 30 dB(500).
        let mut spot = capture_spot();
        spot.snr_db = 23.0;
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert!(line.contains("  30 dB  "), "line was: {line}");
        assert_eq!(col_of(&line, "30 dB"), 48, "line was: {line}");
    }

    #[test]
    fn a_negative_snr_spot_still_renders_a_sane_wire_value() {
        let mut spot = capture_spot();
        spot.snr_db = -4.0; // 2500 Hz
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert!(line.contains("   3 dB  "), "line was: {line}");
    }

    #[test]
    fn midnight_wraps_to_zero_zulu() {
        let line = format_line(&capture_spot(), "S53A", 0, LineFormat::Rbn);
        assert!(line.ends_with("0000Z"), "line was: {line}");
    }

    /// MAN-89 / D4: the `-#` suffix composes with an operator-configured `-N`
    /// SSID to produce RBN's own `CALL-N-#` node identity.
    #[test]
    fn an_ssid_spotter_renders_as_call_n_hash() {
        let line = format_line(&capture_spot(), "W5AU-1", 11_520, LineFormat::Rbn);
        assert!(line.starts_with("DX de W5AU-1-#:"), "line was: {line}");
        // Never truncated, never abutting -- holds under any column layout.
        assert!(
            line.trim_end()
                .strip_prefix("DX de W5AU-1-#:")
                .unwrap()
                .starts_with(' '),
            "identity ran into the frequency: {line}"
        );
    }

    #[test]
    fn the_skimmer_layout_drops_the_mode_column_and_moves_time_to_column_67() {
        let line = format_line(
            &capture_spot(),
            "S53A",
            2 * 3600 + 36 * 60,
            LineFormat::Skimmer,
        );
        assert!(!line.contains(" CW "), "mode column still present: {line}");
        assert_eq!(col_of(&line, "0236Z"), 67, "line was: {line}");
        // Columns 1-41 are identical to the RBN layout.
        let rbn = format_line(&capture_spot(), "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert_eq!(line[..41], rbn[..41]);
    }
}
