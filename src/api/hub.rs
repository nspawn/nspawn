//! The hub's catalog: repositories and tags, and the digest a reference names.

use anyhow::{bail, Result};
use oci_client::Reference;

use crate::api::Context;
use crate::hub::Hub;
use crate::reference::ImageRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub name: String,
    /// None when the tags were not asked for.
    pub tags: Option<Vec<String>>,
}

/// The repositories of the hub, those containing `filter` when given, with their tags
/// unless `with_tags` is off (one request per repository).
pub async fn repositories(
    ctx: &Context,
    filter: Option<&str>,
    with_tags: bool,
) -> Result<Vec<Repository>> {
    let config = &ctx.config;
    let hub = Hub::new(config)?;
    let mut found = Vec::new();
    for name in hub.catalog(&config.registry).await? {
        if let Some(f) = filter {
            if !name.contains(f) {
                continue;
            }
        }
        let tags = if with_tags {
            let reference =
                Reference::with_tag(config.registry.clone(), name.clone(), "latest".to_string());
            Some(hub.tags(&reference).await?)
        } else {
            None
        };
        found.push(Repository { name, tags });
    }
    Ok(found)
}

pub async fn tags(ctx: &Context, repository: &str) -> Result<Vec<String>> {
    let config = &ctx.config;
    let hub = Hub::new(config)?;
    hub.tags(&tags_reference(repository, &config.registry)?)
        .await
}

/// The repository `hub tags` lists, as a pull reads its name against the configured
/// registry, which may have no dot: Docker Hub's official ones under library/, the name
/// checked.
fn tags_reference(repository: &str, registry: &str) -> Result<Reference> {
    ImageRef::parse(repository, registry)?.to_oci()
}

/// Whether a registry's answer says it has nothing for the reference: no such
/// repository or manifest, or no access to it, which registries answer for a repository
/// they do not have as well.
fn registry_lacks(e: &anyhow::Error) -> bool {
    use oci_client::errors::{OciDistributionError as Oci, OciErrorCode as Code};
    e.chain().any(|cause| match cause.downcast_ref::<Oci>() {
        Some(Oci::RegistryError { envelope, .. }) => envelope.errors.iter().any(|e| {
            matches!(
                e.code,
                Code::ManifestUnknown
                    | Code::NameUnknown
                    | Code::NotFound
                    | Code::Denied
                    | Code::Unauthorized
            )
        }),
        Some(Oci::ServerError { code, .. }) => *code == 404,
        Some(Oci::UnauthorizedError { .. } | Oci::ImageManifestNotFoundError(_)) => true,
        _ => false,
    })
}

/// The digest `reference` names on its registry now, without pulling it; NoSuchImage
/// when the registry has nothing for it.
pub async fn digest(ctx: &Context, reference: &str) -> Result<String> {
    let image = ImageRef::parse(reference, &ctx.config.registry)?;
    let digest = match Hub::new(&ctx.config)?.digest(&image.to_oci()?).await {
        Ok(digest) => digest,
        Err(e) if registry_lacks(&e) => {
            return Err(crate::api::FailureKind::NoSuchImage.error(format!("{e:#}")))
        }
        Err(e) => return Err(e),
    };
    if !is_digest(&digest) {
        bail!("{image}: the registry answered {digest:?}, which is not a digest");
    }
    Ok(digest)
}

/// ALGORITHM:HEX, as registries name content.
fn is_digest(text: &str) -> bool {
    text.split_once(':').is_some_and(|(algorithm, hex)| {
        !algorithm.is_empty()
            && algorithm
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && hex.len() >= 32
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_without_the_image_is_told_from_one_that_failed() {
        use oci_client::errors::{
            OciDistributionError as Oci, OciEnvelope, OciError, OciErrorCode,
        };
        let answer = |code| {
            anyhow::Error::new(Oci::RegistryError {
                envelope: OciEnvelope {
                    errors: vec![OciError {
                        code,
                        message: String::new(),
                        detail: serde_json::Value::Null,
                    }],
                },
                url: "https://r/v2/x/manifests/1".into(),
            })
            .context("fetching the digest of r/x:1")
        };
        for code in [
            OciErrorCode::ManifestUnknown,
            OciErrorCode::NameUnknown,
            OciErrorCode::Denied,
            OciErrorCode::Unauthorized,
        ] {
            assert!(registry_lacks(&answer(code)));
        }
        assert!(!registry_lacks(&answer(OciErrorCode::Toomanyrequests)));
        let status = |code| {
            anyhow::Error::new(Oci::ServerError {
                code,
                url: String::new(),
                message: String::new(),
            })
        };
        assert!(registry_lacks(&status(404)));
        assert!(!registry_lacks(&status(503)));
        assert!(!registry_lacks(&anyhow::anyhow!("connection refused")));
    }

    #[test]
    fn tags_are_listed_for_the_repository_a_pull_would_take() {
        let hub = tags_reference("nginx", "docker.io").unwrap();
        assert_eq!(
            (hub.registry(), hub.repository()),
            ("docker.io", "library/nginx")
        );
        let own = tags_reference("e2e/au", "hub").unwrap();
        assert_eq!((own.registry(), own.repository()), ("hub", "e2e/au"));
        assert!(tags_reference("Nginx", "docker.io").is_err());
    }

    #[test]
    fn only_a_digest_is_handed_back() {
        let hex = "a".repeat(64);
        assert!(is_digest(&format!("sha256:{hex}")));
        assert!(is_digest(&format!("sha512:{}", "0".repeat(128))));
        assert!(!is_digest(&hex));
        assert!(!is_digest(&format!(":{hex}")));
        assert!(!is_digest("sha256:abc"));
        assert!(!is_digest(&format!("sha256:{}z", "a".repeat(63))));
        assert!(!is_digest(&format!("SHA256:{hex}")));
        assert!(!is_digest(&format!("sha256:{hex}\n")));
    }
}
