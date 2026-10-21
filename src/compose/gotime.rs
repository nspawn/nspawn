//! Go's local time, for the time logrus writes on the warnings: the zone
//! of TZ, or of /etc/localtime, read as Go reads a zoneinfo file, with the POSIX rule at
//! its end for the years past its last transition.

const SECONDS_PER_DAY: i64 = 86_400;

/// The offset from UTC, in seconds, of the local zone at a Unix time, as Go's time.Local
/// has it; UTC when Go would fall back to it.
pub fn local_offset(unix: i64) -> i64 {
    local_zone().map_or(0, |zone| zone.offset(unix))
}

/// The local zone at a Unix time: its offset and the name Go prints for it (CET, UTC),
/// empty where the zone has none.
pub fn local_at(unix: i64) -> (i64, String) {
    local_zone().map_or_else(|| (0, "UTC".to_string()), |zone| zone.at(unix))
}

/// Go's time.Time.String of a Unix time in the local zone, as ps prints a machine's
/// CreatedAt: `2006-01-02 15:04:05 -0700 MST`, the offset again where the
/// zone has no name.
pub fn time_string(unix: i64) -> String {
    let (offset, name) = local_at(unix);
    let rfc = rfc3339(unix, offset);
    let (date, time) = rfc.split_at(10);
    let minutes = offset / 60;
    let numeric = format!(
        "{}{:02}{:02}",
        if minutes < 0 { '-' } else { '+' },
        minutes.abs() / 60,
        minutes.abs() % 60
    );
    let name = if name.is_empty() {
        numeric.clone()
    } else {
        name
    };
    format!("{date} {} {numeric} {name}", &time[1..9])
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
    name: String,
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
            let at = usize::from(zone_data.read(1)?[0]);
            if at >= abbrev.len() {
                return None;
            }
            let name = &abbrev[at..];
            let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            let name = String::from_utf8_lossy(&name[..end]).into_owned();
            zones.push(Zone { offset, dst, name });
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
        self.at(unix).0
    }

    /// Location.lookup: the offset and the zone's name at a moment.
    fn at(&self, unix: i64) -> (i64, String) {
        let zone = |i: usize| (self.zones[i].offset, self.zones[i].name.clone());
        if unix < self.tx[0].0 {
            return zone(first_zone(&self.zones, &self.tx));
        }
        let lo = self.tx.partition_point(|&(when, _)| when <= unix) - 1;
        if lo == self.tx.len() - 1 && !self.extend.is_empty() {
            if let Some(found) = tzset(&self.extend, unix) {
                return found;
            }
        }
        zone(self.tx[lo].1)
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
    // Floor division: the leap days before year 0 count down too.
    365 * (year - 1970) + (y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400))
        - (1969 / 4 - 1969 / 100 + 1969 / 400)
}

/// tzset: the offset a POSIX TZ rule gives at a moment.
fn tzset(s: &str, sec: i64) -> Option<(i64, String)> {
    let (std_name, s) = tzset_name(s)?;
    let (std_offset, s) = tzset_offset(s)?;
    let std_offset = -std_offset;
    if s.is_empty() || s.starts_with(',') {
        return Some((std_offset, std_name.to_string()));
    }
    let (dst_name, s) = tzset_name(s)?;
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
    let (mut std_zone, mut dst_zone) = ((std_offset, std_name), (dst_offset, dst_name));
    if end_sec < start_sec {
        std::mem::swap(&mut start_sec, &mut end_sec);
        std::mem::swap(&mut std_zone, &mut dst_zone);
    }
    let (offset, name) = if ysec < start_sec || ysec >= end_sec {
        std_zone
    } else {
        dst_zone
    };
    Some((offset, name.to_string()))
}

/// timetypes.GetTimestamp: a Go duration before `now`, an RFC 3339 time or
/// date in one of the forms it tries (in the zone of `offset` when it names none), or
/// Unix seconds; as `seconds[.nanoseconds]`.
pub fn get_timestamp(value: &str, now: f64, offset: i64) -> Result<String, String> {
    if value != "0" {
        if let Ok(nanos) = super::golib::parse_duration(value) {
            return Ok(format!("{}", (now - nanos as f64 / 1e9).floor() as i64));
        }
    }
    let local = !value.contains(['z', 'Z', '+']) && value.matches('-').count() != 3;
    // The time of day: none, the hour, hour and minute, or the whole with a fraction.
    let parts = if value.contains('.') {
        3
    } else if value.contains('T') {
        let mut colons = value.matches(':').count();
        if !local && !value.contains(['z', 'Z']) && colons > 0 {
            colons -= 1;
        }
        colons.min(2) + 1
    } else {
        0
    };
    match parse_layout(value, parts, value.contains('.'), local, offset) {
        Some((unix, nanos)) => Ok(format!("{unix}.{nanos:09}")),
        None if value.contains('-') => Err(format!("parsing time {value:?}: cannot parse")),
        None => {
            // Unix seconds, and a fraction of them, passed on as they are.
            let (seconds, fraction) = match value.split_once('.') {
                Some((s, f)) => (s, Some(f)),
                None => (value, None),
            };
            let int = |s: &str| super::gostr::atoi(s).is_ok();
            if int(seconds) && fraction.is_none_or(int) {
                Ok(value.to_string())
            } else {
                Err(format!(
                    "failed to parse value as time or duration: {}",
                    super::golib::go_quote(value)
                ))
            }
        }
    }
}

/// `value` read with the layout GetTimestamp picks: a date, then `parts` of the time
/// after a T (and a fraction), then a zone unless it is `local`.
fn parse_layout(
    value: &str,
    parts: usize,
    fraction: bool,
    local: bool,
    offset: i64,
) -> Option<(i64, u32)> {
    let b = value.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| std::str::from_utf8(part).ok()?.parse().ok())
            .flatten()
    };
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in(month, year) {
        return None;
    }
    let mut at = 10;
    let (mut hour, mut min, mut sec, mut nanos) = (0, 0, 0, 0u32);
    if parts > 0 {
        if b.get(at) != Some(&b'T') {
            return None;
        }
        hour = num(at + 1, 2)?;
        at += 3;
        if parts > 1 {
            if b.get(at) != Some(&b':') {
                return None;
            }
            min = num(at + 1, 2)?;
            at += 3;
        }
        if parts > 2 {
            if b.get(at) != Some(&b':') {
                return None;
            }
            sec = num(at + 1, 2)?;
            at += 3;
        }
        if fraction {
            if b.get(at) != Some(&b'.') {
                return None;
            }
            let digits = b[at + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if digits == 0 || digits > 9 {
                return None;
            }
            let text = std::str::from_utf8(&b[at + 1..at + 1 + digits]).ok()?;
            nanos = text.parse::<u32>().ok()? * 10u32.pow(9 - digits as u32);
            at += 1 + digits;
        }
        if hour > 23 || min > 59 || sec > 59 {
            return None;
        }
    }
    let zone = if local {
        if at != b.len() {
            return None;
        }
        offset
    } else {
        match b.get(at)? {
            b'Z' if at + 1 == b.len() => 0,
            sign @ (b'+' | b'-') if at + 6 == b.len() && b.get(at + 3) == Some(&b':') => {
                let off = num(at + 1, 2)? * 3600 + num(at + 4, 2)? * 60;
                if *sign == b'-' {
                    -off
                } else {
                    off
                }
            }
            _ => return None,
        }
    };
    let mut days = days_from_epoch(year);
    for m in 1..month {
        days += days_in(m, year);
    }
    days += day - 1;
    Some((
        days * SECONDS_PER_DAY + hour * 3600 + min * 60 + sec - zone,
        nanos,
    ))
}

/// The Unix time of an RFC 3339 timestamp, as Go's time.Parse with RFC3339Nano reads it
/// (its fraction left out): an image config's `created`.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| std::str::from_utf8(part).ok()?.parse().ok())
            .flatten()
    };
    let sep = |at: usize, c: u8| b.get(at) == Some(&c);
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, min, sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(sep(4, b'-') && sep(7, b'-') && matches!(b.get(10), Some(b'T' | b't')))
        || !(sep(13, b':') && sep(16, b':'))
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in(month, year)
        || hour > 23
        || min > 59
        || sec > 59
    {
        return None;
    }
    let mut at = 19;
    if sep(at, b'.') {
        at += 1;
        let digits = b[at..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        at += digits;
    }
    let offset = match b.get(at)? {
        b'Z' | b'z' if at + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if at + 6 == b.len() && sep(at + 3, b':') => {
            let (h, m) = (num(at + 1, 2)?, num(at + 4, 2)?);
            if h > 23 || m > 59 {
                return None;
            }
            let off = h * 3600 + m * 60;
            if *sign == b'-' {
                -off
            } else {
                off
            }
        }
        _ => return None,
    };
    let mut days = days_from_epoch(year);
    for m in 1..month {
        days += days_in(m, year);
    }
    days += day - 1;
    Some(days * SECONDS_PER_DAY + hour * 3600 + min * 60 + sec - offset)
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
        let zone = |offset: i64, name: &str| Some((offset, name.to_string()));
        let rule = "EST5EDT,M3.2.0,M11.1.0";
        assert_eq!(tzset(rule, 1_782_864_000), zone(-4 * 3600, "EDT"));
        assert_eq!(tzset(rule, 1_768_435_200), zone(-5 * 3600, "EST"));
        // The southern hemisphere, where the rule's end comes first in the year.
        let sydney = "AEST-10AEDT,M10.1.0,M4.1.0/3";
        assert_eq!(tzset(sydney, 1_768_435_200), zone(11 * 3600, "AEDT"));
        assert_eq!(tzset(sydney, 1_782_864_000), zone(10 * 3600, "AEST"));
        assert_eq!(tzset("<-03>3", 0), zone(-3 * 3600, "-03"));
        assert_eq!(tzset("UTC0", 0), zone(0, "UTC"));
    }

    #[test]
    fn a_time_prints_as_go_prints_it() {
        // Whatever zone the tests run in, the layout and the offset agree.
        let text = time_string(1_782_864_000);
        let (offset, _) = local_at(1_782_864_000);
        let rfc = rfc3339(1_782_864_000, offset);
        assert!(
            text.starts_with(&format!("{} {}", &rfc[..10], &rfc[11..19])),
            "{text}"
        );
        assert_eq!(text.split(' ').count(), 4, "{text}");
    }

    #[test]
    fn timestamps_are_read_as_docker_reads_them() {
        let now = 1_729_771_200.0;
        let at = |v: &str| get_timestamp(v, now, 7200);
        assert_eq!(at("42m").unwrap(), "1729768680");
        assert_eq!(at("1h30m").unwrap(), "1729765800");
        assert_eq!(at("0").unwrap(), "0");
        assert_eq!(at("1729771200").unwrap(), "1729771200");
        assert_eq!(at("1729771200.5").unwrap(), "1729771200.5");
        assert_eq!(at("2024-10-24T12:00:00Z").unwrap(), "1729771200.000000000");
        assert_eq!(
            at("2024-10-24T14:00:00+02:00").unwrap(),
            "1729771200.000000000"
        );
        // Without a zone, the local one of now.
        assert_eq!(at("2024-10-24T14:00:00").unwrap(), "1729771200.000000000");
        assert_eq!(at("2024-10-24T14:00").unwrap(), "1729771200.000000000");
        assert_eq!(at("2024-10-24T14").unwrap(), "1729771200.000000000");
        assert_eq!(
            at("2024-10-24T14:00:00.25").unwrap(),
            "1729771200.250000000"
        );
        assert_eq!(at("2024-10-24").unwrap(), "1729720800.000000000");
        assert_eq!(at("2024-10-24Z").unwrap(), "1729728000.000000000");
        assert_eq!(
            at("yesterday").unwrap_err(),
            "failed to parse value as time or duration: \"yesterday\""
        );
        assert!(at("2024-13-01").is_err());
        assert!(at("10 min ago").is_err());
    }

    #[test]
    fn year_zero_is_a_leap_year_before_year_one() {
        assert_eq!(days_from_epoch(1), -719_162);
        assert_eq!(days_from_epoch(0), -719_528);
        assert_eq!(days_from_epoch(1970), 0);
        assert_eq!(days_from_epoch(2000), 10_957);
    }

    #[test]
    fn rfc3339_reads_back() {
        for unix in [0i64, 1_729_771_200, 951_782_400, -86_400 * 400] {
            assert_eq!(parse_rfc3339(&rfc3339(unix, 0)), Some(unix), "{unix}");
            assert_eq!(parse_rfc3339(&rfc3339(unix, 7200)), Some(unix), "{unix}");
        }
        assert_eq!(
            parse_rfc3339("2024-10-24T12:00:00.123456789Z"),
            Some(1_729_771_200)
        );
        assert_eq!(
            parse_rfc3339("2024-10-24T14:00:00+02:00"),
            Some(1_729_771_200)
        );
        for bad in [
            "",
            "2024-10-24",
            "2024-13-01T00:00:00Z",
            "2024-02-30T00:00:00Z",
            "2024-10-24T12:00:00",
            "2024-10-24T12:00:00.Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
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
        assert_eq!(zone.at(1_768_435_200).1, "EST");
        assert_eq!(zone.zones[0].name, "EST");
        // What Go finds bad is no zone: a version it does not know, data cut short.
        let mut v4 = file.clone();
        v4[4] = b'4';
        assert!(Location::parse(&v4).is_none());
        assert!(Location::parse(&file[..30]).is_none());
        assert!(Location::parse(b"TZif").is_none());
    }
}
