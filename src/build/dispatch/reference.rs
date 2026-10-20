//! Whether an image reference reads, as distribution/reference v0.6.0's
//! ParseNormalizedNamed reads it, with its messages; and go-digest's Parse.

/// ParseNormalizedNamed, for its verdict alone.
pub(super) fn check_normalized_named(text: &str) -> Result<(), String> {
    if text.len() == 64 && text.bytes().all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9')) {
        return Err(format!(
            "invalid repository name ({text}), cannot specify 64-byte hexadecimal strings"
        ));
    }
    let (domain, remainder) = split_domain(text);
    let remote = remainder.split(':').next().unwrap_or_default();
    if remote.to_lowercase() != remote {
        return Err(format!(
            "invalid reference format: repository name ({remote}) must be lowercase"
        ));
    }
    parse(&format!("{domain}/{remainder}"))
}

/// The registry of a name and the rest (normalize.go): the default registry
/// when the first component names none, under `library/` there without a namespace.
fn split_domain(name: &str) -> (String, String) {
    let Some((first, rest)) = name.split_once('/') else {
        return ("docker.io".to_string(), format!("library/{name}"));
    };
    let (domain, mut remote) = if first == "localhost" {
        (first, rest.to_string())
    } else if first == "index.docker.io" {
        ("docker.io", rest.to_string())
    } else if first.contains(['.', ':']) || first.to_lowercase() != first {
        (first, rest.to_string())
    } else {
        ("docker.io", name.to_string())
    };
    if domain == "docker.io" && !remote.contains('/') {
        remote = format!("library/{remote}");
    }
    (domain.to_string(), remote)
}

/// Parse: the reference grammar, then the length of the path and the digest itself.
fn parse(text: &str) -> Result<(), String> {
    let Some((path, digest)) = matched(text) else {
        if text.is_empty() {
            return Err("repository name must have at least one component".to_string());
        }
        if matched(&text.to_lowercase()).is_some() {
            return Err("repository name must be lowercase".to_string());
        }
        return Err("invalid reference format".to_string());
    };
    if path.len() > 255 {
        return Err("repository name must not be more than 255 characters".to_string());
    }
    if let Some(digest) = digest {
        parse_digest(digest)?;
    }
    Ok(())
}

/// ReferenceRegexp `name[:tag][@digest]`: the path of the name and the digest.
fn matched(text: &str) -> Option<(&str, Option<&str>)> {
    let (rest, digest) = match text.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest)),
        None => (text, None),
    };
    if digest.is_some_and(|d| !is_digest_pattern(d)) {
        return None;
    }
    let (name, tag) = match rest.rfind(':') {
        Some(colon) if !rest[colon..].contains('/') => (&rest[..colon], Some(&rest[colon + 1..])),
        _ => (rest, None),
    };
    if tag.is_some_and(|t| !is_tag(t)) {
        return None;
    }
    let path = match name.split_once('/') {
        Some((domain, path)) if is_domain(domain) && is_remote_name(path) => path,
        _ if is_remote_name(name) => name,
        _ => return None,
    };
    Some((path, digest))
}

/// `[\w][\w.-]{0,127}`
fn is_tag(text: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let b = text.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && word(b[0])
        && b[1..].iter().all(|&c| word(c) || c == b'.' || c == b'-')
}

/// `[A-Za-z][A-Za-z0-9]*(?:[-_+.][A-Za-z][A-Za-z0-9]*)*[:][[:xdigit:]]{32,}`
fn is_digest_pattern(text: &str) -> bool {
    let Some((algorithm, hex)) = text.split_once(':') else {
        return false;
    };
    let component = |part: &str| {
        part.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic())
            && part.bytes().all(|b| b.is_ascii_alphanumeric())
    };
    hex.len() >= 32
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
        && algorithm.split(['-', '_', '+', '.']).all(component)
}

/// A registry host (dot-separated labels or a bracketed IPv6 address) and an optional
/// port.
fn is_domain(text: &str) -> bool {
    let (host, port) = if text.starts_with('[') {
        match text.find(']') {
            Some(end) => (&text[..=end], &text[end + 1..]),
            None => return false,
        }
    } else {
        match text.find(':') {
            Some(i) => (&text[..i], &text[i..]),
            None => (text, ""),
        }
    };
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let label = |part: &str| {
        let b = part.as_bytes();
        !b.is_empty()
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-')
    };
    let host_ok = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(address) => {
            !address.is_empty() && address.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
        }
        None => host.split('.').all(label),
    };
    port_ok && host_ok
}

/// Path components of lowercase letters and digits joined inside by `.`, `_`, `__` or
/// runs of `-`, separated by `/`.
fn is_remote_name(text: &str) -> bool {
    text.split('/').all(|component| {
        let b = component.as_bytes();
        let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
        if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
            return false;
        }
        let mut i = 0;
        while i < b.len() {
            if alnum(b[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && !alnum(b[i]) {
                i += 1;
            }
            let sep = &component[start..i];
            if !(matches!(sep, "." | "_" | "__") || sep.bytes().all(|c| c == b'-')) {
                return false;
            }
        }
        true
    })
}

/// go-digest's Parse, for its verdict.
pub(super) fn parse_digest(digest: &str) -> Result<(), String> {
    const FORMAT: &str = "invalid checksum digest format";
    let (algorithm, encoded) = match digest.split_once(':') {
        Some((a, e)) if !a.is_empty() && !e.is_empty() => (a, e),
        _ => return Err(FORMAT.to_string()),
    };
    let size = match algorithm {
        "sha256" => 32,
        "sha384" => 48,
        "sha512" => 64,
        _ => {
            let component = |part: &str| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            };
            let anchored = algorithm.split(['.', '+', '_', '-']).all(component)
                && encoded
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b'_' | b'-'));
            return Err(if anchored {
                "unsupported digest algorithm"
            } else {
                FORMAT
            }
            .to_string());
        }
    };
    if encoded.len() != size * 2 {
        return Err("invalid checksum digest length".to_string());
    }
    if !encoded
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(FORMAT.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references() {
        let hex = "0123456789abcdef".repeat(4);
        for ok in [
            "busybox",
            "busybox:1.37",
            "docker.io/library/busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e",
            "localhost:5000/app:1",
            "[::1]:5000/app",
            "ghcr.io/project-zot/zot:v2.1.21",
            "a__b/c--d.e_f",
        ] {
            assert_eq!(check_normalized_named(ok), Ok(()), "{ok}");
        }
        assert_eq!(
            check_normalized_named("Busybox").unwrap_err(),
            "invalid reference format: repository name (library/Busybox) must be lowercase"
        );
        assert_eq!(
            check_normalized_named(&hex).unwrap_err(),
            format!("invalid repository name ({hex}), cannot specify 64-byte hexadecimal strings")
        );
        assert_eq!(
            check_normalized_named("busybox:").unwrap_err(),
            "invalid reference format"
        );
        assert_eq!(
            check_normalized_named("a//b").unwrap_err(),
            "invalid reference format"
        );
        assert_eq!(
            check_normalized_named(&format!("busybox@sha256:{}", &hex[..40])).unwrap_err(),
            "invalid checksum digest length"
        );
    }

    #[test]
    fn digests() {
        let hex = "0123456789abcdef".repeat(4);
        assert_eq!(parse_digest(&format!("sha256:{hex}")), Ok(()));
        assert_eq!(
            parse_digest("sha256").unwrap_err(),
            "invalid checksum digest format"
        );
        assert_eq!(
            parse_digest("md5:abc").unwrap_err(),
            "unsupported digest algorithm"
        );
        assert_eq!(
            parse_digest("sha256:abc").unwrap_err(),
            "invalid checksum digest length"
        );
        assert_eq!(
            parse_digest(&format!("sha256:{}", hex.to_uppercase())).unwrap_err(),
            "invalid checksum digest format"
        );
    }
}
