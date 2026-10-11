//! Operator-maintained notched-frequency list (MAN-31): fixed ranges that
//! persistently generate false spots (birdies, spurs, local noise
//! sources). Orthogonal to the automatic validation pipeline
//! (ARCHITECTURE §6 steps 1-4). Legacy precedent: Aggregator's "Notched
//! Frequencies File".

/// A closed frequency interval `[low_hz, high_hz]`, inclusive of both
/// endpoints so an operator can notch an exact spur frequency with a
/// zero-width range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreqRange {
    pub low_hz: f64,
    pub high_hz: f64,
}

impl FreqRange {
    pub fn contains(&self, freq_hz: f64) -> bool {
        freq_hz >= self.low_hz && freq_hz <= self.high_hz
    }
}

/// A pure boolean membership lookup over operator-notched frequency
/// ranges -- like `Blocklist`, this cannot affect `Spot` output ordering
/// (SPEC-decode-core.md §6 rule 3), so a plain `Vec` is fine.
#[derive(Debug, Default, Clone)]
pub struct NotchList {
    ranges: Vec<FreqRange>,
}

impl NotchList {
    /// Parses one `low_hz-high_hz` range per line; blank lines and
    /// `#`-prefixed comment lines are ignored (legacy Aggregator "Notched
    /// Frequencies File" format). Malformed lines -- including a
    /// non-finite (`inf`/`NaN`) endpoint, which would otherwise silently
    /// widen into an unbounded or collapsed notch -- are ignored.
    pub fn parse(text: &str) -> Self {
        let ranges = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(Self::parse_range)
            .collect();
        Self { ranges }
    }

    /// Splits on a `-` that is genuinely the range separator, not the sign
    /// of a negative endpoint -- an offset-frequency track below center
    /// reports negative `freq_hz`, so `-1200--800` must parse as
    /// `(-1200, -800)`, not fail on the leading minus. Tries each `-` from
    /// left to right (skipping index 0, which can only be a leading sign)
    /// and accepts the first split where both sides parse as finite
    /// numbers.
    fn parse_range(line: &str) -> Option<FreqRange> {
        for i in 1..line.len() {
            if line.as_bytes()[i] != b'-' {
                continue;
            }
            if !line.is_char_boundary(i) {
                continue;
            }
            let (Ok(lo), Ok(hi)) = (
                line[..i].trim().parse::<f64>(),
                line[i + 1..].trim().parse::<f64>(),
            ) else {
                continue;
            };
            if !lo.is_finite() || !hi.is_finite() {
                continue;
            }
            return Some(FreqRange {
                low_hz: lo.min(hi),
                high_hz: lo.max(hi),
            });
        }
        None
    }

    pub fn contains(&self, freq_hz: f64) -> bool {
        if self.is_empty() {
            return false;
        }
        self.ranges.iter().any(|r| r.contains(freq_hz))
    }

    /// Parsed ranges; malformed lines are not counted (MAN-78: a reload
    /// logs this).
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    #[cfg(test)]
    fn ranges(&self) -> &[FreqRange] {
        &self.ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# local birdie
14025000-14025050

7040100.5-7040100.5
";

    #[test]
    fn frequency_inside_a_range_is_notched() {
        let notch = NotchList::parse(FIXTURE);
        assert!(notch.contains(14_025_025.0));
    }

    #[test]
    fn frequency_at_a_range_boundary_is_notched() {
        let notch = NotchList::parse(FIXTURE);
        assert!(notch.contains(14_025_000.0));
        assert!(notch.contains(14_025_050.0));
    }

    #[test]
    fn zero_width_range_notches_exactly_one_frequency() {
        let notch = NotchList::parse(FIXTURE);
        assert!(notch.contains(7_040_100.5));
        assert!(!notch.contains(7_040_101.0));
    }

    #[test]
    fn frequency_outside_every_range_is_not_notched() {
        let notch = NotchList::parse(FIXTURE);
        assert!(!notch.contains(14_030_000.0));
    }

    #[test]
    fn comment_and_blank_lines_are_ignored() {
        let notch = NotchList::parse(FIXTURE);
        assert_eq!(notch.ranges().len(), 2);
    }

    #[test]
    fn notch_len_counts_only_parsed_ranges() {
        let notch = NotchList::parse(&format!("{FIXTURE}garbage\n"));
        assert_eq!(notch.len(), 2);
        assert!(!notch.is_empty());
        assert!(NotchList::default().is_empty());
    }

    #[test]
    fn non_finite_endpoints_are_rejected_not_treated_as_an_open_range() {
        // A non-finite endpoint must never widen into an unbounded notch --
        // if it did, "14000000-inf" would silently suppress every spot
        // above 14 MHz.
        let notch = NotchList::parse("14000000-inf\n");
        assert!(!notch.contains(20_000_000.0));
        assert_eq!(notch.ranges().len(), 0);
    }

    #[test]
    fn nan_endpoint_is_rejected_not_collapsed_via_min_max() {
        let notch = NotchList::parse("NaN-14000000\n");
        assert_eq!(notch.ranges().len(), 0);
    }

    #[test]
    fn negative_endpoints_parse_as_a_signed_range() {
        // An offset-frequency track below center reports negative freq_hz;
        // the leading '-' of a negative endpoint must not be mistaken for
        // the range separator.
        let notch = NotchList::parse("-1200--800\n");
        assert!(notch.contains(-1000.0));
        assert!(!notch.contains(-1201.0));
        assert!(!notch.contains(-799.0));
    }

    #[test]
    fn whitespace_around_the_separator_is_tolerated() {
        let notch = NotchList::parse("14025000 - 14025100\n");
        assert!(notch.contains(14_025_050.0));
        assert!(!notch.contains(14_025_200.0));
    }

    #[test]
    fn one_negative_one_positive_endpoint_parses_correctly() {
        let notch = NotchList::parse("-500-500\n");
        assert!(notch.contains(0.0));
        assert!(notch.contains(-500.0));
        assert!(notch.contains(500.0));
        assert!(!notch.contains(-501.0));
        assert!(!notch.contains(501.0));
    }
}
