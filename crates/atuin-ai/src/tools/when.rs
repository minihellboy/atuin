//! Time bounds the search tools take, such as `since: "today"` or `until: "2026-09-01"`.

use atuin_common::time::UtcOffsetExt;

use crate::tools::ToolOutcome;

/// Parse the time bound that the tool argument `field` holds, in the user's local time.
///
/// `None`, or a blank string, is no bound. Models often pass `""` to mean "no filter".
pub fn parse_bound(
    field: &str,
    value: Option<&str>,
    dialect: interim::Dialect,
) -> Result<Option<time::OffsetDateTime>, ToolOutcome> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let offset = time::UtcOffset::local_or_utc();
    let now = time::OffsetDateTime::now_utc().to_offset(offset);
    let offset_at = |t| time::UtcOffset::local_offset_at(t).unwrap_or(offset);

    parse_at(value, now, dialect, offset_at).map(Some).map_err(|e| {
        ToolOutcome::Error(format!(
            "Could not parse {field} {value:?} ({e}). Try 'today', '2 days ago' or a date like \
             '2026-09-01'."
        ))
    })
}

/// Parse `value` relative to `now`.
///
/// interim resolves "today" and "yesterday" to now's time of day on that date. As a bound, they
/// mean the start of that day. That midnight takes the offset in force then, not now's, so a DST
/// change since then doesn't shift the bound by an hour.
pub fn parse_at(
    value: &str,
    now: time::OffsetDateTime,
    dialect: interim::Dialect,
    offset_at: impl Fn(time::OffsetDateTime) -> time::UtcOffset,
) -> Result<time::OffsetDateTime, interim::DateError> {
    let parsed = interim::parse_date_string(value, now, dialect)?;

    if value.eq_ignore_ascii_case("today") || value.eq_ignore_ascii_case("yesterday") {
        let midnight = parsed.replace_time(time::Time::MIDNIGHT);

        Ok(midnight.replace_offset(offset_at(midnight)))
    } else {
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use time::macros::{datetime, offset};

    use super::*;

    #[rstest]
    #[case("today", datetime!(2026-09-29 00:00 +1))]
    #[case("Yesterday", datetime!(2026-09-28 00:00 +1))]
    #[case("3 hours ago", datetime!(2026-09-29 16:30 +1))]
    #[case("2026-09-01", datetime!(2026-09-01 00:00 +1))]
    fn bounds(#[case] value: &str, #[case] expected: time::OffsetDateTime) {
        let now = datetime!(2026-09-29 19:30 +1);

        assert_eq!(parse_at(value, now, interim::Dialect::Uk, |_| now.offset()).unwrap(), expected);
    }

    // US Eastern falls back from -4 to -5 at 06:00 UTC on 2026-11-01.
    #[rstest]
    #[case("today", datetime!(2026-11-01 00:00 -4))]
    #[case("yesterday", datetime!(2026-10-31 00:00 -4))]
    fn bounds_across_dst(#[case] value: &str, #[case] expected: time::OffsetDateTime) {
        let now = datetime!(2026-11-01 19:30 -5);
        let change = datetime!(2026-11-01 06:00 UTC);
        let offset_at = |t: time::OffsetDateTime| {
            if t < change {
                offset!(-4)
            } else {
                offset!(-5)
            }
        };

        assert_eq!(parse_at(value, now, interim::Dialect::Uk, offset_at).unwrap(), expected);
    }

    #[rstest]
    #[case::missing(None)]
    #[case::blank(Some("  "))]
    fn no_value_is_no_bound(#[case] value: Option<&str>) {
        assert!(matches!(parse_bound("since", value, interim::Dialect::Uk), Ok(None)));
    }

    #[rstest]
    fn a_bad_value_names_its_field() {
        let Err(ToolOutcome::Error(e)) =
            parse_bound("until", Some("not a date"), interim::Dialect::Uk)
        else {
            panic!("expected an error");
        };
        assert!(e.starts_with("Could not parse until \"not a date\""), "{e}");
    }
}
