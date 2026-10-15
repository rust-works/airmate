//! Catalog drift: the shipped capability catalog carries an `as_of` date. Context windows and
//! limits change; an entry set nobody has looked at for a long time is flagged.

use crate::Error;

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses `YYYY-MM-DD` into days since the epoch.
///
/// # Errors
///
/// The text is not a date.
pub fn parse_date(s: &str) -> Result<i64, Error> {
    let bad = || Error(format!("`{s}` is not a date like 2026-10-14"));
    let mut p = s.trim().trim_matches('"').split('-');
    let (y, m, d): (i64, i64, i64) = (
        p.next().ok_or_else(bad)?.parse().map_err(|_| bad())?,
        p.next().ok_or_else(bad)?.parse().map_err(|_| bad())?,
        p.next().ok_or_else(bad)?.parse().map_err(|_| bad())?,
    );
    if p.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    Ok(days_from_civil(y, m, d))
}

/// The `as_of` value of the catalog file.
///
/// # Errors
///
/// The file has no `as_of:` line.
pub fn as_of(catalog_yaml: &str) -> Result<String, Error> {
    catalog_yaml
        .lines()
        .find_map(|l| l.strip_prefix("as_of:"))
        .map(|v| v.trim().trim_matches('"').to_owned())
        .ok_or_else(|| Error("the catalog has no `as_of:` date".to_owned()))
}

/// How many days old the catalog is on `today`, and whether that exceeds `max_age_days`.
///
/// # Errors
///
/// A date does not parse.
pub fn check(catalog_yaml: &str, today: &str, max_age_days: i64) -> Result<(i64, bool), Error> {
    let age = parse_date(today)? - parse_date(&as_of(catalog_yaml)?)?;
    Ok((age, age > max_age_days))
}

/// Today's date from the system clock, as `YYYY-MM-DD`.
#[must_use]
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let z = i64::try_from(secs / 86_400).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_parse_and_difference() {
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
        assert_eq!(
            parse_date("2026-10-14").unwrap() - parse_date("2026-07-16").unwrap(),
            90
        );
        assert_eq!(
            parse_date("2000-03-01").unwrap() - parse_date("2000-02-28").unwrap(),
            2,
            "a leap year"
        );
        for bad in [
            "",
            "2026",
            "2026-13-01",
            "2026-10-32",
            "a-b-c",
            "2026-10-14-01",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_catalog_is_stale_past_the_limit() {
        let cat = "# comment\nas_of: \"2026-01-01\"\nentries: []\n";
        assert_eq!(as_of(cat).unwrap(), "2026-01-01");
        assert_eq!(check(cat, "2026-03-31", 90).unwrap(), (89, false));
        assert_eq!(check(cat, "2026-04-02", 90).unwrap(), (91, true));
        assert!(as_of("entries: []").is_err());
    }

    #[test]
    fn today_looks_like_a_date() {
        let t = today();
        assert_eq!(t.len(), 10);
        assert!(parse_date(&t).unwrap() > parse_date("2026-01-01").unwrap());
    }
}
