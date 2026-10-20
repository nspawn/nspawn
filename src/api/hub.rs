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
    // From its parts, as the catalog: the configured registry may have no dot.
    let reference = Reference::with_tag(
        config.registry.clone(),
        repository.to_string(),
        "latest".to_string(),
    );
    hub.tags(&reference).await
}

/// The digest `reference` names on its registry now, without pulling it.
pub async fn digest(ctx: &Context, reference: &str) -> Result<String> {
    let image = ImageRef::parse(reference, &ctx.config.registry)?;
    let digest = Hub::new(&ctx.config)?.digest(&image.to_oci()?).await?;
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
