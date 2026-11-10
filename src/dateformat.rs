//! Timestamp formatting compatible with the masks of the npm `dateformat` package that node-zsync
//! used for snapshot names (default `yyyymmddHHMMssl`).
//!
//! Supported tokens: `d dd ddd dddd m mm mmm mmmm yy yyyy h hh H HH M MM s ss l L t tt T TT Z o S`,
//! quoted literals (`'...'` or `"..."`), the named masks (`isoDateTime`, ...) and the `UTC:` prefix.

use chrono::{DateTime, Datelike, FixedOffset, Local, Offset, TimeZone, Timelike, Utc};

pub const DEFAULT_MASK: &str = "yyyymmddHHMMssl";

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const DAYS_LONG: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTHS_LONG: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn named_mask(name: &str) -> Option<&'static str> {
    Some(match name {
        "default" => "ddd mmm dd yyyy HH:MM:ss",
        "shortDate" => "m/d/yy",
        "mediumDate" => "mmm d, yyyy",
        "longDate" => "mmmm d, yyyy",
        "fullDate" => "dddd, mmmm d, yyyy",
        "shortTime" => "h:MM TT",
        "mediumTime" => "h:MM:ss TT",
        "longTime" => "h:MM:ss TT Z",
        "isoDate" => "yyyy-mm-dd",
        "isoTime" => "HH:MM:ss",
        "isoDateTime" => "yyyy-mm-dd'T'HH:MM:ss",
        "isoUtcDateTime" => "UTC:yyyy-mm-dd'T'HH:MM:ss'Z'",
        _ => return None,
    })
}

/// Format the current local time with `mask`.
pub fn format_now(mask: &str) -> String {
    format_at(mask, &Local::now())
}

/// Format `time` with `mask`. A `UTC:` prefix converts to UTC first.
pub fn format_at<Tz: TimeZone>(mask: &str, time: &DateTime<Tz>) -> String {
    let mask = if mask.is_empty() { DEFAULT_MASK } else { mask };
    let mask = named_mask(mask).unwrap_or(mask);
    let (utc, mask) = match mask.strip_prefix("UTC:") {
        Some(rest) => (true, rest),
        None => (false, mask),
    };
    let fixed: DateTime<FixedOffset> = if utc {
        time.with_timezone(&Utc).fixed_offset()
    } else {
        let off = time.offset().fix();
        time.with_timezone(&off)
    };
    render(mask, &fixed, utc)
}

fn render(mask: &str, t: &DateTime<FixedOffset>, utc: bool) -> String {
    let chars: Vec<char> = mask.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let run = |i: usize, c: char, max: usize| -> usize {
        let mut n = 0;
        while i + n < chars.len() && chars[i + n] == c && n < max {
            n += 1;
        }
        n
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' => {
                // Quoted literal; an unterminated quote is output as-is.
                match chars[i + 1..].iter().position(|&x| x == c) {
                    Some(end) => {
                        out.extend(&chars[i + 1..i + 1 + end]);
                        i += end + 2;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            'd' => {
                let n = run(i, 'd', 4);
                let wd = t.weekday().num_days_from_sunday() as usize;
                out.push_str(&match n {
                    1 => t.day().to_string(),
                    2 => format!("{:02}", t.day()),
                    3 => DAYS[wd].to_string(),
                    _ => DAYS_LONG[wd].to_string(),
                });
                i += n;
            }
            'm' => {
                let n = run(i, 'm', 4);
                let m0 = t.month0() as usize;
                out.push_str(&match n {
                    1 => t.month().to_string(),
                    2 => format!("{:02}", t.month()),
                    3 => MONTHS[m0].to_string(),
                    _ => MONTHS_LONG[m0].to_string(),
                });
                i += n;
            }
            'y' => {
                let n = run(i, 'y', 4);
                if n >= 4 {
                    out.push_str(&format!("{:04}", t.year()));
                    i += 4;
                } else if n >= 2 {
                    out.push_str(&format!("{:02}", t.year().rem_euclid(100)));
                    i += 2;
                } else {
                    out.push('y');
                    i += 1;
                }
            }
            'h' | 'H' | 'M' | 's' | 't' | 'T' => {
                let n = run(i, c, 2);
                let h12 = if t.hour() % 12 == 0 { 12 } else { t.hour() % 12 };
                let am = t.hour() < 12;
                let v = match c {
                    'h' => pad(h12, n),
                    'H' => pad(t.hour(), n),
                    'M' => pad(t.minute(), n),
                    's' => pad(t.second(), n),
                    't' => (if am { "am" } else { "pm" })[..n].to_string(),
                    _ => (if am { "AM" } else { "PM" })[..n].to_string(),
                };
                out.push_str(&v);
                i += n;
            }
            'l' => {
                out.push_str(&format!("{:03}", t.timestamp_subsec_millis()));
                i += 1;
            }
            'L' => {
                out.push_str(&format!("{:02}", t.timestamp_subsec_millis() / 10));
                i += 1;
            }
            'o' => {
                out.push_str(&offset_hhmm(t, ""));
                i += 1;
            }
            'Z' => {
                if utc {
                    out.push_str("UTC");
                } else {
                    out.push_str(&format!("GMT{}", offset_hhmm(t, "")));
                }
                i += 1;
            }
            'S' => {
                let d = t.day();
                let suffix = if (11..=13).contains(&(d % 100)) {
                    "th"
                } else {
                    match d % 10 {
                        1 => "st",
                        2 => "nd",
                        3 => "rd",
                        _ => "th",
                    }
                };
                out.push_str(suffix);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn pad(v: u32, width: usize) -> String {
    if width >= 2 {
        format!("{v:02}")
    } else {
        v.to_string()
    }
}

fn offset_hhmm(t: &DateTime<FixedOffset>, sep: &str) -> String {
    let secs = t.offset().local_minus_utc();
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.abs();
    format!("{sign}{:02}{sep}{:02}", secs / 3600, (secs % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<FixedOffset> {
        FixedOffset::west_opt(5 * 3600)
            .unwrap()
            .with_ymd_and_hms(2024, 3, 2, 7, 4, 9)
            .unwrap()
            .checked_add_signed(chrono::Duration::milliseconds(45))
            .unwrap()
    }

    #[test]
    fn default_mask_is_sortable_digits() {
        assert_eq!(format_at(DEFAULT_MASK, &at()), "20240302070409045");
        assert_eq!(format_at("", &at()), "20240302070409045");
    }

    #[test]
    fn tokens() {
        let t = at();
        assert_eq!(format_at("yy-m-d", &t), "24-3-2");
        assert_eq!(format_at("ddd dddd mmm mmmm", &t), "Sat Saturday Mar March");
        assert_eq!(format_at("h:MM TT tt T t", &t), "7:04 AM am A a");
        assert_eq!(format_at("hh HH", &t), "07 07");
        assert_eq!(format_at("s l L", &t), "9 045 04");
        assert_eq!(format_at("o Z", &t), "-0500 GMT-0500");
        assert_eq!(format_at("dS", &t), "2nd");
        assert_eq!(format_at("'yyyy'-yyyy", &t), "yyyy-2024");
        assert_eq!(format_at("\"at\" HH", &t), "at 07");
        assert_eq!(format_at("isoDate", &t), "2024-03-02");
        assert_eq!(format_at("isoUtcDateTime", &t), "2024-03-02T12:04:09Z");
        assert_eq!(format_at("UTC:HH Z", &t), "12 UTC");
    }

    #[test]
    fn ordinals() {
        let mk = |d| FixedOffset::east_opt(0).unwrap().with_ymd_and_hms(2024, 1, d, 0, 0, 0).unwrap();
        assert_eq!(format_at("S", &mk(1)), "st");
        assert_eq!(format_at("S", &mk(11)), "th");
        assert_eq!(format_at("S", &mk(22)), "nd");
        assert_eq!(format_at("S", &mk(23)), "rd");
    }
}
