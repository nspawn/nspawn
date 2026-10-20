//! build: a Dockerfile build or an mkosi one, told apart by the flags and the context.

use std::path::Path;

use anyhow::{bail, Result};

use crate::cli::BuildArgs;

/// The names a Dockerfile goes by in a context, in the order they are looked for.
pub const DOCKERFILE_NAMES: [&str; 3] = ["Dockerfile", "dockerfile", "Containerfile"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dockerfile,
    Mkosi,
}

/// Which builder `args` ask for: a flag only one of them takes, else what the context
/// holds, a Dockerfile first.
pub fn kind(args: &BuildArgs) -> Result<Kind> {
    if let Some(text) = args.dockerfile.refused() {
        bail!("{text}");
    }
    let dockerfile = args.dockerfile.given();
    let mkosi = args.mkosi.given();
    if dockerfile && mkosi {
        bail!("the flags of a Dockerfile build and of an mkosi build do not go together");
    }
    if dockerfile || args.directory == Path::new("-") {
        return Ok(Kind::Dockerfile);
    }
    if mkosi {
        return Ok(Kind::Mkosi);
    }
    let dir = &args.directory;
    if DOCKERFILE_NAMES.iter().any(|n| dir.join(n).is_file()) {
        return Ok(Kind::Dockerfile);
    }
    if dir.join("mkosi.conf").is_file() || dir.join("mkosi.conf.d").is_dir() {
        return Ok(Kind::Mkosi);
    }
    bail!(
        "{} has no Dockerfile and no mkosi.conf; -f names a Dockerfile elsewhere",
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(argv: &[&str]) -> BuildArgs {
        let mut full = vec!["nspawn", "build"];
        full.extend(argv);
        match crate::cli::Cli::try_parse_from(full).unwrap().command {
            crate::cli::Command::Build(a) => a,
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_flags_and_the_context_pick_the_builder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        assert!(kind(&args(&[path])).is_err(), "an empty context");
        std::fs::write(dir.path().join("mkosi.conf"), "").unwrap();
        assert_eq!(kind(&args(&[path, "-t", "x:1"])).unwrap(), Kind::Mkosi);
        assert_eq!(
            kind(&args(&[path, "--build-arg", "A=1"])).unwrap(),
            Kind::Dockerfile
        );
        std::fs::write(dir.path().join("Containerfile"), "FROM scratch\n").unwrap();
        assert_eq!(kind(&args(&[path])).unwrap(), Kind::Dockerfile);
        assert_eq!(
            kind(&args(&[path, "-d", "fedora"])).unwrap(),
            Kind::Mkosi
        );
        assert!(kind(&args(&[path, "-d", "fedora", "--target", "x"])).is_err());
        assert_eq!(kind(&args(&["-"])).unwrap(), Kind::Dockerfile);
        let refused = kind(&args(&[path, "--ssh", "default"])).unwrap_err();
        assert!(refused.to_string().starts_with("--ssh:"), "{refused}");
        // -f is the Dockerfile; --force keeps its long form only.
        let a = args(&[path, "-f", "Other.Dockerfile", "--force", "-t", "a:1", "-t", "b:2"]);
        assert_eq!(a.dockerfile.file.as_deref(), Some("Other.Dockerfile"));
        assert!(a.force);
        assert_eq!(a.tag, ["a:1", "b:2"]);
    }
}
