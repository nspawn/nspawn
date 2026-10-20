//! Go's local time, for the time logrus writes on the warnings: the zone
//! of TZ, or of /etc/localtime, read as Go reads a zoneinfo file, with the POSIX rule at
//! its end for the years past its last transition.

const SECONDS_PER_DAY: i64 = 86_400;

/// The offset from UTC, in seconds, of the local zone at a Unix time, as Go's time.Local
/// has it; UTC when Go would fall back to it.
pub fn local_offset(unix: i64) -> i64 {
    local_zone().map_or(0, |zone| zone.offset(unix))
}

/// Go's initLocal: TZ names a zone (a path, or a name under the zoneinfo directories,
/// the first of which holds valid data), is empty or UTC for UTC; without TZ,
/// /etc/localtime.
fn local_zone() -> Option<Location> {
    let read = |path: &str| std::fs::read(path).ok().and_then(|d| Location::parse(&d));
    match std::env::var_os("TZ") {
        None => read("/etc/localtime"),
        Some(tz) => {
            let tz = tz.to_string_lossy();
            let tz = tz.strip_prefix(':').unwrap_or(&tz);
            if tz.is_empty() || tz == "UTC" {
                return None;
            }
            if tz.starts_with('/') {
                return read(tz);
            }
            [
                "/usr/share/zoneinfo/",
                "/usr/share/lib/zoneinfo/",
                "/usr/lib/locale/TZ/",
                "/etc/zoneinfo/",
            ]
            .iter()
            .find_map(|dir| read(&format!("{dir}{tz}")))
        }
    }
}

struct Zone {
    offset: i64,
    dst: bool,
}

/// What Go keeps of a zoneinfo file.
struct Location {
    zones: Vec<Zone>,
    /// Transitions: when, and the zone from then on.
    tx: Vec<(i64, usize)>,
    /// The POSIX rule for the times past the last transition.
    extend: String,
}

/// Bytes read in order, as Go's dataIO reads them.
struct Data<'a> {
    rest: &'a [u8],
}

impl<'a> Data<'a> {
    fn read(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest.len() < n {
            return None;
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Some(head)
    }

    fn big4(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.read(4)?.try_into().ok()?))
    }

    fn big8(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.read(8)?.try_into().ok()?))
    }
}

impl Location {
    /// LoadLocationFromTZData: None where Go finds the data bad.
    fn parse(data: &[u8]) -> Option<Location> {
        let mut d = Data { rest: data };
        if d.read(4)? != b"TZif" {
            return None;
        }
        let version = match d.read(16)?[0] {
            0 => 1,
            b'2' => 2,
            b'3' => 3,
            _ => return None,
        };
        let mut n = [0usize; 6];
        for slot in &mut n {
            *slot = d.big4()? as usize;
        }
        let [isut, isstd, leap, time, typ, chars] = n;
        let wide = version > 1;
        if wide {
            // The 32-bit data first, then its header again for the 64-bit data.
            d.read(time * 5 + typ * 6 + chars + leap * 8 + isstd + isut + 4 + 16)?;
            for slot in &mut n {
                *slot = d.big4()? as usize;
            }
        }
        let [isut, isstd, leap, time, typ, chars] = n;
        let size = if wide { 8 } else { 4 };
        let mut times = Data {
            rest: d.read(time * size)?,
        };
        let indexes = d.read(time)?;
        let mut zone_data = Data {
            rest: d.read(typ * 6)?,
        };
        let abbrev = d.read(chars)?;
        d.read(leap * (size + 4))?;
        d.read(isstd)?;
        d.read(isut)?;
        let rest = d.rest;
        let extend = if rest.len() > 2 && rest[0] == b'\n' && rest[rest.len() - 1] == b'\n' {
            String::from_utf8_lossy(&rest[1..rest.len() - 1]).into_owned()
        } else {
            String::new()
        };
        if typ == 0 {
            return None;
        }
        let mut zones = Vec::with_capacity(typ);
        for _ in 0..typ {
            let offset = i64::from(zone_data.big4()? as i32);
            let dst = zone_data.read(1)?[0] != 0;
            if usize::from(zone_data.read(1)?[0]) >= abbrev.len() {
                return None;
            }
            zones.push(Zone { offset, dst });
        }
        let mut tx = Vec::with_capacity(time);
        for &index in indexes {
            let when = if wide {
                times.big8()? as i64
            } else {
                i64::from(times.big4()? as i32)
            };
            let index = usize::from(index);
            if index >= zones.len() {
                return None;
            }
            tx.push((when, index));
        }
        if tx.is_empty() {
            tx.push((i64::MIN, 0));
        }
        Some(Location { zones, tx, extend })
    }

    /// Location.lookup's offset at a moment.
    fn offset(&self, unix: i64) -> i64 {
        if unix < self.tx[0].0 {
            return self.zones[first_zone(&self.zones, &self.tx)].offset;
        }
        let lo = self.tx.partition_point(|&(when, _)| when <= unix) - 1;
        if lo == self.tx.len() - 1 && !self.extend.is_empty() {
            if let Some(offset) = tzset(&self.extend, unix) {
                return offset;
            }
        }
        self.zones[self.tx[lo].1].offset
    }
}

/// lookupFirstZone: the zone before the first transition.
fn first_zone(zones: &[Zone], tx: &[(i64, usize)]) -> usize {
    if !tx.iter().any(|&(_, i)| i == 0) {
        return 0;
    }
    if let Some(&(_, first)) = tx.first() {
        if zones[first].dst {
            for z in (0..first).rev() {
                if !zones[z].dst {
                    return z;
                }
            }
        }
    }
    zones.iter().position(|z| !z.dst).unwrap_or(0)
}

fn tzset_name(s: &str) -> Option<(&str, &str)> {
    if let Some(rest) = s.strip_prefix('<') {
        let end = rest.find('>')?;
        return Some((&rest[..end], &rest[end + 1..]));
    }
    for (i, c) in s.char_indices() {
        if matches!(c, '0'..='9' | ',' | '-' | '+') {
            return (i >= 3).then(|| (&s[..i], &s[i..]));
        }
    }
    (s.len() >= 3).then_some((s, ""))
}

fn tzset_num(s: &str, min: i64, max: i64) -> Option<(i64, &str)> {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let mut num = 0;
    for b in s[..digits].bytes() {
        num = num * 10 + i64::from(b - b'0');
        if num > max {
            return None;
        }
    }
    (num >= min).then_some((num, &s[digits..]))
}

fn tzset_offset(s: &str) -> Option<(i64, &str)> {
    let (neg, s) = match s.as_bytes().first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    let (hours, mut s) = tzset_num(s, 0, 24 * 7)?;
    let mut off = hours * 3600;
    if let Some(rest) = s.strip_prefix(':') {
        let (mins, rest) = tzset_num(rest, 0, 59)?;
        off += mins * 60;
        s = rest;
        if let Some(rest) = s.strip_prefix(':') {
            let (secs, rest) = tzset_num(rest, 0, 59)?;
            off += secs;
            s = rest;
        }
    }
    Some((if neg { -off } else { off }, s))
}

enum Rule {
    Julian(i64),
    Doy(i64),
    MonthWeekDay { mon: i64, week: i64, day: i64 },
}

fn tzset_rule(s: &str) -> Option<(Rule, i64, &str)> {
    let (rule, s) = if let Some(rest) = s.strip_prefix('J') {
        let (day, rest) = tzset_num(rest, 1, 365)?;
        (Rule::Julian(day), rest)
    } else if let Some(rest) = s.strip_prefix('M') {
        let (mon, rest) = tzset_num(rest, 1, 12)?;
        let (week, rest) = tzset_num(rest.strip_prefix('.')?, 1, 5)?;
        let (day, rest) = tzset_num(rest.strip_prefix('.')?, 0, 6)?;
        (Rule::MonthWeekDay { mon, week, day }, rest)
    } else {
        let (day, rest) = tzset_num(s, 0, 365)?;
        (Rule::Doy(day), rest)
    };
    match s.strip_prefix('/') {
        None => Some((rule, 2 * 3600, s)),
        Some(rest) => {
            let (time, rest) = tzset_offset(rest)?;
            Some((rule, time, rest))
        }
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in(mon: i64, year: i64) -> i64 {
    match mon {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// tzruleTime: seconds from the start of the year to the rule's moment, in UTC.
fn rule_time(year: i64, rule: &Rule, time: i64, off: i64) -> i64 {
    let s = match *rule {
        Rule::Julian(day) => {
            let mut s = (day - 1) * SECONDS_PER_DAY;
            if is_leap(year) && day >= 60 {
                s += SECONDS_PER_DAY;
            }
            s
        }
        Rule::Doy(day) => day * SECONDS_PER_DAY,
        Rule::MonthWeekDay { mon, week, day } => {
            let m1 = (mon + 9) % 12 + 1;
            let yy0 = if mon <= 2 { year - 1 } else { year };
            let (yy1, yy2) = (yy0 / 100, yy0 % 100);
            let mut dow = ((26 * m1 - 2) / 10 + 1 + yy2 + yy2 / 4 + yy1 / 4 - 2 * yy1) % 7;
            if dow < 0 {
                dow += 7;
            }
            let mut d = day - dow;
            if d < 0 {
                d += 7;
            }
            for _ in 1..week {
                if d + 7 >= days_in(mon, year) {
                    break;
                }
                d += 7;
            }
            d += (1..mon).map(|m| days_in(m, 1)).sum::<i64>();
            if is_leap(year) && mon > 2 {
                d += 1;
            }
            d * SECONDS_PER_DAY
        }
    };
    s + time - off
}

/// The year of a day counted from 1970-01-01, and the day's number in that year from 1.
fn year_day(days: i64) -> (i64, i64) {
    let mut year = 1970 + days.div_euclid(366);
    while days_from_epoch(year + 1) <= days {
        year += 1;
    }
    while days_from_epoch(year) > days {
        year -= 1;
    }
    (year, days - days_from_epoch(year) + 1)
}

/// The days from 1970-01-01 to the first of January of a year.
pub fn days_from_epoch(year: i64) -> i64 {
    let y = year - 1;
    365 * (year - 1970) + (y / 4 - y / 100 + y / 400) - (1969 / 4 - 1969 / 100 + 1969 / 400)
}

/// tzset: the offset a POSIX TZ rule gives at a moment.
fn tzset(s: &str, sec: i64) -> Option<i64> {
    let (_, s) = tzset_name(s)?;
    let (std_offset, s) = tzset_offset(s)?;
    let std_offset = -std_offset;
    if s.is_empty() || s.starts_with(',') {
        return Some(std_offset);
    }
    let (_, s) = tzset_name(s)?;
    let (dst_offset, s) = if s.is_empty() || s.starts_with(',') {
        (std_offset + 3600, s)
    } else {
        let (off, s) = tzset_offset(s)?;
        (-off, s)
    };
    let s = if s.is_empty() { ",M3.2.0,M11.1.0" } else { s };
    let s = s.strip_prefix([',', ';'])?;
    let (start, start_time, s) = tzset_rule(s)?;
    let s = s.strip_prefix(',')?;
    let (end, end_time, s) = tzset_rule(s)?;
    if !s.is_empty() {
        return None;
    }
    let (year, yday) = year_day(sec.div_euclid(SECONDS_PER_DAY));
    // Go's %, which truncates: a moment before 1970 counts a day early.
    let ysec = (yday - 1) * SECONDS_PER_DAY + sec % SECONDS_PER_DAY;
    let mut start_sec = rule_time(year, &start, start_time, std_offset);
    let mut end_sec = rule_time(year, &end, end_time, dst_offset);
    let (mut std_off, mut dst_off) = (std_offset, dst_offset);
    if end_sec < start_sec {
        std::mem::swap(&mut start_sec, &mut end_sec);
        std::mem::swap(&mut std_off, &mut dst_off);
    }
    Some(if ysec < start_sec || ysec >= end_sec {
        std_off
    } else {
        dst_off
    })
}

/// A Unix time in RFC 3339 at an offset, as Go's time.RFC3339 layout writes it.
pub fn rfc3339(unix: i64, offset: i64) -> String {
    let local = unix + offset;
    let days = local.div_euclid(SECONDS_PER_DAY);
    let secs = local.rem_euclid(SECONDS_PER_DAY);
    let (year, yday) = year_day(days);
    let mut month = 1;
    let mut day = yday;
    while day > days_in(month, year) {
        day -= days_in(month, year);
        month += 1;
    }
    // Go writes the zone in minutes, its sign that of the minutes.
    let minutes = offset / 60;
    let zone = if offset == 0 {
        "Z".to_string()
    } else {
        let sign = if minutes < 0 { '-' } else { '+' };
        let m = minutes.abs();
        format!("{sign}{:02}:{:02}", m / 60, m % 60)
    };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{zone}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_give_go_s_offsets() {
        // New York: 2026-07-01 is in daylight time, 2026-01-15 is not.
        let rule = "EST5EDT,M3.2.0,M11.1.0";
        assert_eq!(tzset(rule, 1_782_864_000), Some(-4 * 3600));
        assert_eq!(tzset(rule, 1_768_435_200), Some(-5 * 3600));
        // The southern hemisphere, where the rule's end comes first in the year.
        let sydney = "AEST-10AEDT,M10.1.0,M4.1.0/3";
        assert_eq!(tzset(sydney, 1_768_435_200), Some(11 * 3600));
        assert_eq!(tzset(sydney, 1_782_864_000), Some(10 * 3600));
        assert_eq!(tzset("<-03>3", 0), Some(-3 * 3600));
        assert_eq!(tzset("UTC0", 0), Some(0));
    }

    #[test]
    fn times_print_as_rfc3339() {
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(1_782_864_000, -4 * 3600),
            "2026-06-30T20:00:00-04:00"
        );
        assert_eq!(rfc3339(951_782_400, 0), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_768_435_200, 19_800), "2026-01-15T05:30:00+05:30");
        assert_eq!(rfc3339(30, -30), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn zoneinfo_files_are_read() {
        // A version 2 file with no transitions: everything from the rule at its end.
        let header = |typ: u32, chars: u32| {
            let mut h = b"TZif2".to_vec();
            h.resize(20, 0);
            for n in [0u32, 0, 0, 0, typ, chars] {
                h.extend_from_slice(&n.to_be_bytes());
            }
            h
        };
        let mut file = header(0, 0);
        file.extend(header(1, 4));
        file.extend_from_slice(&(-18_000i32).to_be_bytes());
        file.extend_from_slice(&[0, 0]);
        file.extend_from_slice(b"EST\0");
        file.extend_from_slice(b"\nEST5EDT,M3.2.0,M11.1.0\n");
        let zone = Location::parse(&file).unwrap();
        assert_eq!(zone.offset(1_782_864_000), -4 * 3600);
        assert_eq!(zone.offset(1_768_435_200), -5 * 3600);
        // What Go finds bad is no zone: a version it does not know, data cut short.
        let mut v4 = file.clone();
        v4[4] = b'4';
        assert!(Location::parse(&v4).is_none());
        assert!(Location::parse(&file[..30]).is_none());
        assert!(Location::parse(b"TZif").is_none());
    }
}
