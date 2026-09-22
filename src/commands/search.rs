use anyhow::Result;

use crate::cli::{SearchArgs, SearchSource};
use crate::config::Config;
use crate::hub::Hub;
use crate::output::table;
use crate::search::{self, Hit};

/// docker search: the hub first, then Docker Hub, each hit with its source and the
/// reference pull takes. A source that cannot be reached is reported, not fatal.
pub async fn run(args: SearchArgs, config: &Config) -> Result<()> {
    let mut hits: Vec<Hit> = Vec::new();
    if args.source.is_none_or(|s| s == SearchSource::Hub) {
        match Hub::new(config) {
            Ok(hub) => {
                match search::search_hub(&hub, &config.registry, &args.term, args.limit).await {
                    Ok(found) => hits.extend(found),
                    Err(e) => eprintln!("warning: {}: {e:#}", config.registry),
                }
            }
            Err(e) => eprintln!("warning: {}: {e:#}", config.registry),
        }
    }
    if args.source.is_none_or(|s| s == SearchSource::Dockerhub) {
        match search::search_docker_hub(&args.term, args.limit).await {
            Ok(found) => hits.extend(found),
            Err(e) => eprintln!("warning: {}: {e:#}", search::DOCKER_HUB),
        }
    }
    if hits.is_empty() {
        println!("nothing found for {:?}", args.term);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = hits
        .into_iter()
        .map(|h| {
            vec![
                h.source,
                h.name,
                shorten(&h.description, 60),
                match h.stars {
                    Some(n) => n.to_string(),
                    None => "-".to_string(),
                },
                if h.official { "yes" } else { "-" }.to_string(),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &["SOURCE", "NAME", "DESCRIPTION", "STARS", "OFFICIAL"],
            rows
        )
    );
    Ok(())
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max - 3).collect::<String>())
    }
}
