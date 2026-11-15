//! GitHub's timestamps (`2026-10-19T08:30:00Z`, `updatedAt`, `createdAt`) as dv keeps time:
//! milliseconds since the Unix epoch.

/// `text` in ms since the Unix epoch, when it is a UTC timestamp as GitHub writes them:
/// `YYYY-MM-DDTHH:MM:SSZ`, with or without a fraction of a second.
pub fn parse_timestamp(text: &str) -> Option<u64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let number = |text: &str, len: usize| {
        (text.len() == len && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
    };
    let mut date = date.split('-');
    let (year, month, day) = (
        number(date.next()?, 4)?,
        number(date.next()?, 2)?,
        number(date.next()?, 2)?,
    );
    let (time, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut time = time.split(':');
    let (hour, minute, second) = (
        number(time.next()?, 2)?,
        number(time.next()?, 2)?,
        number(time.next()?, 2)?,
    );
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    let in_range = date.next().is_none()
        && time.next().is_none()
        && (1..=12).contains(&month)
        && (1..=days_in_month).contains(&day)
        && hour < 24
        && minute < 60
        && second < 61
        && fraction.bytes().all(|b| b.is_ascii_digit());
    if !in_range || year < 1970 {
        return None;
    }
    let millis = match fraction.get(..fraction.len().min(3)) {
        Some(digits) if !digits.is_empty() => {
            digits.parse::<u64>().ok()? * 10u64.pow(3 - digits.len() as u32)
        }
        _ => 0,
    };
    // Days since 1970-01-01 in the proleptic Gregorian calendar, the year starting in March
    // so a leap day is its last.
    let (year, month) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let (era, year_of_era) = (year / 400, year % 400);
    let day_of_year = (153 * month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}
