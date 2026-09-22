//! docker search: the hub first, then Docker Hub, each hit with its source and the
//! reference pull takes.

use anyhow::Result;

use crate::api::{note, Context, Report};
use crate::hub::Hub;
use crate::search::{self, Hit, SearchSource};

/// A source that cannot be reached is reported as a note, not an error.
pub async fn search(
    ctx: &Context,
    term: &str,
    source: Option<SearchSource>,
    limit: usize,
    report: Report<'_>,
) -> Result<Vec<Hit>> {
    let config = &ctx.config;
    let mut hits: Vec<Hit> = Vec::new();
    if source.is_none_or(|s| s == SearchSource::Hub) {
        match Hub::new(config) {
            Ok(hub) => match search::search_hub(&hub, &config.registry, term, limit).await {
                Ok(found) => hits.extend(found),
                Err(e) => note(report, format!("warning: {}: {e:#}", config.registry)),
            },
            Err(e) => note(report, format!("warning: {}: {e:#}", config.registry)),
        }
    }
    if source.is_none_or(|s| s == SearchSource::Dockerhub) {
        match search::search_docker_hub(term, limit).await {
            Ok(found) => hits.extend(found),
            Err(e) => note(report, format!("warning: {}: {e:#}", search::DOCKER_HUB)),
        }
    }
    Ok(hits)
}
