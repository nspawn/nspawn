//! The hub's catalog: repositories and tags.

use anyhow::Result;
use oci_client::Reference;

use crate::api::Context;
use crate::hub::Hub;

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
            let reference = Reference::try_from(format!("{}/{name}", config.registry))?;
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
    let reference = Reference::try_from(format!("{}/{repository}", config.registry))?;
    hub.tags(&reference).await
}
