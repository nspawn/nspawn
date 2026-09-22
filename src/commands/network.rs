use anyhow::Result;

use crate::bridge;
use crate::cli::BackendChoice;
use crate::commands::machines;
use crate::config::Config;
use crate::hostnet;
use crate::output::table;
use crate::settings::Network;
use crate::store::Store;
use crate::systemd::Systemd;
use crate::volmount;

/// Creates the bridge, its NAT and firewall exceptions (start does this on its own; here
/// for boot-time setup and troubleshooting).
pub async fn up(config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    bridge::up(config, &sd).await?;
    bridge::sync_ports(&store, &sd).await?;
    println!(
        "{} is up: {} on {}",
        config.bridge,
        config.subnet.gateway(),
        config.subnet
    );
    Ok(())
}

/// The machines on the bridge, their addresses and published ports.
pub async fn ls(config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    println!(
        "{} {} (gateway {}, host name {})",
        config.bridge,
        config.subnet,
        config.subnet.gateway(),
        bridge::HOST_NAME
    );
    let mut rows = Vec::new();
    for r in store.list_images()? {
        if r.network != Network::Bridge {
            continue;
        }
        let state = if sd.machine_exists(&r.name).await? {
            "running"
        } else {
            "stopped"
        };
        let ports = if r.ports.is_empty() {
            "-".to_string()
        } else {
            r.ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        rows.push(vec![
            r.name,
            r.address
                .map(|a| a.to_string())
                .unwrap_or_else(|| "-".into()),
            ports,
            state.to_string(),
        ]);
    }
    println!("{}", table(&["MACHINE", "ADDRESS", "PORTS", "STATE"], rows));
    Ok(())
}

/// ExecStartPre of systemd-nspawn@NAME.service: the same preparation `start` does, so
/// that machinectl, a boot-time enablement or a restart get their network too.
pub async fn prepare(config: &Config, name: &str) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let _lock = store.lock_for(std::time::Duration::from_secs(60))?;
    let record = store.load_image(name)?;
    machines::prepare(&sd, &store, config, name, record)
        .await
        .map(|_| ())
}

/// ExecStartPost: the machine is registered, its ports can be published.
pub async fn publish(config: &Config, name: &str) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let _lock = store.lock_for(std::time::Duration::from_secs(60))?;
    let Some(record) = store.load_image(name)? else {
        return Ok(());
    };
    match record.network {
        Network::Bridge => bridge::sync_ports(&store, &sd).await?,
        Network::Veth if hostnet::firewalld_running(&sd).await => {
            hostnet::admit(&sd, name).await?;
        }
        _ => {}
    }
    // Volumes of an mstack machine: attached from the host, the settings cannot carry them.
    if record.backend == BackendChoice::Mstack && !record.volumes.is_empty() {
        let leader = sd.machine_leader(name).await?;
        for volume in &record.volumes {
            let source = volume.host_path(&store.volumes_dir());
            volmount::mount_into_machine(leader, &source, &volume.target, volume.read_only)?;
        }
    }
    Ok(())
}

/// ExecStopPost: runs however the machine ended (stop, exit, crash, machinectl). No lock:
/// it runs inside the stop job that `images rm` and friends wait for while holding it.
pub async fn release(config: &Config, name: &str) -> Result<()> {
    let sd = Systemd::connect().await?;
    let store = Store::new(&config.machines_dir, &config.state_dir);
    let record = store.load_image(name)?;
    machines::release_machine(&sd, &store, name, record.as_ref()).await
}
