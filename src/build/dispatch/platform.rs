//! The platform specifiers of FROM --platform as containerd/platforms (v1.0.0-rc.5)
//! reads, normalizes and prints them.

use super::gofmt::quote;
use crate::build::plan::Platform;

const OS_RE: &str = r"^([A-Za-z0-9_-]+)(?:\(([A-Za-z0-9_.%-]*)((?:\+[A-Za-z0-9_.%-]+)*)\))?$";
const SPECIFIER_RE: &str = r"^[A-Za-z0-9_.-]+$";

/// A platform as specs.Platform holds it, the variant empty when there is none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Spec {
    pub(super) os: String,
    pub(super) os_version: String,
    pub(super) architecture: String,
    pub(super) variant: String,
}

impl Spec {
    pub(super) fn of(p: &Platform) -> Spec {
        Spec {
            os: p.os.clone(),
            os_version: String::new(),
            architecture: p.architecture.clone(),
            variant: p.variant.clone().unwrap_or_default(),
        }
    }

    /// Format: os/arch[/variant], "unknown" without an os.
    pub(super) fn format(&self) -> String {
        if self.os.is_empty() {
            return "unknown".to_string();
        }
        super::gofmt::join(&[&self.os, &self.architecture, &self.variant])
    }

    /// Normalize.
    pub(super) fn normalize(&self) -> Spec {
        let (architecture, variant) = normalize_arch(&self.architecture, &self.variant);
        Spec {
            // platforms.Normalize: no OS is the builder's.
            os: normalize_os(&self.os, "linux"),
            os_version: self.os_version.clone(),
            architecture,
            variant,
        }
    }
}

/// The same os, architecture and variant once normalized.
pub(super) fn same(a: &Spec, b: &Spec) -> bool {
    let (a, b) = (a.normalize(), b.normalize());
    a.os == b.os && a.architecture == b.architecture && a.variant == b.variant
}

fn is_word(b: u8, extra: &[u8]) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || extra.contains(&b)
}

/// The os part `name[(version[+feature]*)]`: the name and the version.
fn os_component(part: &str) -> Option<(&str, &str)> {
    let (name, options) = match part.find('(') {
        Some(i) => (&part[..i], Some(&part[i + 1..])),
        None => (part, None),
    };
    if name.is_empty() || !name.bytes().all(|b| is_word(b, b"")) {
        return None;
    }
    let Some(options) = options else {
        return Some((name, ""));
    };
    let inner = options.strip_suffix(')')?;
    let mut pieces = inner.split('+');
    let version = pieces.next().unwrap_or_default();
    if !version.bytes().all(|b| is_word(b, b".%")) {
        return None;
    }
    for feature in pieces {
        if feature.is_empty() || !feature.bytes().all(|b| is_word(b, b".%")) {
            return None;
        }
    }
    Some((name, version))
}

/// url.PathUnescape where the value has a percent sign.
fn decode_option(v: &str) -> Result<String, String> {
    if !v.contains('%') {
        return Ok(v.to_string());
    }
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: u8| (c as char).to_digit(16);
            match (
                b.get(i + 1).and_then(|&c| hex(c)),
                b.get(i + 2).and_then(|&c| hex(c)),
            ) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => {
                    let end = (i + 3).min(b.len());
                    return Err(format!(
                        "invalid URL escape {}",
                        quote(&String::from_utf8_lossy(&b[i..end]))
                    ));
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn is_known_os(os: &str) -> bool {
    matches!(
        os,
        "aix"
            | "android"
            | "darwin"
            | "dragonfly"
            | "freebsd"
            | "hurd"
            | "illumos"
            | "ios"
            | "js"
            | "linux"
            | "nacl"
            | "netbsd"
            | "openbsd"
            | "plan9"
            | "solaris"
            | "windows"
            | "zos"
    )
}

fn is_known_arch(arch: &str) -> bool {
    matches!(
        arch,
        "386"
            | "amd64"
            | "amd64p32"
            | "arm"
            | "armbe"
            | "arm64"
            | "arm64be"
            | "ppc64"
            | "ppc64le"
            | "loong64"
            | "mips"
            | "mipsle"
            | "mips64"
            | "mips64le"
            | "mips64p32"
            | "mips64p32le"
            | "ppc"
            | "riscv"
            | "riscv64"
            | "s390"
            | "s390x"
            | "sparc"
            | "sparc64"
            | "wasm"
    )
}

/// normalizeOS; `host` stands in for runtime.GOOS.
fn normalize_os(os: &str, host: &str) -> String {
    if os.is_empty() {
        return host.to_string();
    }
    let os = os.to_lowercase();
    if os == "macos" {
        "darwin".to_string()
    } else {
        os
    }
}

fn normalize_arch(arch: &str, variant: &str) -> (String, String) {
    let arch = arch.to_lowercase();
    let mut variant = variant.to_lowercase();
    let arch = match arch.as_str() {
        "i386" => {
            variant.clear();
            "386".to_string()
        }
        "x86_64" | "x86-64" | "amd64" => {
            if variant == "v1" {
                variant.clear();
            }
            "amd64".to_string()
        }
        "aarch64" | "arm64" => {
            match variant.as_str() {
                "8" | "v8" | "v8.0" => variant.clear(),
                "9" | "9.0" | "v9.0" => variant = "v9".to_string(),
                _ => {}
            }
            "arm64".to_string()
        }
        "armhf" => {
            variant = "v7".to_string();
            "arm".to_string()
        }
        "armel" => {
            variant = "v6".to_string();
            "arm".to_string()
        }
        "arm" => {
            match variant.as_str() {
                "" | "7" => variant = "v7".to_string(),
                "5" | "6" | "8" => variant = format!("v{variant}"),
                _ => {}
            }
            "arm".to_string()
        }
        _ => arch,
    };
    (arch, variant)
}

/// Parse; `host` stands in for the runtime's os and architecture where the specifier
/// leaves one out.
pub(super) fn parse(specifier: &str, host: &Platform) -> Result<Spec, String> {
    let invalid = "invalid argument";
    if specifier.contains('*') {
        return Err(format!(
            "{}: wildcards not yet supported: {invalid}",
            quote(specifier)
        ));
    }
    let parts: Vec<&str> = specifier.splitn(4, '/').collect();
    let mut p = Spec::default();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some((name, version)) = os_component(part) else {
                return Err(format!(
                    "{} is an invalid OS component of {}: OSAndVersion specifier component must match {}: {invalid}",
                    quote(part),
                    quote(specifier),
                    quote(OS_RE)
                ));
            };
            p.os = normalize_os(name, &host.os);
            p.os_version = decode_option(version).map_err(|e| {
                format!(
                    "{} has an invalid OS version {}: {e}",
                    quote(specifier),
                    quote(version)
                )
            })?;
        } else if part.is_empty() || !part.bytes().all(|b| is_word(b, b".")) {
            return Err(format!(
                "{} is an invalid component of {}: platform specifier component must match {}: {invalid}",
                quote(part),
                quote(specifier),
                quote(SPECIFIER_RE)
            ));
        }
    }
    match parts.len() {
        1 => {
            if is_known_os(&p.os) {
                p.architecture.clone_from(&host.architecture);
                return Ok(p);
            }
            let (arch, mut variant) = normalize_arch(parts[0], "");
            if arch == "arm" && variant == "v7" {
                variant.clear();
            }
            if is_known_arch(&arch) {
                p.architecture = arch;
                p.variant = variant;
                p.os.clone_from(&host.os);
                return Ok(p);
            }
            Err(format!(
                "{}: unknown operating system or architecture: {invalid}",
                quote(specifier)
            ))
        }
        2 => {
            let (arch, mut variant) = normalize_arch(parts[1], "");
            if arch == "arm" && variant == "v7" {
                variant.clear();
            }
            p.architecture = arch;
            p.variant = variant;
            Ok(p)
        }
        3 => {
            let (arch, mut variant) = normalize_arch(parts[1], parts[2]);
            if arch == "arm64" && variant.is_empty() {
                variant = "v8".to_string();
            }
            p.architecture = arch;
            p.variant = variant;
            Ok(p)
        }
        _ => Err(format!(
            "{}: cannot parse platform specifier: {invalid}",
            quote(specifier)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Platform {
        Platform {
            os: "linux".to_string(),
            architecture: "amd64".to_string(),
            variant: None,
        }
    }

    fn parsed(s: &str) -> String {
        parse(s, &host()).unwrap().normalize().format()
    }

    #[test]
    fn specifiers() {
        assert_eq!(parsed("linux/amd64"), "linux/amd64");
        assert_eq!(parsed("Linux/x86_64"), "linux/amd64");
        assert_eq!(parsed("linux"), "linux/amd64");
        assert_eq!(parsed("amd64"), "linux/amd64");
        assert_eq!(parsed("aarch64"), "linux/arm64");
        assert_eq!(parsed("linux/arm64/v8"), "linux/arm64");
        assert_eq!(parsed("linux/arm/v7"), "linux/arm/v7");
        assert_eq!(parsed("linux/armhf"), "linux/arm/v7");
        assert_eq!(parse("linux/arm", &host()).unwrap().variant, "");
        // A base image that names no OS is the builder's.
        let none = Spec {
            os: String::new(),
            os_version: String::new(),
            architecture: "amd64".to_string(),
            variant: String::new(),
        };
        assert_eq!(none.normalize().format(), "linux/amd64");
        assert_eq!(
            parse("nosuch", &host()).unwrap_err(),
            "\"nosuch\": unknown operating system or architecture: invalid argument"
        );
        assert_eq!(
            parse("linux/*", &host()).unwrap_err(),
            "\"linux/*\": wildcards not yet supported: invalid argument"
        );
        assert_eq!(
            parse("a/b/c/d", &host()).unwrap_err(),
            "\"a/b/c/d\": cannot parse platform specifier: invalid argument"
        );
        assert!(parse("lin ux/amd64", &host())
            .unwrap_err()
            .starts_with("\"lin ux\" is an invalid OS component of \"lin ux/amd64\""));
        assert!(parse("linux/am d64", &host())
            .unwrap_err()
            .starts_with("\"am d64\" is an invalid component of"));
    }
}
