//! The port specifications of EXPOSE (convert_expose.go): `[ip:]public:private/proto`
//! read into `port/proto`, a range into each of its ports.

use std::net::IpAddr;

use super::gofmt::quote;

/// parsePorts.
pub(super) fn parse_ports(ports: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for p in ports {
        out.extend(parse_port(p)?);
    }
    Ok(out)
}

/// parsePort.
pub(super) fn parse_port(raw: &str) -> Result<Vec<String>, String> {
    let (ip, host_port, container_port) = split_parts(raw);
    let (proto, container_port) = split_proto_port(&container_port)
        .map_err(|e| format!("invalid port: {}: {e}", quote(raw)))?;

    let mut ip = ip;
    if ip.starts_with('[') {
        ip = split_host(&format!("{ip}:")).map_err(|e| format!("invalid IP address {ip}: {e}"))?;
    }
    if !ip.is_empty() && ip.parse::<IpAddr>().is_err() {
        return Err(format!("invalid IP address: {ip}"));
    }

    let (start, end) = parse_port_range(&container_port)
        .map_err(|_| format!("invalid containerPort: {container_port}"))?;
    if !host_port.is_empty() {
        let (host_start, host_end) =
            parse_port_range(&host_port).map_err(|_| format!("invalid hostPort: {host_port}"))?;
        if end - start != host_end - host_start && end != start {
            return Err(format!(
                "invalid ranges specified for container and host Ports: {container_port} and {host_port}"
            ));
        }
    }
    let proto = proto.to_lowercase();
    Ok((start..=end).map(|p| format!("{p}/{proto}")).collect())
}

fn split_parts(raw: &str) -> (String, String, String) {
    let parts: Vec<&str> = raw.split(':').collect();
    match parts.len() {
        1 => (String::new(), String::new(), parts[0].to_string()),
        2 => (String::new(), parts[0].to_string(), parts[1].to_string()),
        3 => (
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        ),
        n => (
            parts[..n - 2].join(":"),
            parts[n - 2].to_string(),
            parts[n - 1].to_string(),
        ),
    }
}

fn split_proto_port(raw: &str) -> Result<(String, String), String> {
    let (port, proto) = raw.split_once('/').unwrap_or((raw, ""));
    if port.is_empty() {
        return Err("no port specified".to_string());
    }
    match proto.to_lowercase().as_str() {
        "" => Ok(("tcp".to_string(), port.to_string())),
        "tcp" | "udp" | "sctp" => Ok((proto.to_string(), port.to_string())),
        _ => Err(format!("invalid proto: {proto}")),
    }
}

fn parse_port_range(ports: &str) -> Result<(i64, i64), ()> {
    if ports.is_empty() {
        return Err(());
    }
    let (start, end) = match ports.split_once('-') {
        Some((s, e)) => (s, Some(e)),
        None => (ports, None),
    };
    let start_port = parse_port_number(start)?;
    let Some(end) = end.filter(|e| *e != start) else {
        return Ok((start_port, start_port));
    };
    let end_port = parse_port_number(end)?;
    if end_port < start_port {
        return Err(());
    }
    Ok((start_port, end_port))
}

/// strconv.ParseInt(raw, 10, 0) in 0 to 65535; why it fails is never shown.
fn parse_port_number(raw: &str) -> Result<i64, ()> {
    let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    let port: i64 = raw.parse().map_err(|_| ())?;
    if !(0..=65535).contains(&port) {
        return Err(());
    }
    Ok(port)
}

/// net.SplitHostPort's host, for an address in brackets.
fn split_host(hostport: &str) -> Result<String, String> {
    let fail = |why: &str| format!("address {hostport}: {why}");
    let Some(i) = hostport.rfind(':') else {
        return Err(fail("missing port in address"));
    };
    let Some(end) = hostport.find(']') else {
        return Err(fail("missing ']' in address"));
    };
    if end + 1 == hostport.len() {
        return Err(fail("missing port in address"));
    }
    if end + 1 != i {
        if hostport.as_bytes()[end + 1] == b':' {
            return Err(fail("too many colons in address"));
        }
        return Err(fail("missing port in address"));
    }
    let host = &hostport[1..end];
    if hostport[1..].contains('[') {
        return Err(fail("unexpected '[' in address"));
    }
    if hostport[end + 1..].contains(']') {
        return Err(fail("unexpected ']' in address"));
    }
    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(v: &[&str]) -> Result<Vec<String>, String> {
        parse_ports(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn empty_container_port() {
        assert_eq!(
            parse_port("").unwrap_err(),
            "invalid port: \"\": no port specified"
        );
        assert_eq!(
            parse_port("0.0.0.0:1234-1235:/tcp").unwrap_err(),
            "invalid port: \"0.0.0.0:1234-1235:/tcp\": no port specified"
        );
        assert_eq!(
            parse_port("0.0.0.0:1234-1235:").unwrap_err(),
            "invalid port: \"0.0.0.0:1234-1235:\": no port specified"
        );
    }

    #[test]
    fn full() {
        assert_eq!(
            parse_port("0.0.0.0:1234-1235:3333-3334/tcp").unwrap(),
            ["3333/tcp", "3334/tcp"]
        );
    }

    #[test]
    fn ipv6() {
        for (spec, want) in [
            ("[2001:4860:0:2001::68]::333", "333/tcp"),
            ("[::1]:80:80", "80/tcp"),
            ("2001:4860:0:2001::68::333", "333/tcp"),
            ("::1:80:80", "80/tcp"),
            ("::::80", "80/tcp"),
        ] {
            assert_eq!(parse_port(spec).unwrap(), [want], "{spec}");
        }
    }

    #[test]
    fn lists() {
        let want = ["1234/tcp", "2345/udp", "3456/sctp"];
        assert_eq!(ports(&["1234/tcp", "2345/udp", "3456/sctp"]).unwrap(), want);
        assert_eq!(
            ports(&["1234:1234/tcp", "2345:2345/udp", "3456:3456/sctp"]).unwrap(),
            want
        );
        assert_eq!(
            ports(&[
                "0.0.0.0:1234:1234/tcp",
                "0.0.0.0:2345:2345/udp",
                "0.0.0.0:3456:3456/sctp"
            ])
            .unwrap(),
            want
        );
        assert!(ports(&["localhost:1234:1234/tcp"]).is_err());
    }

    #[test]
    fn ranges() {
        let want = [
            "1234/tcp",
            "1235/tcp",
            "1236/tcp",
            "2345/udp",
            "2346/udp",
            "2347/udp",
            "3456/sctp",
            "3457/sctp",
            "3458/sctp",
        ];
        assert_eq!(
            ports(&["1234-1236/tcp", "2345-2347/udp", "3456-3458/sctp"]).unwrap(),
            want
        );
        assert_eq!(
            ports(&[
                "1234-1236:1234-1236/tcp",
                "2345-2347:2345-2347/udp",
                "3456-3458:3456-3458/sctp"
            ])
            .unwrap(),
            want
        );
        assert!(ports(&["localhost:1234-1236:1234-1236/tcp"]).is_err());
        assert_eq!(
            parse_port("9000-9002").unwrap(),
            ["9000/tcp", "9001/tcp", "9002/tcp"]
        );
        assert_eq!(parse_port("80/UDP").unwrap(), ["80/udp"]);
        assert_eq!(
            parse_port("80/icmp").unwrap_err(),
            "invalid port: \"80/icmp\": invalid proto: icmp"
        );
        assert_eq!(
            parse_port("70000").unwrap_err(),
            "invalid containerPort: 70000"
        );
        assert_eq!(
            parse_port("90-80").unwrap_err(),
            "invalid containerPort: 90-80"
        );
        assert_eq!(
            parse_port("1-3:5-6").unwrap_err(),
            "invalid ranges specified for container and host Ports: 5-6 and 1-3"
        );
    }
}
