//! The bridge network: bringing it up, what is on it, and the hooks the machine units
//! run around a start and a stop.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::Result;

use crate::api::{machines, Context, Event, Report};
use crate::backend::BackendChoice;
use crate::bridge::{self, PortMap, Subnet};
use crate::hostnet;
use crate::oci::Mode;
use crate::settings::Network;
use crate::volmount;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInfo {
    pub bridge: String,
    pub subnet: Subnet,
    pub gateway: Ipv4Addr,
    /// The name every machine resolves to the host.
    pub host_name: String,
}

/// One machine on the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEntry {
    pub name: String,
    pub address: Option<Ipv4Addr>,
    pub ports: Vec<PortMap>,
    pub running: bool,
}

fn bridge_info(ctx: &Context) -> BridgeInfo {
    BridgeInfo {
        bridge: ctx.config.bridge.clone(),
        subnet: ctx.config.subnet,
        gateway: ctx.config.subnet.gateway(),
        host_name: bridge::HOST_NAME.to_string(),
    }
}

/// Creates the bridge, its NAT and firewall exceptions (start does this on its own; here
/// for boot-time setup and troubleshooting).
pub async fn up(ctx: &Context, report: Report<'_>) -> Result<BridgeInfo> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let _lock = store.lock_for(Duration::from_secs(60)).await?;
    bridge::up(&ctx.config, sd, report).await?;
    bridge::sync_ports(store, sd).await?;
    Ok(bridge_info(ctx))
}

/// The bridge and the machines on it, with their addresses and published ports.
pub async fn list(ctx: &Context) -> Result<(BridgeInfo, Vec<NetworkEntry>)> {
    let sd = ctx.sd().await?;
    let mut entries = Vec::new();
    for r in ctx.store.list_images()? {
        if r.network != Network::Bridge {
            continue;
        }
        entries.push(NetworkEntry {
            running: sd.machine_exists(&r.name).await?,
            name: r.name,
            address: r.address,
            ports: r.ports,
        });
    }
    Ok((bridge_info(ctx), entries))
}

/// ExecStartPre of systemd-nspawn@NAME.service: the same preparation `start` does, so
/// that machinectl, a boot-time enablement or a restart get their network too.
pub async fn prepare(ctx: &Context, name: &str) -> Result<()> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let _lock = store.lock_for(Duration::from_secs(60)).await?;
    let record = store.load_image(name)?;
    machines::prepare(sd, store, &ctx.config, name, record, &to_journal)
        .await
        .map(|_| ())
}

/// The hooks run under systemd: what the library remarks goes to the unit's journal.
fn to_journal(event: Event) {
    if let Event::Line(text) | Event::Note(text) = event {
        eprintln!("{text}");
    }
}

/// ExecStartPost: the machine is registered, its ports can be published. Volumes of an
/// mstack machine are attached first and without the store lock: the machine's boot is
/// waiting for them, and a long pull or create must not hold them up.
pub async fn publish(ctx: &Context, name: &str) -> Result<()> {
    let sd = ctx.sd().await?;
    let store = &ctx.store;
    let Some(record) = store.load_image(name)? else {
        return Ok(());
    };
    if record.backend == BackendChoice::Mstack {
        let leader = sd.machine_leader(name).await?;
        for volume in &record.volumes {
            let source = volume.host_path(&store.volumes_dir());
            volmount::mount_into_machine(leader, &source, &volume.target, volume.read_only)?;
        }
        // nspawn does not put the veth of a managed user namespace on the bridge.
        if record.network == Network::Bridge && record.mode == Mode::Boot {
            bridge::adopt_managed_veth(&ctx.config, leader)?;
        }
    }
    let _lock = store.lock_for(Duration::from_secs(60)).await?;
    match record.network {
        Network::Bridge => bridge::sync_ports(store, sd).await?,
        Network::Veth if hostnet::firewalld_running(sd).await => {
            hostnet::admit(sd, name, &to_journal).await?;
        }
        _ => {}
    }
    Ok(())
}

/// ExecStopPost: runs however the machine ended (stop, exit, crash, machinectl). No lock:
/// it runs inside the stop job that `images rm` and friends wait for while holding it.
pub fn release(ctx: &Context, name: &str) -> Result<()> {
    let record = ctx.store.load_image(name)?;
    machines::release_machine(name, record.as_ref())
}
