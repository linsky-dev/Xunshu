use crate::QueryError;
use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeZone, Utc};
use std::cmp::Ordering;

/// 寻枢内置分类只比较扩展名，路径和文件内容都不参与分类。
pub(crate) fn category_extensions(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "audio" => Some(&[
            "aac", "aiff", "alac", "ape", "flac", "m4a", "mid", "mp3", "ogg", "opus", "wav", "wma",
        ]),
        "document" | "doc" => Some(&[
            "csv", "doc", "docm", "docx", "epub", "htm", "html", "md", "odf", "odp", "ods", "odt",
            "pdf", "ppt", "pptm", "pptx", "rtf", "txt", "wps", "xls", "xlsm", "xlsx",
        ]),
        "executable" | "exe" => Some(&["appimage", "bat", "cmd", "com", "exe", "msi", "ps1", "sh"]),
        "picture" | "pic" => Some(&[
            "avif", "bmp", "gif", "heic", "ico", "jpeg", "jpg", "png", "raw", "svg", "tif", "tiff",
            "webp",
        ]),
        "video" => Some(&[
            "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "ts", "webm", "wmv",
        ]),
        "compressed" | "archive" | "zip" => {
            Some(&["7z", "bz2", "gz", "rar", "tar", "tgz", "xz", "zip", "zst"])
        }
        _ => None,
    }
}

pub(crate) fn attribute_mask(value: &str, offset: usize) -> Result<u64, QueryError> {
    let value = value.to_ascii_lowercase();
    let mask = if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else if value.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        value.parse().ok()
    } else {
        value.bytes().try_fold(0, |mask, letter| {
            Some(
                mask | match letter {
                    b'r' => 0x0001,
                    b'h' => 0x0002,
                    b's' => 0x0004,
                    b'd' => 0x0010,
                    b'a' => 0x0020,
                    b'n' => 0x0080,
                    b't' => 0x0100,
                    b'l' => 0x0400,
                    b'c' => 0x0800,
                    b'o' => 0x1000,
                    b'i' => 0x2000,
                    b'e' => 0x4000,
                    _ => return None,
                },
            )
        })
    };
    mask.filter(|mask| *mask != 0)
        .ok_or_else(|| QueryError::new(offset, "attrib: 需要有效的属性字母组合或非零位掩码"))
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Compare {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}
impl Compare {
    fn matches(self, ordering: Ordering) -> bool {
        match self {
            Self::Eq => ordering == Ordering::Equal,
            Self::Ne => ordering != Ordering::Equal,
            Self::Lt => ordering == Ordering::Less,
            Self::Le => ordering != Ordering::Greater,
            Self::Gt => ordering == Ordering::Greater,
            Self::Ge => ordering != Ordering::Less,
        }
    }
}
fn comparator(text: &str) -> (Compare, &str) {
    for (prefix, compare) in [
        (">=", Compare::Ge),
        ("<=", Compare::Le),
        ("!=", Compare::Ne),
        ("==", Compare::Eq),
        (">", Compare::Gt),
        ("<", Compare::Lt),
        ("=", Compare::Eq),
    ] {
        if let Some(value) = text.strip_prefix(prefix) {
            return (compare, value);
        }
    }
    (Compare::Eq, text)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ByteBound {
    numerator: u128,
    denominator: u128,
}
impl ByteBound {
    fn compare(self, value: u64) -> Ordering {
        (u128::from(value) * self.denominator).cmp(&self.numerator)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SizePredicate {
    Compare(Compare, ByteBound),
    Range(Option<ByteBound>, Option<ByteBound>),
}
impl SizePredicate {
    pub(crate) fn parse(value: &str, offset: usize) -> Result<Self, QueryError> {
        let error = || {
            QueryError::new(
                offset,
                "size: 需要有效的非负字节数、比较式或区间，例如 >1MB、1KB..2MB",
            )
        };
        if value.is_empty() {
            return Err(error());
        }
        if let Some((start, end)) = value.split_once("..") {
            if end.contains("..") || start.is_empty() && end.is_empty() {
                return Err(error());
            }
            let start = if start.is_empty() {
                None
            } else {
                Some(bytes(start).ok_or_else(error)?)
            };
            let end = if end.is_empty() {
                None
            } else {
                Some(bytes(end).ok_or_else(error)?)
            };
            if matches!((start, end), (Some(a), Some(b)) if a.numerator * b.denominator > b.numerator * a.denominator)
            {
                return Err(QueryError::new(offset, "size: 区间下界不能大于上界"));
            }
            Ok(Self::Range(start, end))
        } else {
            let (compare, number) = comparator(value);
            Ok(Self::Compare(compare, bytes(number).ok_or_else(error)?))
        }
    }
    pub(crate) fn matches(&self, value: u64) -> bool {
        match self {
            Self::Compare(compare, bound) => compare.matches(bound.compare(value)),
            Self::Range(start, end) => {
                start.is_none_or(|start| start.compare(value) != Ordering::Less)
                    && end.is_none_or(|end| end.compare(value) != Ordering::Greater)
            }
        }
    }
}

/// Parse decimal values as exact rationals; never pass a large file size
/// through a floating-point number. SI-looking KB/MB aliases use 1024, the
/// compatibility convention used by the query language; KiB/MiB are explicit.
fn bytes(text: &str) -> Option<ByteBound> {
    let first_unit = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(first_unit);
    let factor: u128 = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        "p" | "pb" | "pib" => 1 << 50,
        _ => return None,
    };
    if number.is_empty() {
        return None;
    }
    let (integer, fraction) = number.split_once('.').unwrap_or((number, ""));
    if integer.is_empty()
        || !integer.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 9
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || number.ends_with('.')
    {
        return None;
    }
    let denominator = 10_u128.pow(fraction.len() as u32);
    let integer = integer.parse::<u128>().ok()?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
    };
    let numerator = integer
        .checked_mul(denominator)?
        .checked_add(fraction)?
        .checked_mul(factor)?;
    if numerator > u128::from(u64::MAX) * denominator {
        return None;
    }
    Some(ByteBound {
        numerator,
        denominator,
    })
}

#[derive(Debug, Clone)]
pub(crate) enum DatePredicate {
    Compare(Compare, Period),
    Range(Option<i64>, Option<i64>),
}
#[derive(Debug, Clone, Copy)]
pub(crate) struct Period {
    start: i64,
    end: i64,
}
impl DatePredicate {
    pub(crate) fn parse(
        value: &str,
        now: i64,
        offset: usize,
        local_dates: bool,
    ) -> Result<Self, QueryError> {
        let error = || {
            QueryError::new(
                offset,
                "日期无效；使用 YYYY、YYYY-MM、YYYY-MM-DD、ISO UTC 时间、today/yesterday、utc: 或明确区间",
            )
        };
        let (value, local_dates) = match value.strip_prefix("utc:") {
            Some(value) => (value, false),
            None => (value, local_dates),
        };
        if let Some((start, end)) = value.split_once("..") {
            if end.contains("..") || start.is_empty() && end.is_empty() {
                return Err(error());
            }
            let start = if start.is_empty() {
                None
            } else {
                Some(period(start, now, local_dates).ok_or_else(error)?.start)
            };
            let end = if end.is_empty() {
                None
            } else {
                Some(period(end, now, local_dates).ok_or_else(error)?.end)
            };
            if matches!((start, end), (Some(start), Some(end)) if start >= end) {
                return Err(QueryError::new(offset, "日期区间下界不能晚于上界"));
            }
            Ok(Self::Range(start, end))
        } else {
            let (compare, value) = comparator(value);
            Ok(Self::Compare(
                compare,
                period(value, now, local_dates).ok_or_else(error)?,
            ))
        }
    }
    pub(crate) fn matches(&self, instant: i64) -> bool {
        match self {
            Self::Range(start, end) => {
                start.is_none_or(|start| instant >= start) && end.is_none_or(|end| instant < end)
            }
            Self::Compare(compare, period) => match compare {
                Compare::Eq => instant >= period.start && instant < period.end,
                Compare::Ne => instant < period.start || instant >= period.end,
                Compare::Lt => instant < period.start,
                Compare::Le => instant < period.end,
                Compare::Gt => instant >= period.end,
                Compare::Ge => instant >= period.start,
            },
        }
    }
}

fn period(text: &str, now: i64, local_dates: bool) -> Option<Period> {
    if local_dates {
        period_with_timezone(text, now, Some(&Local))
    } else {
        period_with_timezone::<Local>(text, now, None)
    }
}

fn period_with_timezone<Tz: TimeZone>(
    text: &str,
    now: i64,
    timezone: Option<&Tz>,
) -> Option<Period> {
    let lowercase = text.to_ascii_lowercase();
    if timezone.is_none() && matches!(lowercase.as_str(), "today" | "yesterday") {
        let day = now.div_euclid(86400).checked_mul(86400)?;
        return if lowercase == "today" {
            Some(Period {
                start: day,
                end: day.checked_add(86400)?,
            })
        } else {
            Some(Period {
                start: day.checked_sub(86400)?,
                end: day,
            })
        };
    }
    let today = if let Some(timezone) = timezone {
        DateTime::<Utc>::from_timestamp(now, 0)?
            .with_timezone(timezone)
            .date_naive()
    } else {
        let (year, month, day) = civil_from_days(now.div_euclid(86400));
        NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)?
    };
    let midnight = |date: NaiveDate| {
        if let Some(timezone) = timezone {
            local_midnight(timezone, date)
        } else {
            Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp())
        }
    };
    match lowercase.as_str() {
        "today" => {
            return Some(Period {
                start: midnight(today)?,
                end: midnight(today.succ_opt()?)?,
            });
        }
        "yesterday" => {
            return Some(Period {
                start: midnight(today.pred_opt()?)?,
                end: midnight(today)?,
            });
        }
        "thisyear" | "thismonth" => {
            let start_date = NaiveDate::from_ymd_opt(
                today.year(),
                if lowercase == "thisyear" {
                    1
                } else {
                    today.month()
                },
                1,
            )?;
            let end_date = if lowercase == "thisyear" || today.month() == 12 {
                NaiveDate::from_ymd_opt(today.year().checked_add(1)?, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)?
            };
            return Some(Period {
                start: midnight(start_date)?,
                end: midnight(end_date)?,
            });
        }
        _ => (),
    }
    if let Some(days) = lowercase.strip_suffix("days") {
        let count = days.parse::<i64>().ok()?;
        if !(1..=366000).contains(&count) {
            return None;
        }
        return Some(Period {
            start: now.checked_sub(count.checked_mul(86400)?)?,
            end: now.checked_add(1)?,
        });
    }
    if let Some(timestamp) = text.strip_prefix('@') {
        let start = timestamp.parse::<i64>().ok()?;
        return Some(Period {
            start,
            end: start.checked_add(1)?,
        });
    }
    // Accept an explicitly UTC timestamp or an unambiguous UTC civil time.
    if text.len() == 20 && text.ends_with('Z') || text.len() == 19 {
        if text.as_bytes().get(10) != Some(&b'T') {
            return None;
        }
        let date = parse_civil(text.get(..10)?)?;
        let time = text.get(11..19)?;
        if time.as_bytes().get(2) != Some(&b':') || time.as_bytes().get(5) != Some(&b':') {
            return None;
        }
        let hour = ascii_number(time.get(..2)?)?;
        let minute = ascii_number(time.get(3..5)?)?;
        let second = ascii_number(time.get(6..8)?)?;
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let start = days_from_civil(date.0, date.1, date.2)?
            .checked_mul(86400)?
            .checked_add(hour * 3600 + minute * 60 + second)?;
        return Some(Period {
            start,
            end: start.checked_add(1)?,
        });
    }
    let (year, month, day) = parse_civil(text)?;
    let date = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month as u32, day as u32)?;
    let start = midnight(date)?;
    let end = match text.len() {
        4 => midnight(NaiveDate::from_ymd_opt((year + 1) as i32, 1, 1)?)?,
        7 => {
            let next = if month == 12 {
                NaiveDate::from_ymd_opt((year + 1) as i32, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(year as i32, (month + 1) as u32, 1)?
            };
            midnight(next)?
        }
        10 => midnight(date.succ_opt()?)?,
        _ => return None,
    };
    Some(Period { start, end })
}

fn local_midnight<Tz: TimeZone>(timezone: &Tz, date: NaiveDate) -> Option<i64> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    match timezone.from_local_datetime(&midnight) {
        LocalResult::Single(value) => Some(value.timestamp()),
        LocalResult::Ambiguous(first, second) => Some(first.timestamp().min(second.timestamp())),
        LocalResult::None => {
            for minute in 1..=24 * 60 {
                let candidate = midnight.checked_add_signed(chrono::TimeDelta::minutes(minute))?;
                match timezone.from_local_datetime(&candidate) {
                    LocalResult::Single(value) => return Some(value.timestamp()),
                    LocalResult::Ambiguous(first, second) => {
                        return Some(first.timestamp().min(second.timestamp()));
                    }
                    LocalResult::None => (),
                }
            }
            None
        }
    }
}
fn ascii_number(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}
fn parse_civil(text: &str) -> Option<(i64, i64, i64)> {
    if !matches!(text.len(), 4 | 7 | 10) || !text.is_ascii() {
        return None;
    }
    let year = ascii_number(&text[..4])?;
    if !(1601..=9999).contains(&year) {
        return None;
    }
    let month = if text.len() >= 7 {
        if text.as_bytes()[4] != b'-' {
            return None;
        }
        ascii_number(&text[5..7])?
    } else {
        1
    };
    let day = if text.len() == 10 {
        if text.as_bytes()[7] != b'-' {
            return None;
        }
        ascii_number(&text[8..10])?
    } else {
        1
    };
    days_from_civil(year, month, day)?;
    Some((year, month, day))
}

// Proleptic Gregorian civil-date conversion, expressed with integer arithmetic.
// The epoch-offset/era decomposition is a standard calendar algorithm, not a
// recovered application-specific date table. Dates are validated first.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1601..=10000).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_length = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=month_length).contains(&day) {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146097 + day_of_era - 719468)
}
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Clamp only date-derived convenience expressions at the supported civil
    // calendar boundary. Absolute timestamp predicates remain exact i64 values.
    let days = days.clamp(-134774, 2932896);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimal_size_is_exact_above_float_precision() {
        let query = SizePredicate::parse("=9007199254740993", 0).unwrap();
        assert!(query.matches(9007199254740993));
        assert!(!query.matches(9007199254740992));
        assert!(SizePredicate::parse("1.5KB", 0).unwrap().matches(1536));
        assert!(SizePredicate::parse(">1.1B", 0).unwrap().matches(2));
        assert!(!SizePredicate::parse("=1.1B", 0).unwrap().matches(1));
        assert!(
            SizePredicate::parse("18446744073709551615", 0)
                .unwrap()
                .matches(u64::MAX)
        );
        assert!(SizePredicate::parse("18446744073709551616", 0).is_err());
    }
    #[test]
    fn civil_epoch_leap_and_roundtrip() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1969, 12, 31), Some(-1));
        assert!(days_from_civil(1900, 2, 29).is_none());
        assert!(days_from_civil(2000, 2, 29).is_some());
        for year in [1601, 1900, 1970, 2000, 2026, 9999] {
            for month in 1..=12 {
                let days = days_from_civil(year, month, 1).unwrap();
                assert_eq!(civil_from_days(days), (year, month, 1));
            }
        }
    }

    #[test]
    fn local_calendar_days_follow_daylight_saving_transitions() {
        use chrono_tz::America::New_York;

        let spring_noon = days_from_civil(2026, 3, 8).unwrap() * 86400 + 12 * 3600;
        let spring = period_with_timezone("today", spring_noon, Some(&New_York)).unwrap();
        assert_eq!(spring.end - spring.start, 23 * 3600);
        assert_eq!(
            spring.start,
            days_from_civil(2026, 3, 8).unwrap() * 86400 + 5 * 3600
        );

        let autumn_noon = days_from_civil(2026, 11, 1).unwrap() * 86400 + 17 * 3600;
        let autumn = period_with_timezone("today", autumn_noon, Some(&New_York)).unwrap();
        assert_eq!(autumn.end - autumn.start, 25 * 3600);
        assert_eq!(
            autumn.start,
            days_from_civil(2026, 11, 1).unwrap() * 86400 + 4 * 3600
        );
    }

    #[test]
    fn local_date_periods_use_local_calendar_boundaries() {
        use chrono_tz::Asia::Shanghai;

        let now = days_from_civil(2026, 11, 8).unwrap() * 86400 + 2 * 3600;
        let local_today = period_with_timezone("today", now, Some(&Shanghai)).unwrap();
        assert_eq!(
            local_today.start,
            days_from_civil(2026, 11, 7).unwrap() * 86400 + 16 * 3600
        );
        assert_eq!(
            local_today.end,
            days_from_civil(2026, 11, 8).unwrap() * 86400 + 16 * 3600
        );

        let local_month = period_with_timezone("2026-11", now, Some(&Shanghai)).unwrap();
        assert_eq!(
            local_month.start,
            days_from_civil(2026, 10, 31).unwrap() * 86400 + 16 * 3600
        );
    }
}
