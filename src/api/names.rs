//! Names alone, for a shell's completion: what `ps`, `images ls`, `network ls` and
//! `volume ls` list, and nothing else about it. machined tells any user the names of
//! its machines and images; these are no more than that, so the service hands them out
//! without asking polkit, which a completion could not answer anyway.

use std::collections::BTreeSet;

use anyhow::Result;

use crate::api::Context;

/// The machines that run, and with `all` every machine nspawn keeps a record of too.
pub async fn machines(ctx: &Context, all: bool) -> Result<Vec<String>> {
    let sd = ctx.sd().await?;
    let running = sd.list_machines().await?.into_iter().map(|m| m.name);
    if !all {
        return Ok(visible(running));
    }
    let records = ctx.store.list_images()?.into_iter().map(|r| r.name);
    Ok(visible(running.chain(records)))
}

/// The images machined knows, nspawn's among them: what `start` takes.
pub async fn images(ctx: &Context) -> Result<Vec<String>> {
    let sd = ctx.sd().await?;
    Ok(visible(sd.list_images().await?.into_iter().map(|i| i.name)))
}

/// The references of the local images: what `run` makes a machine from without a pull.
pub fn references(ctx: &Context) -> Result<Vec<String>> {
    Ok(visible(
        ctx.store.list_images()?.into_iter().map(|r| r.reference),
    ))
}

/// The networks, the default one ("bridge") among them.
pub fn networks(ctx: &Context) -> Result<Vec<String>> {
    Ok(visible(
        crate::api::network::all(&ctx.store, &ctx.config)?
            .into_iter()
            .map(|n| n.name),
    ))
}

/// The named volumes.
pub fn volumes(ctx: &Context) -> Result<Vec<String>> {
    Ok(visible(
        crate::api::volumes::list(&ctx.store)?
            .into_iter()
            .map(|v| v.name),
    ))
}

/// Sorted, once each, without the empty ones and those machined hides (".host").
fn visible(names: impl Iterator<Item = String>) -> Vec<String> {
    names
        .filter(|n| !n.is_empty() && !n.starts_with('.'))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig};
    use crate::store::ImageRecord;

    fn context() -> (tempfile::TempDir, Context) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::merge(FileConfig::default(), None, None).unwrap();
        config.machines_dir = tmp.path().join("machines");
        config.state_dir = tmp.path().join("state");
        let ctx = Context::new(config);
        ctx.store.init().unwrap();
        (tmp, ctx)
    }

    fn record(ctx: &Context, name: &str, reference: &str) {
        let record: ImageRecord = serde_json::from_str(&format!(
            r#"{{"name": "{name}", "reference": "{reference}", "manifest_digest": "d", "layers": [], "backend": "overlay", "created": 0}}"#
        ))
        .unwrap();
        ctx.store.record_image(&record).unwrap();
    }

    #[test]
    fn names_are_sorted_once_each_without_hidden_ones() {
        let names = ["web", ".host", "db", "", "web"].map(String::from);
        assert_eq!(visible(names.into_iter()), ["db", "web"]);
    }

    #[test]
    fn references_networks_and_volumes_come_from_the_store() {
        let (_tmp, ctx) = context();
        assert_eq!(references(&ctx).unwrap(), Vec::<String>::new());
        assert_eq!(networks(&ctx).unwrap(), ["bridge"]);
        assert_eq!(volumes(&ctx).unwrap(), Vec::<String>::new());
        record(&ctx, "fedora-44", "hub.nspawn.org/fedora:44");
        record(&ctx, "web", "docker.io/library/nginx:1.27");
        record(&ctx, "web-2", "docker.io/library/nginx:1.27");
        ctx.store
            .record_network(&crate::bridge::NetSpec {
                name: "backend".into(),
                interface: "nsbr-backend".into(),
                subnet: "10.99.7.0/24".parse().unwrap(),
                internal: false,
                created: 0,
                labels: Default::default(),
            })
            .unwrap();
        std::fs::create_dir_all(ctx.store.volumes_dir().join("pgdata")).unwrap();
        assert_eq!(
            references(&ctx).unwrap(),
            ["docker.io/library/nginx:1.27", "hub.nspawn.org/fedora:44"]
        );
        assert_eq!(networks(&ctx).unwrap(), ["backend", "bridge"]);
        assert_eq!(volumes(&ctx).unwrap(), ["pgdata"]);
    }
}
