//! What the host must provide for a machine with a virtual ethernet pair.
//!
//! systemd-nspawn only creates the pair. Its host end (`ve-<name>`) is brought up,
//! addressed, served by a DHCP server and masqueraded by systemd-networkd through the
//! stock 80-container-ve.network; without networkd the interface stays down and `host0`
//! inside the machine never sees a carrier. On hosts running firewalld the new interface
//! lands in the default zone, which drops the machine's DHCP requests, so it is bound to
//! the trusted zone while the machine runs (runtime configuration only, like docker does
//! with its own zone).

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::systemd::Systemd;

const NETWORKD: &str = "systemd-networkd.service";
const FIREWALLD: &str = "org.fedoraproject.FirewallD1";
const FIREWALLD_PATH: &str = "/org/fedoraproject/FirewallD1";
const FIREWALLD_ZONE: &str = "org.fedoraproject.FirewallD1.zone";
const ZONE: &str = "trusted";
const OWN_CONFIG_DIRS: [&str; 2] = ["/etc/systemd/network", "/run/systemd/network"];

/// Makes sure systemd-networkd runs on the host. It is started only when the host has no
/// .network files of its own, so that nspawn never takes over interfaces another network
/// manager is handling.
pub async fn ensure_networkd(sd: &Systemd) -> Result<()> {
    let (load, active) = sd.unit_state(NETWORKD).await?;
    if matches!(active.as_str(), "active" | "activating" | "reloading") {
        return Ok(());
    }
    if load == "masked" {
        bail!("{}", explanation("systemd-networkd is masked on this host"));
    }
    if let Some(dir) = own_network_config() {
        bail!(
            "{}",
            explanation(&format!(
                "systemd-networkd is not running and {dir} holds the host's own configuration, which nspawn does not activate by itself"
            ))
        );
    }
    eprintln!("starting systemd-networkd on the host to configure the machine's virtual ethernet");
    sd.start_unit(NETWORKD)
        .await
        .with_context(|| explanation("it could not be started"))
}

fn explanation(reason: &str) -> String {
    format!(
        "a machine with a virtual ethernet pair needs systemd-networkd on the host, which \
         brings up the host end of the pair with an address, a DHCP server and NAT \
         (80-container-ve.network); {reason}. Run `systemctl enable --now systemd-networkd` \
         after checking that it does not fight another network manager, or use --network host"
    )
}

/// The first directory holding .network files written on this host, if any.
fn own_network_config() -> Option<&'static str> {
    OWN_CONFIG_DIRS
        .into_iter()
        .find(|dir| has_network_files(Path::new(dir)))
}

fn has_network_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".network"))
        })
        .unwrap_or(false)
}

/// True when firewalld owns its bus name, i.e. filters this host's traffic.
pub async fn firewalld_running(sd: &Systemd) -> bool {
    sd.name_has_owner(FIREWALLD).await
}

/// Host-side interface names of a running machine (`ve-<name>` for a veth pair).
pub async fn machine_interfaces(sd: &Systemd, name: &str) -> Result<Vec<String>> {
    let indices = sd.machine_interfaces(name).await?;
    Ok(indices.into_iter().filter_map(interface_name).collect())
}

/// Lets the machine's traffic through firewalld by binding its host-side interfaces to
/// the trusted zone. Returns the names that were bound, for `release`.
pub async fn admit(sd: &Systemd, name: &str) -> Result<Vec<String>> {
    let interfaces = machine_interfaces(sd, name).await?;
    for ifname in &interfaces {
        trust_interface(sd, ifname).await?;
    }
    Ok(interfaces)
}

/// Binds one host interface to the trusted zone of firewalld (runtime configuration).
pub async fn trust_interface(sd: &Systemd, ifname: &str) -> Result<()> {
    match zone_call(sd, "addInterface", ifname).await {
        Ok(()) => Ok(()),
        Err(zbus::Error::MethodError(_, Some(message), _))
            if message.contains("ZONE_ALREADY_SET") =>
        {
            Ok(())
        }
        Err(e) => {
            Err(e).with_context(|| format!("adding {ifname} to the {ZONE} zone of firewalld"))
        }
    }
}

/// Undoes `admit` once the machine is gone. Best effort: the interface no longer exists.
pub async fn release(sd: &Systemd, interfaces: &[String]) {
    for ifname in interfaces {
        let _ = zone_call(sd, "removeInterface", ifname).await;
    }
}

async fn zone_call(sd: &Systemd, method: &str, ifname: &str) -> zbus::Result<()> {
    sd.connection()
        .call_method(
            Some(FIREWALLD),
            FIREWALLD_PATH,
            Some(FIREWALLD_ZONE),
            method,
            &(ZONE, ifname),
        )
        .await?;
    Ok(())
}

fn interface_name(index: i32) -> Option<String> {
    for entry in std::fs::read_dir("/sys/class/net").ok()?.flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path().join("ifindex")) {
            if text.trim().parse::<i32>().ok() == Some(index) {
                return Some(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_interface_one() {
        assert_eq!(interface_name(1).as_deref(), Some("lo"));
        assert_eq!(interface_name(i32::MAX), None);
    }

    #[test]
    fn only_network_files_count_as_own_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_network_files(dir.path()));
        std::fs::write(dir.path().join("10-eth.link"), "").unwrap();
        std::fs::create_dir(dir.path().join("80-foo.network.d")).unwrap();
        assert!(!has_network_files(dir.path()));
        std::fs::write(dir.path().join("20-wlan.network"), "").unwrap();
        assert!(has_network_files(dir.path()));
        assert!(!has_network_files(Path::new("/nonexistent")));
    }

    #[test]
    fn explanation_names_the_fix() {
        let text = explanation("because");
        assert!(text.contains("systemctl enable --now systemd-networkd"));
        assert!(text.contains("--network host"));
        assert!(text.contains("because"));
    }
}
