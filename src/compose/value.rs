//! The model of a compose file between its stages, as compose-go keeps it: the values
//! go-yaml decodes into `any`, mappings keyed by strings. Scalars keep the type YAML
//! gave them until a stage converts them, and print as Go prints them.

use std::collections::BTreeMap;
use std::fmt;

pub type Map = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// An integer above i64::MAX.
    Uint(u64),
    Float(f64),
    Str(String),
    /// A plain scalar YAML reads as a timestamp.
    Time(Time),
    Seq(Vec<Value>),
    Map(Map),
}

impl Value {
    pub fn str(text: impl Into<String>) -> Value {
        Value::Str(text.into())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_map_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Seq(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The value at a path of mapping keys, when every step is a mapping.
    pub fn get(&self, keys: &[&str]) -> Option<&Value> {
        keys.iter()
            .try_fold(self, |value, key| value.as_map()?.get(*key))
    }

    /// The name of the value's Go type, for the messages that name it.
    pub fn go_type(&self) -> &'static str {
        match self {
            Value::Null => "<nil>",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Uint(_) => "uint64",
            Value::Float(_) => "float64",
            Value::Str(_) => "string",
            Value::Time(_) => "time.Time",
            Value::Seq(_) => "[]interface {}",
            Value::Map(_) => "map[string]interface {}",
        }
    }
}

/// A scalar as Go's fmt prints it with %v: how a number or a date is written where a
/// string goes.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("<nil>"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Uint(u) => write!(f, "{u}"),
            Value::Float(x) => f.write_str(&go_float(*x)),
            Value::Str(s) => f.write_str(s),
            Value::Time(t) => write!(f, "{t}"),
            Value::Seq(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Value::Map(map) => {
                f.write_str("map[")?;
                for (i, (key, item)) in map.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" ")?;
                    }
                    write!(f, "{key}:{item}")?;
                }
                f.write_str("]")
            }
        }
    }
}

/// A float64 as Go writes it with %v, strconv's 'g' with the shortest precision: with an
/// exponent below -4 or from 6 up, `1e+06`, `1e-05`; plain digits otherwise.
pub fn go_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    // Rust's {:e} gives the shortest digits that read back the same, as Go's do.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exponent) = sci.split_once('e').unwrap();
    let exponent: i32 = exponent.parse().unwrap();
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let sign = if x < 0.0 { "-" } else { "" };
    if !(-4..6).contains(&exponent) {
        let rest = &digits[1..];
        let dot = if rest.is_empty() { "" } else { "." };
        let (esign, eabs) = if exponent < 0 {
            ('-', -exponent)
        } else {
            ('+', exponent)
        };
        return format!("{sign}{}{dot}{rest}e{esign}{eabs:02}", &digits[..1]);
    }
    let point = exponent + 1;
    if point <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!(
            "{sign}{digits}{}",
            "0".repeat(point as usize - digits.len())
        )
    } else {
        let (whole, fraction) = digits.split_at(point as usize);
        format!("{sign}{whole}.{fraction}")
    }
}

/// A timestamp of a YAML plain scalar, in the zone it was written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Time {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nanosecond: u32,
    /// Seconds east of UTC as written (`+00:00` too); None for `Z` or no zone.
    pub offset: Option<i32>,
}

/// getnum: one or two digits, exactly two when `fixed`.
fn getnum(s: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
    let digit = |i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
    if !digit(0) {
        return None;
    }
    if !digit(1) {
        if fixed {
            return None;
        }
        return Some((u32::from(s[0] - b'0'), &s[1..]));
    }
    Some((
        u32::from(s[0] - b'0') * 10 + u32::from(s[1] - b'0'),
        &s[2..],
    ))
}

fn days_in(month: u32, year: i64) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        _ => 28,
    }
}

impl Time {
    /// go-yaml's parseTimestamp: four digits and a `-`, then the first of its layouts
    /// Go's time.Parse reads the text with: a date and time separated by `T` or `t`
    /// with a zone, by a space without one, or a date alone.
    pub fn parse(text: &str) -> Option<Time> {
        let bytes = text.as_bytes();
        let year_end = bytes.iter().position(|b| !b.is_ascii_digit())?;
        if year_end != 4 || bytes[4] != b'-' {
            return None;
        }
        [Some(b'T'), Some(b't'), Some(b' '), None]
            .into_iter()
            .find_map(|separator| Self::parse_layout(bytes, separator))
    }

    fn parse_layout(value: &[u8], separator: Option<u8>) -> Option<Time> {
        let year: i64 = std::str::from_utf8(&value[..4]).ok()?.parse().ok()?;
        let value = value[4..].strip_prefix(b"-")?;
        let (month, value) = getnum(value, false)?;
        if !(1..=12).contains(&month) {
            return None;
        }
        let value = value.strip_prefix(b"-")?;
        let (day, mut value) = getnum(value, false)?;
        let mut time = Time {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            nanosecond: 0,
            offset: None,
        };
        if let Some(separator) = separator {
            value = value.strip_prefix(&[separator])?;
            let (hour, rest) = getnum(value, false)?;
            let (minute, rest) = getnum(rest.strip_prefix(b":")?, false)?;
            let (second, rest) = getnum(rest.strip_prefix(b":")?, false)?;
            if hour >= 24 || minute >= 60 || second >= 60 {
                return None;
            }
            (time.hour, time.minute, time.second) = (hour, minute, second);
            value = rest;
            // .999999999: any number of digits, after a period or a comma, the first
            // nine counted.
            if value.len() >= 2 && matches!(value[0], b'.' | b',') && value[1].is_ascii_digit() {
                let digits = value[1..].iter().take_while(|b| b.is_ascii_digit()).count();
                let kept = &value[1..1 + digits.min(9)];
                let mut ns: u32 = std::str::from_utf8(kept).ok()?.parse().ok()?;
                for _ in kept.len()..9 {
                    ns *= 10;
                }
                time.nanosecond = ns;
                value = &value[1 + digits..];
            }
            if separator != b' ' {
                // Z07:00.
                if let Some(rest) = value.strip_prefix(b"Z") {
                    value = rest;
                } else {
                    if value.len() < 6 || value[3] != b':' {
                        return None;
                    }
                    let (hours, _) = getnum(&value[1..3], true)?;
                    let (minutes, _) = getnum(&value[4..6], true)?;
                    if hours > 24 || minutes > 60 {
                        return None;
                    }
                    let seconds = (hours * 60 + minutes) as i32 * 60;
                    time.offset = Some(match value[0] {
                        b'+' => seconds,
                        b'-' => -seconds,
                        _ => return None,
                    });
                    value = &value[6..];
                }
            }
        }
        if !value.is_empty() || day < 1 || day > days_in(month, year) {
            return None;
        }
        Some(time)
    }

    /// RFC 3339 with the fractions it has, as Go's json writes a time.Time; Go's
    /// MarshalJSON refuses an offset of 24 hours or more.
    pub fn rfc3339(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{}{}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.fraction(),
            match self.offset {
                None | Some(0) => "Z".to_string(),
                Some(offset) => format!("{}:{:02}", zone_hours(offset), offset.abs() / 60 % 60),
            }
        )
    }

    /// The error of Go's MarshalJSON, for a time RFC 3339 cannot write.
    pub fn json_error(&self) -> Option<&'static str> {
        if !(0..=9999).contains(&self.year) {
            return Some("Time.MarshalJSON: year outside of range [0,9999]");
        }
        match self.offset {
            Some(offset) if offset.abs() / 3600 >= 24 => {
                Some("Time.MarshalJSON: timezone hour outside of range [0,23]")
            }
            _ => None,
        }
    }

    /// Go's time.Time.GoString, as %#v prints a time: its zone UTC, the local zone
    /// when that has the time's offset then, or a zone with no name.
    pub fn go_string(&self) -> String {
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        let zone = match self.offset {
            None => "time.UTC".to_string(),
            Some(_) => {
                if self.is_local() {
                    "time.Local".to_string()
                } else {
                    "time.Location(\"\")".to_string()
                }
            }
        };
        format!(
            "time.Date({}, time.{}, {}, {}, {}, {}, {}, {zone})",
            self.year,
            MONTHS[self.month as usize - 1],
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.nanosecond
        )
    }

    /// Seconds since the Unix epoch.
    pub fn unix(&self) -> i64 {
        super::gotime::days_from_epoch(self.year) * 86_400
            + i64::from(self.day_of_year() - 1) * 86_400
            + i64::from(self.hour * 3600 + self.minute * 60 + self.second)
            - i64::from(self.offset.unwrap_or(0))
    }

    /// Whether two times of this zone share Go's *Location: UTC, the local zone, and
    /// the unnamed zones FixedZone keeps one of each, the whole hours from -12 to +14.
    pub fn shared_zone(&self) -> bool {
        match self.offset {
            None => true,
            Some(offset) => {
                (offset % 3600 == 0 && (-12..=14).contains(&(offset / 3600))) || self.is_local()
            }
        }
    }

    /// Whether time.Parse gave this time the local zone: its offset is the one the
    /// local zone has at that moment.
    pub fn is_local(&self) -> bool {
        self.offset
            .is_some_and(|offset| super::gotime::local_offset(self.unix()) == i64::from(offset))
    }

    fn day_of_year(&self) -> u32 {
        (1..self.month).map(|m| days_in(m, self.year)).sum::<u32>() + self.day
    }

    fn fraction(&self) -> String {
        if self.nanosecond == 0 {
            return String::new();
        }
        format!(".{:09}", self.nanosecond)
            .trim_end_matches('0')
            .to_string()
    }
}

fn zone_hours(offset: i32) -> String {
    format!(
        "{}{:02}",
        if offset < 0 { '-' } else { '+' },
        offset.abs() / 3600
    )
}

/// Go's time.Time.String: `2006-01-02 15:04:05.999999999 -0700 MST`, the zone of a
/// time parsed with an offset named by the offset itself, unless time.Parse put it in
/// the local zone, whose name it then has.
impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}{} ",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
            self.fraction()
        )?;
        match self.offset {
            None => f.write_str("+0000 UTC"),
            Some(offset) => {
                let zone = format!("{}{:02}", zone_hours(offset), offset.abs() / 60 % 60);
                let name = if self.is_local() {
                    Some(super::gotime::local_at(self.unix()).1).filter(|n| !n.is_empty())
                } else {
                    None
                };
                write!(f, "{zone} {}", name.as_deref().unwrap_or(&zone))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_as_go_s_time_parse() {
        let t = Time::parse("2001-12-14T21:59:43,123456789123Z").unwrap();
        assert_eq!(t.nanosecond, 123_456_789);
        assert_eq!(Time::parse("2001-1-2").unwrap().day, 2);
        assert!(Time::parse("2001-12-14T21:59:43+25:00").is_none());
        assert!(Time::parse("2001-12-14T21:59:43+05:61").is_none());
        assert!(Time::parse("2001-02-30").is_none());
        let fixed = Time::parse("2001-12-14T21:59:43+24:00").unwrap();
        assert_eq!(fixed.rfc3339(), "2001-12-14T21:59:43+24:00");
        assert!(fixed.json_error().is_some());
        assert_eq!(
            Time::parse("2001-12-14T21:59:43+00:00").unwrap().rfc3339(),
            "2001-12-14T21:59:43Z"
        );
        assert_eq!(
            Time::parse("2001-12-14").unwrap().go_string(),
            "time.Date(2001, time.December, 14, 0, 0, 0, 0, time.UTC)"
        );
    }

    #[test]
    fn floats_print_as_go_prints_them() {
        for (x, go) in [
            (1000.0, "1000"),
            (100000.0, "100000"),
            (1e6, "1e+06"),
            (1e20, "1e+20"),
            (0.5, "0.5"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123456789.0, "1.23456789e+08"),
            (1.5e300, "1.5e+300"),
            (-2.25, "-2.25"),
            (-0.0, "-0"),
            (1.0, "1"),
            (f64::INFINITY, "+Inf"),
        ] {
            assert_eq!(go_float(x), go, "{x}");
        }
    }

    #[test]
    fn timestamps_read_and_print_as_go_does() {
        let date = Time::parse("2001-12-14").unwrap();
        assert_eq!(date.to_string(), "2001-12-14 00:00:00 +0000 UTC");
        assert_eq!(date.rfc3339(), "2001-12-14T00:00:00Z");
        let spaced = Time::parse("2001-12-14 21:59:43.10").unwrap();
        assert_eq!(spaced.to_string(), "2001-12-14 21:59:43.1 +0000 UTC");
        let zoned = Time::parse("2001-12-14T21:59:43.5+05:30").unwrap();
        assert_eq!(zoned.to_string(), "2001-12-14 21:59:43.5 +0530 +0530");
        assert_eq!(zoned.rfc3339(), "2001-12-14T21:59:43.5+05:30");
        assert_eq!(
            Time::parse("2001-1-2t3:4:5Z").unwrap().to_string(),
            "2001-01-02 03:04:05 +0000 UTC"
        );
        for not in [
            "2001-12-14 21:59:43.10 -5",
            "2001-13-01",
            "2001-02-30",
            "201-12-14",
            "2001-12-14x",
            "12001-12-14",
        ] {
            assert_eq!(Time::parse(not), None, "{not}");
        }
    }
}
