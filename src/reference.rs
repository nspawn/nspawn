//! Image references: `[registry/]repository[:tag][@digest]`.

use std::fmt;

use anyhow::{anyhow, bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ImageRef {
    /// Parses a reference. Without a host part the default registry is used; without a tag
    /// or digest the tag is `latest`.
    pub fn parse(input: &str, default_registry: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty image reference");
        }
        let (registry, rest) = match input.split_once('/') {
            Some((first, rest)) if looks_like_registry(first) => (first.to_string(), rest),
            _ => (default_registry.to_string(), input),
        };
        let (rest, digest) = match rest.split_once('@') {
            Some((r, d)) => (r, Some(d.to_string())),
            None => (rest, None),
        };
        let (repository, tag) = match rest.rsplit_once(':') {
            Some((r, t)) if !t.contains('/') => (r.to_string(), Some(t.to_string())),
            _ => (rest.to_string(), None),
        };
        if repository.is_empty() {
            bail!("reference {input:?} has no repository name");
        }
        if !repository.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/')
        }) {
            bail!("repository name {repository:?} may only contain lowercase letters, digits, '.', '_', '-' and '/'");
        }
        if let Some(d) = &digest {
            if !d.starts_with("sha256:")
                || d.len() != 71
                || !d[7..].chars().all(|c| c.is_ascii_hexdigit())
            {
                bail!("digest {d:?} is not a sha256 digest");
            }
        }
        let tag = match (tag, &digest) {
            (Some(t), _) => Some(t),
            (None, Some(_)) => None,
            (None, None) => Some("latest".to_string()),
        };
        Ok(ImageRef {
            registry,
            repository,
            tag,
            digest,
        })
    }

    pub fn to_oci(&self) -> Result<oci_client::Reference> {
        oci_client::Reference::try_from(self.to_string())
            .map_err(|e| anyhow!("invalid OCI reference {}: {e}", self))
    }

    /// The machine image name used locally: `fedora:44` becomes `fedora-44`, `debian:latest`
    /// becomes `debian`, a digest reference keeps twelve hex digits of the digest.
    pub fn local_name(&self) -> String {
        // Docker Hub's official images live under library/; nobody wants that in a name.
        let repository = match self.registry.as_str() {
            "docker.io" | "index.docker.io" | "registry-1.docker.io" => self
                .repository
                .strip_prefix("library/")
                .unwrap_or(&self.repository),
            _ => self.repository.as_str(),
        };
        let mut name = repository.replace('/', "-");
        match (&self.tag, &self.digest) {
            (Some(t), _) if t != "latest" => {
                name.push('-');
                name.push_str(t);
            }
            (None, Some(d)) => {
                name.push('-');
                name.push_str(&d[7..19]);
            }
            _ => {}
        }
        sanitize_machine_name(&name)
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

fn looks_like_registry(component: &str) -> bool {
    component == "localhost" || component.contains('.') || component.contains(':')
}

/// machined accepts names made of letters, digits, '.', '_' and '-', not starting with a dot.
pub fn sanitize_machine_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    out.truncate(64);
    out
}

pub fn validate_machine_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || name.starts_with('.') {
        bail!("invalid machine name {name:?}");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("machine name {name:?} may only contain letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUB: &str = "hub.nspawn.org";

    #[test]
    fn short_name_gets_default_registry_and_tag() {
        let r = ImageRef::parse("fedora:44", HUB).unwrap();
        assert_eq!(r.registry, HUB);
        assert_eq!(r.repository, "fedora");
        assert_eq!(r.tag.as_deref(), Some("44"));
        assert_eq!(r.to_string(), "hub.nspawn.org/fedora:44");
        assert_eq!(r.local_name(), "fedora-44");

        let r = ImageRef::parse("debian", HUB).unwrap();
        assert_eq!(r.tag.as_deref(), Some("latest"));
        assert_eq!(r.local_name(), "debian");
    }

    #[test]
    fn explicit_registry_with_port() {
        let r = ImageRef::parse("hub.nspawn.test:8443/fedora:44", HUB).unwrap();
        assert_eq!(r.registry, "hub.nspawn.test:8443");
        assert_eq!(r.repository, "fedora");
        assert_eq!(r.tag.as_deref(), Some("44"));
        let r = ImageRef::parse("localhost:5000/team/app", HUB).unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "team/app");
        assert_eq!(r.local_name(), "team-app");
        let r = ImageRef::parse("docker.io/library/busybox:1.36", HUB).unwrap();
        assert_eq!(r.local_name(), "busybox-1.36");
        let r = ImageRef::parse("docker.io/library/archlinux", HUB).unwrap();
        assert_eq!(r.local_name(), "archlinux");
        let r = ImageRef::parse("hub.example/library/x", HUB).unwrap();
        assert_eq!(r.local_name(), "library-x");
    }

    #[test]
    fn digest_reference() {
        let d = format!("sha256:{}", "ab".repeat(32));
        let r = ImageRef::parse(&format!("fedora@{d}"), HUB).unwrap();
        assert_eq!(r.tag, None);
        assert_eq!(r.digest.as_deref(), Some(d.as_str()));
        assert_eq!(r.local_name(), "fedora-abababababab");
        assert!(ImageRef::parse("fedora@sha256:zz", HUB).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(ImageRef::parse("", HUB).is_err());
        assert!(ImageRef::parse("Fedora:44", HUB).is_err());
        assert!(ImageRef::parse("hub.example/", HUB).is_err());
    }

    #[test]
    fn oci_conversion_round_trips() {
        let r = ImageRef::parse("fedora:44", HUB).unwrap();
        let o = r.to_oci().unwrap();
        assert_eq!(o.registry(), HUB);
        assert_eq!(o.repository(), "fedora");
        assert_eq!(o.tag(), Some("44"));
    }

    #[test]
    fn machine_names() {
        assert_eq!(sanitize_machine_name("a/b:c"), "a-b-c");
        assert_eq!(sanitize_machine_name("..x"), "x");
        assert!(validate_machine_name("fedora-44").is_ok());
        assert!(validate_machine_name(".hidden").is_err());
        assert!(validate_machine_name("a b").is_err());
    }
}
