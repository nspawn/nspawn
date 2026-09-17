use anyhow::Result;

use crate::bridge;
use crate::config::Config;
use crate::output::table;
use crate::settings::Network;
use crate::store::Store;
use crate::systemd::Systemd;

/// Creates the bridge, its NAT and firewall exceptions (start does this on its own; here
/// for boot-time setup and troubleshooting).
pub async fn up(config: &Config) -> Result<()> {
    let sd = Systemd::connect().await?;
    bridge::up(config, &sd).await?;
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
