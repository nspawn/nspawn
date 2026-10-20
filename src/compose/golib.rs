//! What the compose CLI takes from Go libraries, done the same way: splitting a command
//! line (go-shellwords), sizes (go-units), durations (go-str2duration and Go's
//! time.Duration), port specs (go-connections) and Go's quoting.

use std::net::IpAddr;

use super::gostr;
use super::value::Value;

/// go-shellwords' Parse with its defaults: quotes and backslashes as a shell reads
/// them, no variables and no command substitution; the line ends at an unquoted `;`,
/// `&`, `|`, `<` or `>`, which is how compose reads a string command.
pub fn shell_words(line: &str) -> Result<Vec<String>, String> {
    let invalid = || "invalid command line string".to_string();
    let mut args = Vec::new();
    let mut buf = String::new();
    #[derive(PartialEq)]
    enum Got {
        No,
        Single,
        Quoted,
    }
    let mut got = Got::No;
    let (mut escaped, mut double, mut single, mut back, mut dollar) =
        (false, false, false, false, false);
    let flush = |buf: &mut String, got: &mut Got, args: &mut Vec<String>| {
        if *got == Got::Quoted || (*got != Got::No && !buf.is_empty()) {
            args.push(std::mem::take(buf));
        }
        buf.clear();
        *got = Got::No;
    };
    for r in line.chars() {
        if escaped {
            escaped = false;
            if back || dollar {
                buf.push('\\');
                buf.push(r);
                got = Got::Single;
                continue;
            }
            buf.push(match r {
                't' => '\t',
                'n' => '\n',
                other => other,
            });
            got = Got::Single;
            continue;
        }
        if r == '\\' {
            if single {
                buf.push('\\');
            } else {
                escaped = true;
            }
            continue;
        }
        if matches!(r, ' ' | '\t' | '\r' | '\n') {
            if single || double || back || dollar {
                buf.push(r);
            } else {
                flush(&mut buf, &mut got, &mut args);
            }
            continue;
        }
        match r {
            '`' if !single && !double && !dollar => back = !back,
            ')' if !single && !double && !back => {
                if !dollar {
                    return Err(invalid());
                }
                buf.push(')');
                dollar = false;
                got = Got::Single;
                continue;
            }
            '(' if !single && !double && !back => {
                let after_dollar = buf.ends_with('$') && !buf.ends_with("\\$");
                if !dollar && after_dollar {
                    dollar = true;
                    buf.push('(');
                    continue;
                }
                return Err(invalid());
            }
            '"' if !single && !dollar && !back => {
                if double {
                    got = Got::Quoted;
                }
                double = !double;
                continue;
            }
            '\'' if !double && !dollar && !back => {
                if single {
                    got = Got::Quoted;
                }
                single = !single;
                continue;
            }
            ';' | '&' | '|' | '<' | '>' if !(single || double || back || dollar) => {
                // `2>` drops the digits before it.
                if r == '>' && !buf.is_empty() && buf.bytes().all(|b| b.is_ascii_digit()) {
                    got = Got::No;
                }
                break;
            }
            _ => {}
        }
        got = Got::Single;
        buf.push(r);
    }
    flush(&mut buf, &mut got, &mut args);
    if escaped || single || double || back || dollar {
        return Err(invalid());
    }
    Ok(args)
}

/// go-units' RAMInBytes: a number with an optional binary suffix (`k`, `m`, `g`, `t`,
/// `p`, each with an optional `b` or `ib`), case-insensitive.
pub fn ram_in_bytes(size: &str) -> Result<i64, String> {
    let Some(sep) = size.rfind(|c: char| c.is_ascii_digit() || c == '.' || c == ' ') else {
        return Err(format!("invalid size: '{size}'"));
    };
    let (num, suffix) = if size.as_bytes()[sep] != b' ' {
        (&size[..=sep], &size[sep + 1..])
    } else {
        (&size[..sep], &size[sep + 1..])
    };
    let mut number = parse_float(num)?;
    if number < 0.0 {
        return Err(format!("invalid size: '{size}'"));
    }
    if suffix.is_empty() {
        return Ok(gostr::f64_to_i64(number));
    }
    let bad = |s: &str| format!("invalid suffix: '{s}'");
    // The length is the one written; the letters are compared lowered, as Go lowers
    // them, which may shorten them.
    if gostr::to_bytes(suffix).len() > 3 {
        return Err(bad(suffix));
    }
    let lower = gostr::to_lower_str(suffix);
    let bytes = lower.as_bytes();
    if bytes[0] == b'b' {
        return if bytes.len() > 1 {
            Err(bad(&lower))
        } else {
            Ok(gostr::f64_to_i64(number))
        };
    }
    let multiplier: i64 = match bytes[0] {
        b'k' => 1 << 10,
        b'm' => 1 << 20,
        b'g' => 1 << 30,
        b't' => 1 << 40,
        b'p' => 1 << 50,
        _ => return Err(bad(&lower)),
    };
    number *= multiplier as f64;
    match bytes.len() {
        2 if bytes[1] != b'b' => return Err(bad(&lower)),
        3 if &bytes[1..] != b"ib" => return Err(bad(&lower)),
        _ => {}
    }
    Ok(gostr::f64_to_i64(number))
}

/// strconv.ParseFloat(text, 64) with its error.
pub fn parse_float(text: &str) -> Result<f64, String> {
    gostr::parse_float(text, 64).map_err(|e| gostr::num_error("ParseFloat", text, e))
}

const NANOSECOND: i64 = 1;
const MICROSECOND: i64 = 1_000;
const MILLISECOND: i64 = 1_000_000;
const SECOND: i64 = 1_000_000_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;

/// go-str2duration's ParseDuration: Go's durations with days (`d`) and weeks (`w`).
/// Its arithmetic wraps as Go's does, and an overflow is caught where Go catches it.
pub fn parse_duration(original: &str) -> Result<i64, String> {
    let invalid = || format!("time: invalid duration \"{original}\"");
    let mut s = original.as_bytes();
    let mut negative = false;
    if let Some(&c) = s.first() {
        if c == b'-' || c == b'+' {
            negative = c == b'-';
            s = &s[1..];
        }
    }
    if s == b"0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    let mut total: i64 = 0;
    while !s.is_empty() {
        if !(s[0] == b'.' || s[0].is_ascii_digit()) {
            return Err(invalid());
        }
        let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
        let mut whole: i64 = 0;
        for &b in &s[..digits] {
            if whole > i64::MAX / 10 {
                return Err(invalid());
            }
            whole = whole.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
            if whole < 0 {
                return Err(invalid());
            }
        }
        s = &s[digits..];
        let mut fraction: i64 = 0;
        let mut scale: f64 = 1.0;
        let mut post = false;
        if s.first() == Some(&b'.') {
            s = &s[1..];
            let count = s.iter().take_while(|b| b.is_ascii_digit()).count();
            post = count > 0;
            let mut overflow = false;
            for &b in &s[..count] {
                if overflow {
                    continue;
                }
                if fraction > i64::MAX / 10 {
                    overflow = true;
                    continue;
                }
                let y = fraction.wrapping_mul(10).wrapping_add(i64::from(b - b'0'));
                if y < 0 {
                    overflow = true;
                    continue;
                }
                fraction = y;
                scale *= 10.0;
            }
            s = &s[count..];
        }
        if digits == 0 && !post {
            return Err(invalid());
        }
        let unit_len = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if unit_len == 0 {
            return Err(format!("time: missing unit in duration \"{original}\""));
        }
        let unit_text = gostr::from_bytes(&s[..unit_len]);
        s = &s[unit_len..];
        let unit = match unit_text.as_str() {
            "ns" => NANOSECOND,
            "us" | "\u{b5}s" | "\u{3bc}s" => MICROSECOND,
            "ms" => MILLISECOND,
            "s" => SECOND,
            "m" => MINUTE,
            "h" => HOUR,
            "d" => 24 * HOUR,
            "w" => 168 * HOUR,
            _ => {
                return Err(format!(
                    "time: unknown unit \"{unit_text}\" in duration \"{original}\""
                ))
            }
        };
        if whole > i64::MAX / unit {
            return Err(invalid());
        }
        let mut value = whole * unit;
        if fraction > 0 {
            value = value.wrapping_add(gostr::f64_to_i64(fraction as f64 * (unit as f64 / scale)));
            if value < 0 {
                return Err(invalid());
            }
        }
        total = total.wrapping_add(value);
        if total < 0 {
            return Err(invalid());
        }
    }
    Ok(if negative { -total } else { total })
}

/// Go's time.Duration.String: `1h30m0s`, `1.5s`, `500ms`, `0s`.
pub fn duration_string(d: i64) -> String {
    if d == 0 {
        return "0s".to_string();
    }
    let negative = d < 0;
    let mut u = d.unsigned_abs();
    let sign = if negative { "-" } else { "" };
    if u < SECOND as u64 {
        // Below a second: a fraction of the largest unit that fits.
        let (unit, prec) = if u < MICROSECOND as u64 {
            return format!("{sign}{u}ns");
        } else if u < MILLISECOND as u64 {
            ("\u{b5}s", 3)
        } else {
            ("ms", 6)
        };
        return format!("{sign}{}{unit}", fraction(u, prec));
    }
    let mut out = String::new();
    // Seconds with their fraction.
    let seconds = fraction(u % (60 * SECOND as u64), 9);
    u /= 60 * SECOND as u64;
    out.insert_str(0, &format!("{seconds}s"));
    if u > 0 {
        out.insert_str(0, &format!("{}m", u % 60));
        u /= 60;
        if u > 0 {
            out.insert_str(0, &format!("{u}h"));
        }
    }
    format!("{sign}{out}")
}

/// A value in units of 10^-prec printed with its fraction, trailing zeros dropped.
fn fraction(value: u64, prec: u32) -> String {
    let unit = 10u64.pow(prec);
    let whole = value / unit;
    let rest = value % unit;
    if rest == 0 {
        return whole.to_string();
    }
    let digits = format!("{rest:0width$}", width = prec as usize);
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

/// Go's %s of a value from YAML: a string as it is, a list or a map with %s of what
/// they hold, any other value as `%!s(type=value)`.
pub fn go_s(value: &Value) -> String {
    match value {
        Value::Str(s) => s.clone(),
        Value::Null => "%!s(<nil>)".to_string(),
        // time.Time has a String method.
        Value::Time(t) => t.to_string(),
        Value::Seq(items) => format!("[{}]", items.iter().map(go_s).collect::<Vec<_>>().join(" ")),
        Value::Map(map) => format!(
            "map[{}]",
            map.iter()
                .map(|(k, v)| format!("{k}:{}", go_s(v)))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        other => format!("%!s({}={other})", other.go_type()),
    }
}

/// Go's %q of a string.
pub fn go_quote(s: &str) -> String {
    gostr::quote(s)
}

/// net.SplitHostPort, for its errors: the host, or why the address is not one.
fn split_host_port(hostport: &str) -> Result<&str, String> {
    let error = |why: &str| Err(format!("address {hostport}: {why}"));
    let bytes = hostport.as_bytes();
    let Some(i) = hostport.rfind(':') else {
        return error("missing port in address");
    };
    let (host, j, k) = if bytes[0] == b'[' {
        let Some(end) = hostport.find(']') else {
            return error("missing ']' in address");
        };
        if end + 1 == bytes.len() {
            return error("missing port in address");
        } else if end + 1 != i {
            if bytes[end + 1] == b':' {
                return error("too many colons in address");
            }
            return error("missing port in address");
        }
        (&hostport[1..end], 1, end + 1)
    } else {
        let host = &hostport[..i];
        if host.contains(':') {
            return error("too many colons in address");
        }
        (host, 0, 0)
    };
    if hostport[j..].contains('[') {
        return error("unexpected '[' in address");
    }
    if hostport[k..].contains(']') {
        return error("unexpected ']' in address");
    }
    Ok(host)
}

/// Go's utf8.DecodeRune: the rune at the start of `s` and its length; a byte that
/// starts no rune is U+FFFD of length 1.
fn decode_rune(s: &[u8]) -> (char, usize) {
    let len = match s.first() {
        None => return (char::REPLACEMENT_CHARACTER, 0),
        Some(b) if *b < 0x80 => 1,
        Some(b) if *b >= 0xf0 => 4,
        Some(b) if *b >= 0xe0 => 3,
        Some(_) => 2,
    };
    match s.get(..len).and_then(|b| std::str::from_utf8(b).ok()) {
        Some(text) => (text.chars().next().expect("a rune"), len),
        None => (char::REPLACEMENT_CHARACTER, 1),
    }
}

/// Go's path/filepath.Match on Unix: `*`, `?`, `[...]` with ranges and `^`, and `\`
/// escaping; Err for a malformed pattern, as ErrBadPattern.
pub fn filepath_match(pattern: &str, name: &str) -> Result<bool, ()> {
    let pattern = gostr::to_bytes(pattern);
    let name = gostr::to_bytes(name);
    let (mut pattern, mut name) = (pattern.as_slice(), name.as_slice());
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            return Ok(!name.contains(&b'/'));
        }
        let (t, ok, err) = match_chunk(chunk, name);
        if ok && (t.is_empty() || !pattern.is_empty()) {
            name = t;
            continue;
        }
        if err {
            return Err(());
        }
        if star {
            let mut i = 0;
            while i < name.len() && name[i] != b'/' {
                let (t, ok, err) = match_chunk(chunk, &name[i + 1..]);
                if ok {
                    if pattern.is_empty() && !t.is_empty() {
                        i += 1;
                        continue;
                    }
                    name = t;
                    continue 'pattern;
                }
                if err {
                    return Err(());
                }
                i += 1;
            }
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

fn scan_chunk(mut pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    while pattern.first() == Some(&b'*') {
        pattern = &pattern[1..];
        star = true;
    }
    let mut in_range = false;
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'\\' if i + 1 < pattern.len() => i += 1,
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => return (star, &pattern[..i], &pattern[i..]),
            _ => {}
        }
        i += 1;
    }
    (star, pattern, &[])
}

/// matchChunk: what is left of `s` past the chunk, whether it matched, and whether
/// the chunk is malformed.
fn match_chunk<'a>(mut chunk: &[u8], mut s: &'a [u8]) -> (&'a [u8], bool, bool) {
    let mut failed = false;
    while let Some(&first) = chunk.first() {
        failed = failed || s.is_empty();
        match first {
            b'[' => {
                let mut r = '\0';
                if !failed {
                    let (rune, n) = decode_rune(s);
                    r = rune;
                    s = &s[n..];
                }
                chunk = &chunk[1..];
                let negated = chunk.first() == Some(&b'^');
                if negated {
                    chunk = &chunk[1..];
                }
                let mut matched = false;
                let mut ranges = 0;
                loop {
                    if chunk.first() == Some(&b']') && ranges > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let Some((lo, rest)) = get_esc(chunk) else {
                        return (&[], false, true);
                    };
                    chunk = rest;
                    let mut hi = lo;
                    if chunk[0] == b'-' {
                        let Some((h, rest)) = get_esc(&chunk[1..]) else {
                            return (&[], false, true);
                        };
                        hi = h;
                        chunk = rest;
                    }
                    matched = matched || (lo <= r && r <= hi);
                    ranges += 1;
                }
                failed = failed || matched == negated;
            }
            b'?' => {
                if !failed {
                    failed = s[0] == b'/';
                    let (_, n) = decode_rune(s);
                    s = &s[n..];
                }
                chunk = &chunk[1..];
            }
            _ => {
                if first == b'\\' {
                    chunk = &chunk[1..];
                    if chunk.is_empty() {
                        return (&[], false, true);
                    }
                }
                if !failed {
                    failed = chunk[0] != s[0];
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        (&[], false, false)
    } else {
        (s, true, false)
    }
}

/// getEsc: the rune a class names, maybe escaped; None for a malformed class.
fn get_esc(mut chunk: &[u8]) -> Option<(char, &[u8])> {
    if chunk.is_empty() || chunk[0] == b'-' || chunk[0] == b']' {
        return None;
    }
    if chunk[0] == b'\\' {
        chunk = &chunk[1..];
        if chunk.is_empty() {
            return None;
        }
    }
    let (r, n) = decode_rune(chunk);
    let rest = &chunk[n..];
    if (r == char::REPLACEMENT_CHARACTER && n == 1) || rest.is_empty() {
        return None;
    }
    Some((r, rest))
}

/// One published port of a spec, as go-connections reads `[IP:][HOST:]CONTAINER[/PROTO]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortBinding {
    pub host_ip: String,
    pub host_port: String,
    pub target: u32,
    pub protocol: String,
}

/// go-connections' ParsePortSpec, then compose-go's ordering: one binding per container
/// port, sorted by `PORT/PROTO` as text.
pub fn parse_port_spec(raw: &str) -> Result<Vec<PortBinding>, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    let (ip, host_port, container) = match parts.len() {
        1 => (String::new(), "", parts[0]),
        2 => (String::new(), parts[0], parts[1]),
        3 => (parts[0].to_string(), parts[1], parts[2]),
        n => (parts[..n - 2].join(":"), parts[n - 2], parts[n - 1]),
    };
    let (container, protocol) = container.split_once('/').unwrap_or((container, ""));
    if container.is_empty() {
        return Err(format!("no port specified: {raw}<empty>"));
    }
    let protocol = if protocol.is_empty() {
        "tcp".to_string()
    } else {
        protocol.to_lowercase()
    };
    if !matches!(protocol.as_str(), "tcp" | "udp" | "sctp") {
        return Err(format!("invalid proto: {protocol}"));
    }
    let mut ip = ip;
    if ip.starts_with('[') {
        let address = format!("{ip}:");
        match split_host_port(&address) {
            Ok(host) => ip = host.to_string(),
            Err(why) => return Err(format!("invalid IP address {ip}: {why}")),
        }
    }
    if !ip.is_empty() && ip.parse::<IpAddr>().is_err() {
        return Err(format!("invalid IP address: {ip}"));
    }
    let (start, end) =
        port_range(container).map_err(|_| format!("invalid containerPort: {container}"))?;
    let (host_start, host_end) = if host_port.is_empty() {
        (0, 0)
    } else {
        let range = port_range(host_port).map_err(|_| format!("invalid hostPort: {host_port}"))?;
        if end - start != range.1 - range.0 && end != start {
            return Err(format!(
                "invalid ranges specified for container and host Ports: {container} and {host_port}"
            ));
        }
        range
    };
    let count = end - start + 1;
    let mut bindings: Vec<(String, PortBinding)> = (0..count)
        .map(|i| {
            let mut host = String::new();
            if !host_port.is_empty() {
                host = (host_start + i).to_string();
                if count == 1 && host_start != host_end {
                    host += &format!("-{host_end}");
                }
            }
            let target = start + i;
            (
                format!("{target}/{protocol}"),
                PortBinding {
                    host_ip: ip.clone(),
                    host_port: host,
                    target,
                    protocol: protocol.clone(),
                },
            )
        })
        .collect();
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(bindings.into_iter().map(|(_, b)| b).collect())
}

fn port_range(ports: &str) -> Result<(u32, u32), ()> {
    if ports.is_empty() {
        return Err(());
    }
    let (start, end) = ports
        .split_once('-')
        .map_or((ports, None), |(s, e)| (s, Some(e)));
    let start = port_number(start)?;
    let Some(end) = end.filter(|e| *e != ports.split_once('-').unwrap().0) else {
        return Ok((start, start));
    };
    let end = port_number(end)?;
    if end < start {
        return Err(());
    }
    Ok((start, end))
}

fn port_number(raw: &str) -> Result<u32, ()> {
    let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    let port: i64 = raw.parse().map_err(|_| ())?;
    if !(0..=65535).contains(&port) {
        return Err(());
    }
    Ok(port as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_as_go_shellwords_splits_them() {
        let ok = |line: &str| shell_words(line).unwrap();
        assert_eq!(ok("echo hello"), ["echo", "hello"]);
        assert_eq!(ok("sh -c 'echo $HOME; ls'"), ["sh", "-c", "echo $HOME; ls"]);
        assert_eq!(ok("a \"b c\" d\\ e ''"), ["a", "b c", "d e", ""]);
        assert_eq!(ok("echo a && echo b"), ["echo", "a"]);
        assert_eq!(ok("echo a 2>/dev/null"), ["echo", "a"]);
        assert_eq!(ok("x $(date)"), ["x", "$(date)"]);
        assert_eq!(ok("tab\\tin"), ["tab\tin"]);
        assert!(shell_words("echo 'open").is_err());
        assert!(shell_words("f(x)").is_err());
    }

    #[test]
    fn sizes_read_as_go_units_reads_them() {
        assert_eq!(ram_in_bytes("512m"), Ok(512 << 20));
        assert_eq!(ram_in_bytes("1gb"), Ok(1 << 30));
        assert_eq!(ram_in_bytes("1GiB"), Ok(1 << 30));
        assert_eq!(ram_in_bytes("1.5k"), Ok(1536));
        assert_eq!(ram_in_bytes("100"), Ok(100));
        assert_eq!(ram_in_bytes("10 b"), Ok(10));
        assert_eq!(ram_in_bytes("x"), Err("invalid size: 'x'".to_string()));
        assert_eq!(ram_in_bytes("1kx"), Err("invalid suffix: 'kx'".to_string()));
        // Go's ParseFloat, its lowering and its conversion of a float to an integer.
        assert_eq!(ram_in_bytes("1_000m"), Ok(1000 << 20));
        assert_eq!(ram_in_bytes("0x1p4"), Ok(16));
        assert_eq!(ram_in_bytes("1e19"), Ok(i64::MIN));
        assert_eq!(ram_in_bytes("16777216t"), Ok(i64::MIN));
        assert_eq!(
            ram_in_bytes("1k\u{130}"),
            Err("invalid suffix: 'ki'".to_string())
        );
        assert_eq!(
            ram_in_bytes("1\u{130}"),
            Err("invalid suffix: 'i'".to_string())
        );
    }

    #[test]
    fn durations_read_and_print_as_go_does() {
        assert_eq!(parse_duration("1m30s"), Ok(90 * SECOND));
        assert_eq!(parse_duration("1d"), Ok(24 * HOUR));
        assert_eq!(parse_duration("1.5h"), Ok(90 * MINUTE));
        assert_eq!(parse_duration("500ms"), Ok(500 * MILLISECOND));
        assert_eq!(
            parse_duration("3"),
            Err("time: missing unit in duration \"3\"".to_string())
        );
        assert_eq!(
            parse_duration("3y"),
            Err("time: unknown unit \"y\" in duration \"3y\"".to_string())
        );
        assert_eq!(
            parse_duration("9223372036.9s"),
            Err("time: invalid duration \"9223372036.9s\"".to_string())
        );
        assert_eq!(
            parse_duration("99999999999999999999ns"),
            Err("time: invalid duration \"99999999999999999999ns\"".to_string())
        );
        assert_eq!(duration_string(90 * SECOND), "1m30s");
        assert_eq!(duration_string(24 * HOUR), "24h0m0s");
        assert_eq!(duration_string(1500 * MILLISECOND), "1.5s");
        assert_eq!(duration_string(500 * MILLISECOND), "500ms");
        assert_eq!(duration_string(1500), "1.5\u{b5}s");
        assert_eq!(duration_string(0), "0s");
    }

    #[test]
    fn patterns_match_as_filepath_match_does() {
        assert_eq!(filepath_match("w?b", "web"), Ok(true));
        assert_eq!(filepath_match("*", "web"), Ok(true));
        assert_eq!(filepath_match("\\web", "web"), Ok(true));
        assert_eq!(filepath_match("w[a-f]b", "web"), Ok(true));
        assert_eq!(filepath_match("w[^a-f]b", "web"), Ok(false));
        assert_eq!(filepath_match("w*", "w/b"), Ok(false));
        assert_eq!(filepath_match("[", "web"), Err(()));
        assert_eq!(filepath_match("web\\", "web"), Err(()));
        assert_eq!(filepath_match("a*b*c", "aXbYc"), Ok(true));
    }

    #[test]
    fn port_specs_read_as_go_connections_reads_them() {
        let one = |spec: &str| parse_port_spec(spec).unwrap();
        assert_eq!(
            one("127.0.0.1:8080:80"),
            [PortBinding {
                host_ip: "127.0.0.1".into(),
                host_port: "8080".into(),
                target: 80,
                protocol: "tcp".into()
            }]
        );
        let range = one("9-10:9-10/udp");
        assert_eq!(range.iter().map(|b| b.target).collect::<Vec<_>>(), [10, 9]);
        assert_eq!(one("8000-8010:80")[0].host_port, "8000-8010");
        assert_eq!(one("[::1]:80:80")[0].host_ip, "::1");
        assert_eq!(one("80")[0].host_port, "");
        assert_eq!(
            parse_port_spec("x:80:80").unwrap_err(),
            "invalid IP address: x"
        );
        assert_eq!(parse_port_spec("80/xyz").unwrap_err(), "invalid proto: xyz");
        assert_eq!(
            parse_port_spec("[::1:80:80").unwrap_err(),
            "invalid IP address [::1: address [::1:: missing ']' in address"
        );
        assert_eq!(
            parse_port_spec("[::1]x:80:80").unwrap_err(),
            "invalid IP address [::1]x: address [::1]x:: missing port in address"
        );
        assert_eq!(
            parse_port_spec("[::1]:1:80:80").unwrap_err(),
            "invalid IP address [::1]:1: address [::1]:1:: too many colons in address"
        );
        assert_eq!(
            parse_port_spec("[[::1]:80:80").unwrap_err(),
            "invalid IP address [[::1]: address [[::1]:: unexpected '[' in address"
        );
        assert_eq!(
            parse_port_spec("8000-8001:80-82").unwrap_err(),
            "invalid ranges specified for container and host Ports: 80-82 and 8000-8001"
        );
    }
}
