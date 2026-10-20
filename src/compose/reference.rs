//! Image references as distribution/reference's ParseDockerRef reads them, the form
//! `config --resolve-image-digests` writes: the registry and `library/` spelled out,
//! `latest` for a name without a tag, and only the digest of one that has both.

use super::gostr::to_lower_str;

/// A reference as read: tagged or digested, never neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// The registry, with its port; empty for a name that has none, which only a
    /// domain the grammar refuses leaves.
    pub domain: String,
    pub path: String,
    /// Empty when the reference is digested only.
    pub tag: String,
    /// Empty when the reference is tagged only.
    pub digest: String,
}

impl std::fmt::Display for Named {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.domain.is_empty() {
            write!(f, "{}/", self.domain)?;
        }
        f.write_str(&self.path)?;
        if !self.tag.is_empty() {
            write!(f, ":{}", self.tag)?;
        }
        if !self.digest.is_empty() {
            write!(f, "@{}", self.digest)?;
        }
        Ok(())
    }
}

/// ParseDockerRef: the name normalized, `latest` when it has neither tag nor digest, the
/// tag dropped when it has both.
pub fn parse_docker_ref(text: &str) -> Result<Named, String> {
    let mut named = parse_normalized_named(text)?;
    if !named.tag.is_empty() && !named.digest.is_empty() {
        named.tag.clear();
    } else if named.tag.is_empty() && named.digest.is_empty() {
        named.tag = "latest".to_string();
    }
    Ok(named)
}

/// WithDigest.
pub fn with_digest(mut named: Named, digest: &str) -> Result<Named, String> {
    if !is_digest_pattern(digest) {
        return Err("invalid digest format".to_string());
    }
    named.digest = digest.to_string();
    Ok(named)
}

fn parse_normalized_named(text: &str) -> Result<Named, String> {
    if text.len() == 64 && text.bytes().all(|b| matches!(b, b'a'..=b'f' | b'0'..=b'9')) {
        return Err(format!(
            "invalid repository name ({text}), cannot specify 64-byte hexadecimal strings"
        ));
    }
    let (domain, remainder) = split_docker_domain(text);
    let remote = remainder.split(':').next().unwrap_or_default();
    if to_lower_str(remote) != remote {
        return Err(format!(
            "invalid reference format: repository name ({remote}) must be lowercase"
        ));
    }
    parse(&format!("{domain}/{remainder}"))
}

/// splitDockerDomain: the registry, Docker Hub when the first component names none,
/// and the rest, under `library/` for a name of Docker Hub's without a namespace.
fn split_docker_domain(name: &str) -> (String, String) {
    let Some((first, rest)) = name.split_once('/') else {
        return ("docker.io".to_string(), format!("library/{name}"));
    };
    let (domain, mut remote) = if first == "localhost" {
        (first, rest.to_string())
    } else if first == "index.docker.io" {
        ("docker.io", rest.to_string())
    } else if first.contains(['.', ':']) || to_lower_str(first) != first {
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
fn parse(text: &str) -> Result<Named, String> {
    let Some(named) = matched(text) else {
        if text.is_empty() {
            return Err("repository name must have at least one component".to_string());
        }
        if matched(&to_lower_str(text)).is_some() {
            return Err("repository name must be lowercase".to_string());
        }
        return Err("invalid reference format".to_string());
    };
    if named.path.len() > 255 {
        return Err("repository name must not be more than 255 characters".to_string());
    }
    if !named.digest.is_empty() {
        check_digest(&named.digest)?;
    }
    Ok(named)
}

/// ReferenceRegexp: `name[:tag][@digest]`, the name with a registry when its first
/// component reads as one and the rest still makes a name.
fn matched(text: &str) -> Option<Named> {
    let (rest, digest) = match text.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest)),
        None => (text, None),
    };
    if digest.is_some_and(|d| !is_digest_pattern(d)) {
        return None;
    }
    // A colon after the last slash starts the tag: a name has one only in its port.
    let (name, tag) = match rest.rfind(':') {
        Some(colon) if rest[colon..].find('/').is_none() => (&rest[..colon], &rest[colon + 1..]),
        _ => (rest, ""),
    };
    if rest.len() != name.len() && !is_tag(tag) {
        return None;
    }
    let (domain, path) = match name.split_once('/') {
        Some((domain, path)) if is_domain(domain) && is_remote_name(path) => (domain, path),
        _ if is_remote_name(name) => ("", name),
        _ => return None,
    };
    Some(Named {
        domain: domain.to_string(),
        path: path.to_string(),
        tag: tag.to_string(),
        digest: digest.unwrap_or_default().to_string(),
    })
}

/// `[\w][\w.-]{0,127}`
fn is_tag(text: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && word(bytes[0])
        && bytes[1..]
            .iter()
            .all(|&b| word(b) || b == b'.' || b == b'-')
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

/// The domain and port: dot-separated labels of letters, digits and inner hyphens, or
/// an IPv6 address in brackets, then an optional `:PORT`.
fn is_domain(text: &str) -> bool {
    let (host, port) = if text.starts_with('[') {
        match text.find(']') {
            Some(end) => (&text[..=end], &text[end + 1..]),
            None => return false,
        }
    } else {
        match text.split_once(':') {
            Some((host, _)) => (host, &text[host.len()..]),
            None => (text, ""),
        }
    };
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let label = |part: &str| {
        let bytes = part.as_bytes();
        !bytes.is_empty()
            && bytes[0].is_ascii_alphanumeric()
            && bytes[bytes.len() - 1].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let host_ok = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(address) => {
            !address.is_empty() && address.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
        }
        None => host.split('.').all(label),
    };
    port_ok && host_ok
}

/// Path components of lowercase letters and digits, joined inside by `.`, `_`, `__` or
/// runs of `-`, separated by `/`.
fn is_remote_name(text: &str) -> bool {
    text.split('/').all(|component| {
        let bytes = component.as_bytes();
        let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        if bytes.is_empty() || !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
            return false;
        }
        let mut i = 0;
        while i < bytes.len() {
            if alnum(bytes[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < bytes.len() && !alnum(bytes[i]) {
                i += 1;
            }
            let separator = &component[start..i];
            if !(matches!(separator, "." | "_" | "__") || separator.bytes().all(|b| b == b'-')) {
                return false;
            }
        }
        true
    })
}

/// go-digest's Parse of a digest the grammar let through.
fn check_digest(digest: &str) -> Result<(), String> {
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

    fn docker(text: &str) -> Result<String, String> {
        parse_docker_ref(text).map(|n| n.to_string())
    }

    #[test]
    fn names_are_spelled_out_as_docker_reads_them() {
        let hex = "0123456789abcdef".repeat(4);
        assert_eq!(
            docker("busybox").unwrap(),
            "docker.io/library/busybox:latest"
        );
        assert_eq!(
            docker("busybox:1.36").unwrap(),
            "docker.io/library/busybox:1.36"
        );
        assert_eq!(
            docker("library/busybox").unwrap(),
            "docker.io/library/busybox:latest"
        );
        assert_eq!(
            docker("orhunp/rustypaste").unwrap(),
            "docker.io/orhunp/rustypaste:latest"
        );
        assert_eq!(
            docker("index.docker.io/nginx").unwrap(),
            "docker.io/library/nginx:latest"
        );
        assert_eq!(
            docker("docker.io/nginx:alpine").unwrap(),
            "docker.io/library/nginx:alpine"
        );
        assert_eq!(
            docker("ghcr.io/project-zot/zot:v2.1.21").unwrap(),
            "ghcr.io/project-zot/zot:v2.1.21"
        );
        assert_eq!(docker("localhost/app").unwrap(), "localhost/app:latest");
        assert_eq!(
            docker("localhost:5000/app:1").unwrap(),
            "localhost:5000/app:1"
        );
        assert_eq!(docker("[::1]:5000/app").unwrap(), "[::1]:5000/app:latest");
        assert_eq!(
            docker("a__b/c--d.e_f").unwrap(),
            "docker.io/a__b/c--d.e_f:latest"
        );
        // A digest alone stays alone; with a tag, the tag goes.
        assert_eq!(
            docker(&format!("busybox@sha256:{hex}")).unwrap(),
            format!("docker.io/library/busybox@sha256:{hex}")
        );
        assert_eq!(
            docker(&format!("busybox:1.36@sha256:{hex}")).unwrap(),
            format!("docker.io/library/busybox@sha256:{hex}")
        );
        // An uppercase first component is a registry.
        assert_eq!(docker("Example/app").unwrap(), "Example/app:latest");
        // A first component that is no domain but a fine path stays in the path.
        assert_eq!(
            docker("my_registry.com/app").unwrap(),
            "my_registry.com/app:latest"
        );
    }

    #[test]
    fn what_docker_refuses_is_refused_with_its_words() {
        let hex = "0123456789abcdef".repeat(4);
        assert_eq!(
            docker("Busybox").unwrap_err(),
            "invalid reference format: repository name (library/Busybox) must be lowercase"
        );
        assert_eq!(
            docker(&hex).unwrap_err(),
            format!("invalid repository name ({hex}), cannot specify 64-byte hexadecimal strings")
        );
        assert_eq!(
            docker("busybox:${TAG}").unwrap_err(),
            "invalid reference format"
        );
        assert_eq!(docker("a//b").unwrap_err(), "invalid reference format");
        assert_eq!(docker("busybox:").unwrap_err(), "invalid reference format");
        assert_eq!(docker("-a").unwrap_err(), "invalid reference format");
        assert_eq!(docker("a@b@c").unwrap_err(), "invalid reference format");
        assert_eq!(
            docker(&format!("a:{}", "t".repeat(129))).unwrap_err(),
            "invalid reference format"
        );
        assert!(docker(&format!("a:{}", "t".repeat(128))).is_ok());
        assert_eq!(
            docker(&format!("a/{}", "b".repeat(260))).unwrap_err(),
            "repository name must not be more than 255 characters"
        );
        assert_eq!(
            docker(&format!("a@sha256:{}", "a".repeat(32))).unwrap_err(),
            "invalid checksum digest length"
        );
        assert_eq!(
            docker(&format!("a@sha256:{}", "A".repeat(64))).unwrap_err(),
            "invalid checksum digest format"
        );
        assert_eq!(
            docker(&format!("a@md5:{}", "a".repeat(32))).unwrap_err(),
            "unsupported digest algorithm"
        );
        assert_eq!(
            docker(&format!("a:t@Md5:{}", "a".repeat(32))).unwrap_err(),
            "invalid checksum digest format"
        );
        // Up to the first colon is the name, to be lowercase.
        assert_eq!(
            docker(&format!("a@Md5:{}", "a".repeat(32))).unwrap_err(),
            "invalid reference format: repository name (library/a@Md5) must be lowercase"
        );
    }

    #[test]
    fn a_digest_is_added_to_the_tag() {
        let hex = "0123456789abcdef".repeat(4);
        let named = parse_docker_ref("busybox").unwrap();
        assert_eq!(
            with_digest(named.clone(), &format!("sha256:{hex}"))
                .unwrap()
                .to_string(),
            format!("docker.io/library/busybox:latest@sha256:{hex}")
        );
        assert_eq!(
            with_digest(named, "sha256:abc").unwrap_err(),
            "invalid digest format"
        );
    }
}
