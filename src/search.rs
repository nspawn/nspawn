//! docker search across the sources nspawn knows: the configured hub (its catalog, with
//! the tags of every match) and Docker Hub (its search API). Every hit names its source
//! and the reference `pull` takes.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::hub::Hub;
use crate::reference::ImageRef;

pub const DOCKER_HUB: &str = "Docker Hub";
const DOCKER_HUB_SEARCH: &str = "https://index.docker.io/v1/search";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Where it comes from: the hub's host, or "Docker Hub".
    pub source: String,
    /// What to give `pull`.
    pub name: String,
    pub description: String,
    pub stars: Option<u64>,
    pub official: bool,
}

/// Repositories of the hub whose name contains `term`, with their tags.
pub async fn search_hub(hub: &Hub, registry: &str, term: &str, limit: usize) -> Result<Vec<Hit>> {
    let repos = filter_catalog(&hub.catalog(registry).await?, term);
    let mut hits = Vec::new();
    for repo in repos.into_iter().take(limit) {
        let image = ImageRef::parse(&repo, registry)?;
        let tags = hub.tags(&image.to_oci()?).await.unwrap_or_default();
        hits.push(Hit {
            source: registry.to_string(),
            name: repo,
            description: if tags.is_empty() {
                "(no tags)".to_string()
            } else {
                format!("tags: {}", tags.join(", "))
            },
            stars: None,
            official: false,
        });
    }
    Ok(hits)
}

/// Case-insensitive substring match on the repository names, in catalog order.
pub fn filter_catalog(repos: &[String], term: &str) -> Vec<String> {
    let needle = term.to_ascii_lowercase();
    repos
        .iter()
        .filter(|r| r.to_ascii_lowercase().contains(&needle))
        .cloned()
        .collect()
}

#[derive(Deserialize)]
struct DockerHubPage {
    #[serde(default)]
    results: Vec<DockerHubResult>,
}

#[derive(Deserialize)]
struct DockerHubResult {
    name: String,
    /// Docker Hub sends explicit nulls for these at times.
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    star_count: Option<u64>,
    #[serde(default)]
    is_official: Option<bool>,
}

/// What docker search itself queries.
pub async fn search_docker_hub(term: &str, limit: usize) -> Result<Vec<Hit>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("nspawn/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let body = client
        .get(DOCKER_HUB_SEARCH)
        .query(&[("q", term), ("n", &limit.to_string())])
        .send()
        .await
        .context("reaching Docker Hub")?
        .error_for_status()
        .context("Docker Hub search")?
        .bytes()
        .await
        .context("reading the Docker Hub answer")?;
    parse_docker_hub(&body)
}

pub fn parse_docker_hub(json: &[u8]) -> Result<Vec<Hit>> {
    let page: DockerHubPage =
        serde_json::from_slice(json).context("parsing the Docker Hub answer")?;
    Ok(page
        .results
        .into_iter()
        .map(|r| {
            let official = r.is_official.unwrap_or(false);
            Hit {
                source: DOCKER_HUB.to_string(),
                // Official images live under library/; the reference makes that explicit.
                name: if official && !r.name.contains('/') {
                    format!("docker.io/library/{}", r.name)
                } else {
                    format!("docker.io/{}", r.name)
                },
                description: r.description.unwrap_or_default().trim().to_string(),
                stars: Some(r.star_count.unwrap_or(0)),
                official,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_hub_answers_become_pullable_references() {
        let json = br#"{"num_results": 2, "results": [
            {"name": "busybox", "description": " Busybox base image. ", "star_count": 3517, "is_official": true},
            {"name": "someone/busybox-extra", "description": "", "star_count": 3, "is_official": false}
        ]}"#;
        let hits = parse_docker_hub(json).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "docker.io/library/busybox");
        assert_eq!(hits[0].description, "Busybox base image.");
        assert_eq!(hits[0].stars, Some(3517));
        assert!(hits[0].official);
        assert_eq!(hits[1].name, "docker.io/someone/busybox-extra");
        assert!(!hits[1].official);
        assert!(parse_docker_hub(b"{}").unwrap().is_empty());
        let nulls = br#"{"results": [{"name": "x", "description": null, "star_count": null, "is_official": null}]}"#;
        let hit = &parse_docker_hub(nulls).unwrap()[0];
        assert_eq!(
            (
                hit.name.as_str(),
                hit.description.as_str(),
                hit.stars,
                hit.official
            ),
            ("docker.io/x", "", Some(0), false)
        );
        assert!(parse_docker_hub(b"not json").is_err());
    }

    #[test]
    fn catalog_filter_is_case_insensitive() {
        let repos = vec![
            "fedora".to_string(),
            "e2e/built".to_string(),
            "Debian".to_string(),
        ];
        assert_eq!(filter_catalog(&repos, "DEB"), vec!["Debian"]);
        assert_eq!(
            filter_catalog(&repos, "e"),
            vec!["fedora", "e2e/built", "Debian"]
        );
        assert!(filter_catalog(&repos, "arch").is_empty());
    }
}
