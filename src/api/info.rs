//! Info: what the service is, what it keeps and the host it runs on, in one answer that
//! `nspawn info` and the Docker API's GET /info both print.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use crate::api::machines::MachineSummary;
use crate::api::Context;
use crate::backend::BackendChoice;
use crate::store::Store;

/// The machines by state. A record pulled or built that never ran is an image and not
/// counted here (`machines::is_machine`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Machines {
    pub total: u64,
    pub running: u64,
    pub paused: u64,
    /// Every other machine: stopped, starting or restarting.
    pub stopped: u64,
}

/// What the host says of itself.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Host {
    pub hostname: String,
    /// /etc/machine-id as it is written: 32 hexadecimal digits.
    pub machine_id: String,
    pub kernel: String,
    /// PRETTY_NAME of os-release, "Linux" without one.
    pub os: String,
    pub cpus: u64,
    /// Bytes.
    pub memory: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    /// The service's version, which may differ from its client's after an upgrade.
    pub version: String,
    pub machines: Machines,
    /// Distinct images, by their config: an image and the machines made from it are one.
    pub images: u64,
    pub machines_dir: String,
    pub state_dir: String,
    /// The backend pulls and creates use when none is named: auto, overlay, flat or
    /// mstack.
    pub backend: String,
    /// What auto means on this host: overlay where the kernel has overlayfs, flat
    /// otherwise.
    pub auto_backend: String,
    /// What a machine's output goes to without --log-driver.
    pub log_driver: String,
    pub registry: String,
    pub architecture: String,
    /// systemd's version as PID 1 gives it, empty when it cannot be read.
    pub systemd_version: String,
    pub host: Host,
}

/// The answer of Info.
pub async fn info(ctx: &Context) -> Result<Info> {
    let summaries = crate::api::machines::list(ctx, true).await?;
    let machines = count_machines(&ctx.store, &summaries);
    let images = image_count(&ctx.store)?;
    let systemd_version = ctx.sd().await?.version().await.unwrap_or_default();
    let auto_backend = if crate::backend::overlay_available() {
        "overlay"
    } else {
        "flat"
    };
    Ok(Info {
        version: env!("CARGO_PKG_VERSION").to_string(),
        machines,
        images,
        machines_dir: ctx.config.machines_dir.to_string_lossy().into_owned(),
        state_dir: ctx.config.state_dir.to_string_lossy().into_owned(),
        backend: backend_name(ctx.config.backend).to_string(),
        auto_backend: auto_backend.to_string(),
        log_driver: crate::tuning::DEFAULT_LOG_DRIVER.to_string(),
        registry: ctx.config.registry.clone(),
        architecture: std::env::consts::ARCH.to_string(),
        systemd_version,
        host: Host::read(Path::new("/")),
    })
}

fn backend_name(choice: BackendChoice) -> &'static str {
    match choice {
        BackendChoice::Auto => "auto",
        BackendChoice::Overlay => "overlay",
        BackendChoice::Flat => "flat",
        BackendChoice::Mstack => "mstack",
    }
}

/// The machines of a listing (`machines::list` with all) by state.
pub fn count_machines(store: &Store, summaries: &[MachineSummary]) -> Machines {
    let mut counts = Machines::default();
    for summary in summaries
        .iter()
        .filter(|s| crate::api::machines::is_machine(store, s))
    {
        counts.total += 1;
        match summary.state.as_str() {
            // machined's opening and closing are a run under way.
            "running" | "opening" | "closing" => counts.running += 1,
            "paused" => counts.paused += 1,
            _ => counts.stopped += 1,
        }
    }
    counts
}

/// The images the store holds, each config once, as the Docker API lists them: the
/// records of an image, of the machines made from it and of its other tags share it.
pub fn image_count(store: &Store) -> Result<u64> {
    let mut configs = BTreeSet::new();
    for record in store.list_images()? {
        let Ok(bytes) = store.load_manifest(&record.name) else {
            continue;
        };
        if let Ok(manifest) =
            serde_json::from_slice::<oci_client::manifest::OciImageManifest>(&bytes)
        {
            configs.insert(manifest.config.digest);
        }
    }
    Ok(configs.len() as u64)
}

impl Host {
    /// The host under `root`: `/` but in tests. What cannot be read is left empty.
    pub fn read(root: &Path) -> Host {
        let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap_or_default();
        // os-release(5): /etc's, or the vendor's in /usr/lib without it.
        let os = pretty_name(&read("etc/os-release"))
            .or_else(|| pretty_name(&read("usr/lib/os-release")))
            .unwrap_or_else(|| "Linux".to_string());
        Host {
            hostname: read("proc/sys/kernel/hostname").trim().to_string(),
            machine_id: read("etc/machine-id").trim().to_string(),
            kernel: read("proc/sys/kernel/osrelease").trim().to_string(),
            os,
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
            memory: mem_total(&read("proc/meminfo")),
        }
    }
}

/// PRETTY_NAME of an os-release file, unquoted.
fn pretty_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|v| !v.is_empty())
}

/// MemTotal of /proc/meminfo in bytes, 0 without it.
fn mem_total(text: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ImageRecord;

    fn record(name: &str, origin: &str) -> ImageRecord {
        serde_json::from_value(serde_json::json!({
            "name": name, "reference": "docker.io/library/busybox:latest",
            "manifest_digest": format!("sha256:{}", "d".repeat(64)),
            "layers": [], "backend": "overlay", "created": 1, "origin": origin,
        }))
        .unwrap()
    }

    fn summary(name: &str, state: &str, record: Option<ImageRecord>) -> MachineSummary {
        MachineSummary {
            name: name.to_string(),
            record,
            state: state.to_string(),
            started: None,
            leader: None,
            os: None,
            health: None,
            exit_code: None,
            finished: None,
            last_started: None,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        (tmp, store)
    }

    #[test]
    fn machines_are_counted_by_state_and_a_record_that_never_ran_is_an_image() {
        let (_tmp, store) = store();
        let summaries = [
            summary("web", "running", Some(record("web", "create"))),
            summary("boot", "opening", Some(record("boot", "create"))),
            summary("frozen", "paused", Some(record("frozen", "create"))),
            summary("done", "stopped", Some(record("done", "create"))),
            summary("again", "restarting", Some(record("again", "create"))),
            // Pulled and never started: an image.
            summary("pulled", "stopped", Some(record("pulled", "pull"))),
            // A machine nspawn keeps no record of is not nspawn's.
            summary("other", "running", None),
        ];
        assert_eq!(
            count_machines(&store, &summaries),
            Machines {
                total: 5,
                running: 2,
                paused: 1,
                stopped: 2
            }
        );
        // The same record once it runs is a machine.
        let running = [summary("pulled", "running", Some(record("pulled", "pull")))];
        assert_eq!(count_machines(&store, &running).running, 1);
    }

    #[test]
    fn images_are_counted_once_per_config() {
        let (_tmp, store) = store();
        let manifest = |config: char| {
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {
                    "mediaType": "application/vnd.oci.image.config.v1+json",
                    "digest": format!("sha256:{}", config.to_string().repeat(64)),
                    "size": 2,
                },
                "layers": [],
            }))
            .unwrap()
        };
        for (name, config) in [("a", 'a'), ("a-machine", 'a'), ("b", 'b')] {
            store.record_image(&record(name, "pull")).unwrap();
            store.save_manifest(name, &manifest(config)).unwrap();
        }
        // A record whose manifest is gone counts for nothing.
        store.record_image(&record("lost", "pull")).unwrap();
        assert_eq!(image_count(&store).unwrap(), 2);
    }

    #[test]
    fn the_host_is_read_from_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "etc/os-release",
            "NAME=Fedora\nPRETTY_NAME=\"Fedora Linux 44 (Server Edition)\"\n",
        );
        write("proc/sys/kernel/hostname", "box\n");
        write("proc/sys/kernel/osrelease", "6.17.1-300.fc44.x86_64\n");
        write("etc/machine-id", "0123456789abcdef0123456789abcdef\n");
        write(
            "proc/meminfo",
            "MemTotal:        8040736 kB\nMemFree: 1 kB\n",
        );
        let host = Host::read(root);
        assert_eq!(host.os, "Fedora Linux 44 (Server Edition)");
        assert_eq!(host.hostname, "box");
        assert_eq!(host.kernel, "6.17.1-300.fc44.x86_64");
        assert_eq!(host.machine_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(host.memory, 8040736 * 1024);
        assert!(host.cpus >= 1);
        // The vendor's os-release without /etc's, and Linux without either.
        std::fs::remove_file(root.join("etc/os-release")).unwrap();
        write("usr/lib/os-release", "PRETTY_NAME='Arch Linux'\n");
        assert_eq!(Host::read(root).os, "Arch Linux");
        std::fs::remove_file(root.join("usr/lib/os-release")).unwrap();
        let bare = Host::read(root);
        assert_eq!(bare.os, "Linux");
        std::fs::remove_file(root.join("proc/meminfo")).unwrap();
        assert_eq!(Host::read(root).memory, 0);
    }

    #[test]
    fn the_backend_is_named_as_the_configuration_spells_it() {
        assert_eq!(backend_name(BackendChoice::Auto), "auto");
        assert_eq!(backend_name(BackendChoice::Overlay), "overlay");
        assert_eq!(backend_name(BackendChoice::Flat), "flat");
        assert_eq!(backend_name(BackendChoice::Mstack), "mstack");
    }
}
