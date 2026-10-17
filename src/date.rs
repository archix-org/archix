use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

/// A calendar date, used to name Arch Linux Archive snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl Date {
    pub fn today_utc() -> Date {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Date::from_unix(secs)
    }

    pub fn from_unix(secs: u64) -> Date {
        Date::from_days((secs / 86_400) as i64)
    }

    pub fn previous(self) -> Date {
        Date::from_days(self.days() - 1)
    }

    pub fn next(self) -> Date {
        Date::from_days(self.days() + 1)
    }

    pub fn days_until(self, later: Date) -> i64 {
        later.days() - self.days()
    }

    /// Days since 1970-01-01, using Howard Hinnant's civil calendar algorithm.
    fn days(self) -> i64 {
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn from_days(days: i64) -> Date {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Date { year, month, day }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl FromStr for Date {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("invalid date {s:?}; expected YYYY-MM-DD");
        let mut parts = s.split('-');
        let (Some(y), Some(m), Some(d), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(bad());
        };
        if y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return Err(bad());
        }
        let date = Date {
            year: y.parse().map_err(|_| bad())?,
            month: m.parse().map_err(|_| bad())?,
            day: d.parse().map_err(|_| bad())?,
        };
        if Date::from_days(date.days()) != date {
            return Err(bad());
        }
        Ok(date)
    }
}

impl serde::Serialize for Date {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Date {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_days() {
        for (days, text) in [(0, "1970-01-01"), (19_782, "2024-02-29"), (20_740, "2026-10-14")] {
            let date: Date = text.parse().unwrap();
            assert_eq!(date.days(), days);
            assert_eq!(Date::from_days(days), date);
            assert_eq!(date.to_string(), text);
        }
    }

    #[test]
    fn previous_crosses_month_and_year() {
        assert_eq!(
            "2026-03-01".parse::<Date>().unwrap().previous().to_string(),
            "2026-02-28"
        );
        assert_eq!(
            "2027-01-01".parse::<Date>().unwrap().previous().to_string(),
            "2026-12-31"
        );
    }

    #[test]
    fn rejects_impossible_dates() {
        for bad in ["2026-02-30", "2026-13-01", "26-01-01", "2026-1-01", "2026-01-01-01"] {
            assert!(bad.parse::<Date>().is_err(), "{bad}");
        }
    }
}
