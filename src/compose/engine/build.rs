//! The images of services with a build section: built by nspawn's builder from their
//! context, named as docker compose names them, with the labels it gives them.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use zbus::zvariant::Value as Bus;

use super::progress::Status;
use super::Engine;
use crate::client::Options;
use crate::compose::value::{Map, Value};

fn s<'a>(map: &'a Map, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The image a service runs: its own, or PROJECT-SERVICE for one that only builds.
pub fn image_of(project: &str, name: &str, service: &Map) -> String {
    match s(service, "image") {
        "" => format!("{project}-{name}"),
        image => image.to_string(),
    }
}

/// What --build-arg and the build section's args come to, as KEY=VALUE: a key without a
/// value takes it from the project's environment, and is left out when it has none there.
fn build_args(
    build: &Map,
    overrides: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut args: BTreeMap<String, Option<String>> = BTreeMap::new();
    if let Some(map) = build.get("args").and_then(Value::as_map) {
        for (k, v) in map {
            args.insert(k.clone(), v.as_str().map(str::to_string));
        }
    }
    for arg in overrides {
        match arg.split_once('=') {
            Some((k, v)) => args.insert(k.to_string(), Some(v.to_string())),
            None => args.insert(arg.clone(), env(arg)),
        };
    }
    args.into_iter()
        .filter_map(|(k, v)| v.map(|v| format!("{k}={v}")))
        .collect()
}

/// The bytes of the project's secrets a build section names.
fn secrets(e: &Engine<'_>, build: &Map) -> Result<BTreeMap<String, Vec<u8>>> {
    let project = e.project();
    let mut out = BTreeMap::new();
    for item in build
        .get("secrets")
        .and_then(Value::as_seq)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let Some(item) = item.as_map() else { continue };
        let source = s(item, "source");
        let target = match s(item, "target") {
            "" => source,
            t => t,
        };
        let def = project
            .secrets
            .get(source)
            .and_then(Value::as_map)
            .with_context(|| format!("build secret {source} is not a secret of the project"))?;
        let bytes = match (s(def, "file"), s(def, "environment")) {
            (file, _) if !file.is_empty() => {
                std::fs::read(file).with_context(|| format!("secret {source}: reading {file}"))?
            }
            _ if def.contains_key("content") => s(def, "content").as_bytes().to_vec(),
            ("", "") => bail!("secret {source}: build secrets come from a file or the environment"),
            (_, var) => project
                .environment
                .get(var)
                .with_context(|| format!("secret {source}: {var} is not set"))?
                .as_bytes()
                .to_vec(),
        };
        out.insert(target.to_string(), bytes);
    }
    Ok(out)
}

/// What the build of a service asks for besides its section, from the command line.
#[derive(Debug, Default, Clone)]
pub struct Flags {
    pub build_args: Vec<String>,
    pub no_cache: bool,
    pub pull: bool,
    pub quiet: bool,
}

/// Builds the image of `name`, tagged with `image` and the section's tags; the local
/// name it is kept under.
pub async fn build(
    e: &Engine<'_>,
    name: &str,
    service: &Map,
    image: &str,
    flags: &Flags,
) -> Result<String> {
    let project = e.project();
    let build = service
        .get("build")
        .and_then(Value::as_map)
        .with_context(|| format!("service {name:?} has no build section"))?;
    let context = s(build, "context");
    if context.contains("://") || context.starts_with("git@") {
        bail!("service {name:?}: build context {context}: a remote context is not supported; clone it and point context at the directory");
    }
    for (key, what) in [
        ("ssh", "SSH agent forwarding"),
        ("additional_contexts", "named contexts"),
    ] {
        if build.get(key).is_some_and(|v| !matches!(v, Value::Null)) {
            bail!("service {name:?}: build.{key}: {what} is not supported by nspawn's builder");
        }
    }
    if build.get("privileged") == Some(&Value::Bool(true)) {
        bail!("service {name:?}: build.privileged: build steps run with the confinement of an app machine");
    }
    let host = format!("linux/{}", crate::oci::host_architecture());
    if let Some(platforms) = build.get("platforms").and_then(Value::as_seq) {
        for p in platforms.iter().filter_map(Value::as_str) {
            if p != host {
                bail!("service {name:?}: build.platforms {p}: builds run for this host's {host}");
            }
        }
    }
    let dir = std::path::PathBuf::from(context);
    let (dockerfile, dockerfile_path) = match s(build, "dockerfile_inline") {
        "" => {
            let file = match s(build, "dockerfile") {
                "" => "Dockerfile",
                f => f,
            };
            let path = dir.join(file);
            let bytes = std::fs::read(&path)
                .with_context(|| format!("service {name:?}: reading {}", path.display()))?;
            (bytes, Some(path))
        }
        inline => (inline.as_bytes().to_vec(), None),
    };
    let reference = super::image_reference(image)?;
    let mut tags = vec![reference.to_string()];
    for tag in build
        .get("tags")
        .and_then(Value::as_seq)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let Some(tag) = tag.as_str() {
            tags.push(super::image_reference(tag)?.to_string());
        }
    }
    let mut labels: Vec<String> = build
        .get("labels")
        .and_then(Value::as_map)
        .map(|m| {
            m.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default();
    labels.push(format!("{}={}", super::label::PROJECT, project.name));
    labels.push(format!("{}={name}", super::label::SERVICE));
    labels.push(format!(
        "{}={}",
        super::label::VERSION,
        super::compose_version()
    ));
    let mut options = Options::new();
    options.insert("dockerfile", Bus::from(dockerfile));
    options.insert("tags", Bus::from(tags));
    options.insert("labels", Bus::from(labels));
    let args = build_args(build, &flags.build_args, &|k| {
        project.environment.get(k).cloned()
    });
    if !args.is_empty() {
        options.insert("build_args", Bus::from(args));
    }
    if !s(build, "target").is_empty() {
        options.insert("target", Bus::from(s(build, "target").to_string()));
    }
    if !s(build, "network").is_empty() {
        options.insert("network", Bus::from(s(build, "network").to_string()));
    }
    if flags.no_cache || build.get("no_cache") == Some(&Value::Bool(true)) {
        options.insert("no_cache", Bus::from(true));
    }
    if flags.pull || build.get("pull") == Some(&Value::Bool(true)) {
        options.insert("pull", Bus::from(true));
    }
    let mut hosts = Vec::new();
    if let Some(extra) = build.get("extra_hosts").and_then(Value::as_map) {
        for (host, ips) in extra {
            for ip in ips.as_seq().map(Vec::as_slice).unwrap_or(&[]) {
                if let Some(ip) = ip.as_str() {
                    hosts.push(format!("{host}={ip}"));
                }
            }
        }
    }
    if !hosts.is_empty() {
        options.insert("add_hosts", Bus::from(hosts));
    }
    let ulimits = super::convert::ulimits(build);
    if !ulimits.is_empty() {
        options.insert("ulimits", Bus::from(ulimits));
    }
    let filter: Vec<String> = match build.get("no_cache_filter") {
        Some(Value::Seq(v)) => v
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::Str(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    if !filter.is_empty() {
        options.insert("no_cache_filter", Bus::from(filter));
    }
    if let Some(Value::Int(size)) = build.get("shm_size") {
        options.insert("shm_size", Bus::from(*size as u64));
    }
    let mut entitlements: Vec<String> = build
        .get("entitlements")
        .and_then(Value::as_seq)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // A build on the host's network is one the section asks for: it takes the
    // entitlement with it.
    if s(build, "network") == "host" && !entitlements.iter().any(|e| e == "network.host") {
        entitlements.push("network.host".to_string());
    }
    if !entitlements.is_empty() {
        options.insert("allow", Bus::from(entitlements));
    }
    let secrets = secrets(e, build)?;
    if !secrets.is_empty() {
        options.insert(
            "secrets",
            Bus::from(
                secrets
                    .into_iter()
                    .collect::<std::collections::HashMap<_, _>>(),
            ),
        );
    }
    let id = format!("Image {image}");
    e.progress.event(&id, "Building", "", Status::Working);
    let context = if dir.as_os_str().is_empty() {
        crate::commands::build::Context::Tar(Vec::new())
    } else {
        crate::commands::build::Context::Dir(dir)
    };
    match crate::commands::build::send(&e.client, options, context, dockerfile_path, flags.quiet)
        .await
    {
        Ok(_) => {
            e.progress.event(&id, "Built", "", Status::Done);
            Ok(reference.local_name())
        }
        Err(err) => {
            e.progress.event(&id, "Error", "", Status::Error);
            Err(err.context(format!("building the image of service {name:?}")))
        }
    }
}

/// compose build: the services named (all of them otherwise) that have a build
/// section, their dependencies too with --with-dependencies; --push sends their tags.
pub async fn run(e: &mut Engine<'_>, a: &crate::compose::cli::BuildArgs) -> Result<()> {
    let project = e.project().clone();
    let mut names: Vec<String> = if a.service.is_empty() {
        project.services.keys().cloned().collect()
    } else {
        if a.with_dependencies {
            let selected = project
                .clone()
                .with_selected_services(&a.service, false)
                .map_err(anyhow::Error::msg)?;
            selected.services.keys().cloned().collect()
        } else {
            a.service.clone()
        }
    };
    names.sort();
    let flags = Flags {
        build_args: a.build_arg.clone(),
        no_cache: a.no_cache,
        pull: a.pull,
        quiet: a.quiet,
    };
    for name in names {
        let service = super::convert::service(&project, &name)?;
        if service.get("build").and_then(Value::as_map).is_none() {
            continue;
        }
        let image = image_of(&project.name, &name, service);
        build(e, &name, service, &image, &flags).await?;
        if a.push && !s(service, "image").is_empty() {
            let mut tags = vec![image.clone()];
            tags.extend(
                service
                    .get("build")
                    .and_then(|b| b.get(&["tags"]))
                    .and_then(Value::as_seq)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string),
            );
            for tag in tags {
                let local = super::image_reference(&tag)?.local_name();
                let manager = &e.client.manager;
                e.client
                    .run_job_silent(|| manager.push_image(&local, Options::new()), a.quiet)
                    .await
                    .with_context(|| format!("pushing {tag}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_without_an_image_gets_the_project_s_name() {
        let service: Map = [("build".to_string(), Value::Map(Map::new()))].into();
        assert_eq!(image_of("demo", "web", &service), "demo-web");
        let named: Map = [("image".to_string(), Value::str("app:1"))].into();
        assert_eq!(image_of("demo", "web", &named), "app:1");
    }

    #[test]
    fn build_args_come_from_the_section_the_flags_and_the_environment() {
        let build: Map = [(
            "args".to_string(),
            Value::Map(
                [
                    ("A".to_string(), Value::str("1")),
                    ("UNSET".to_string(), Value::Null),
                ]
                .into(),
            ),
        )]
        .into();
        let env = |k: &str| (k == "FROM_ENV").then(|| "e".to_string());
        assert_eq!(
            build_args(
                &build,
                &["B=2".into(), "A=3".into(), "FROM_ENV".into(), "NONE".into()],
                &env
            ),
            ["A=3", "B=2", "FROM_ENV=e"]
        );
    }
}
