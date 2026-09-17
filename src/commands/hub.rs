use anyhow::Result;
use oci_client::Reference;

use crate::cli::{HubLsArgs, HubTagsArgs};
use crate::config::Config;
use crate::hub::Hub;
use crate::output::table;

pub async fn ls(args: HubLsArgs, config: &Config) -> Result<()> {
    let hub = Hub::new(config)?;
    let repos = hub.catalog(&config.registry).await?;
    let mut rows = Vec::new();
    for repo in repos {
        if let Some(f) = &args.filter {
            if !repo.contains(f.as_str()) {
                continue;
            }
        }
        let tags = if args.no_tags {
            "-".to_string()
        } else {
            let reference = Reference::try_from(format!("{}/{repo}", config.registry))?;
            hub.tags(&reference).await?.join(", ")
        };
        rows.push(vec![repo, tags]);
    }
    if rows.is_empty() {
        println!("no repositories on {}", config.registry);
    } else {
        println!("{}", table(&["REPOSITORY", "TAGS"], rows));
    }
    Ok(())
}

pub async fn tags(args: HubTagsArgs, config: &Config) -> Result<()> {
    let hub = Hub::new(config)?;
    let reference = Reference::try_from(format!("{}/{}", config.registry, args.repository))?;
    for tag in hub.tags(&reference).await? {
        println!("{tag}");
    }
    Ok(())
}
