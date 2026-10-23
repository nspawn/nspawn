//! The IPv6 side of published ports. Machines have IPv4 addresses alone and nftables
//! cannot translate IPv6 into IPv4, so a port published on the host's IPv6 addresses is
//! served by a proxy in user space, as docker's userland proxy serves it:
//! `nspawn port-proxy NAME`, run by the transient unit `nspawn-proxy-NAME.service` from
//! the machine's publish hook, listens there and relays TCP connections and UDP
//! datagrams to the machine's address. The IPv4 side stays the DNAT's.

use std::collections::HashMap;
use std::io::{IoSlice, IoSliceMut};
use std::net::{Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use nix::sys::socket::{
    self, setsockopt, sockopt, AddressFamily, Backlog, ControlMessage, ControlMessageOwned,
    MsgFlags, SockFlag, SockType, SockaddrIn6,
};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::Semaphore;
use zbus::zvariant::OwnedValue;

use crate::api::{note, Context, Report};
use crate::bridge::Protocol;
use crate::store::ImageRecord;
use crate::systemd::Systemd;

/// docker-proxy's: a UDP flow nothing has crossed for this long is forgotten.
const UDP_IDLE: Duration = Duration::from_secs(90);
/// Clients of one UDP port relayed at once; a datagram of one more is dropped.
const UDP_FLOWS: usize = 4096;
/// TCP connections relayed at once by a machine's proxy; one more is closed.
const TCP_CONNECTIONS: usize = 16384;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an IPv6 address of the host may take to come up (duplicate address
/// detection at boot) before the proxy gives up on it.
const ADDRESS_WAIT: Duration = Duration::from_secs(30);
/// How often the proxy looks whether its machine still runs.
const WATCH: Duration = Duration::from_secs(5);

pub fn unit_name(name: &str) -> String {
    format!("nspawn-proxy-{name}.service")
}

/// Whether the host has IPv6 at all: a kernel booted with ipv6.disable=1 makes no
/// AF_INET6 socket.
pub fn host_has_ipv6() -> bool {
    socket::socket(
        AddressFamily::Inet6,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .is_ok()
}

/// A socket bound to an IPv6 address for IPv6 alone: the IPv4 side of the port is the
/// DNAT's, and a dual-stack socket would take it from it.
pub fn bind_v6(address: SocketAddrV6, protocol: Protocol) -> nix::Result<OwnedFd> {
    let kind = match protocol {
        Protocol::Tcp => SockType::Stream,
        Protocol::Udp => SockType::Datagram,
    };
    let fd = socket::socket(
        AddressFamily::Inet6,
        kind,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )?;
    setsockopt(&fd, sockopt::Ipv6V6Only, &true)?;
    if protocol == Protocol::Tcp {
        // A restart finds the port free while connections of the run before linger.
        setsockopt(&fd, sockopt::ReuseAddr, &true)?;
    }
    socket::bind(fd.as_raw_fd(), &SockaddrIn6::from(address))?;
    Ok(fd)
}

/// One listener of the proxy: where it listens and where it relays to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listener {
    pub address: SocketAddrV6,
    pub protocol: Protocol,
    pub target: SocketAddrV4,
}

/// The IPv6 listeners a machine's published ports ask for, towards its primary address,
/// where the DNAT sends the IPv4 side too.
pub fn listeners(record: &ImageRecord) -> Vec<Listener> {
    let Some(address) = record.address else {
        return Vec::new();
    };
    record
        .ports
        .iter()
        .filter_map(|p| {
            Some(Listener {
                address: SocketAddrV6::new(p.ipv6_side()?, p.host, 0, 0),
                protocol: p.protocol,
                target: SocketAddrV4::new(address, p.container),
            })
        })
        .collect()
}

/// Starts the proxy of a machine whose ports have an IPv6 side, on a host with IPv6;
/// a proxy already there is replaced, so that it reads the record anew. From the publish
/// hook, as the health runner is started: the unit's start job waits for the machine's.
pub async fn start(ctx: &Context, record: &ImageRecord) -> Result<()> {
    let name = record.name.as_str();
    let sd = ctx.sd().await?;
    if listeners(record).is_empty() || !host_has_ipv6() {
        return stop(sd, name).await;
    }
    let mut argv = crate::settings::hook_argv(&ctx.config)?;
    argv.extend(["port-proxy".to_string(), name.to_string()]);
    let machine_unit = format!("systemd-nspawn@{name}.service");
    let value = |v: zbus::zvariant::Value<'_>| {
        OwnedValue::try_from(v).expect("plain values carry no file descriptor")
    };
    let mut properties = crate::api::run::transient_service(
        &format!("IPv6 side of the published ports of machine {name}"),
        &argv,
    );
    properties.extend([
        ("Type".to_string(), value("notify".into())),
        // Requisite, not BindsTo: the proxy never pulls the machine in, and it leaves by
        // itself once the machine's run is over.
        (
            "Requisite".to_string(),
            value(vec![machine_unit.clone()].into()),
        ),
        ("After".to_string(), value(vec![machine_unit].into())),
        // Ports below 1024 and nothing else.
        (
            "CapabilityBoundingSet".to_string(),
            value((1u64 << 10).into()),
        ),
        // An allow list (true): the relays and the system bus.
        (
            "RestrictAddressFamilies".to_string(),
            value((true, vec!["AF_INET", "AF_INET6", "AF_UNIX"]).into()),
        ),
        ("LimitNOFILE".to_string(), value(65536u64.into())),
        ("LimitNOFILESoft".to_string(), value(65536u64.into())),
        ("TasksMax".to_string(), value(64u64.into())),
        ("MemoryMax".to_string(), value((512u64 << 20).into())),
        (
            "Environment".to_string(),
            value(vec!["TOKIO_WORKER_THREADS=2"].into()),
        ),
    ]);
    let unit = unit_name(name);
    if sd.unit_status(&unit).await?.busy() || sd.unit_queued(&unit).await? {
        sd.stop_unit(&unit).await?;
    }
    sd.start_transient(&unit, properties, "replace").await
}

/// Says so when the proxy of a machine just started is not up a few seconds after the
/// machine registered: a listener it could not bind fails its unit, which goes away
/// at once, and the IPv4 side of the ports works all the same.
pub async fn report_down(sd: &Systemd, record: &ImageRecord, report: Report<'_>) {
    if listeners(record).is_empty() || !host_has_ipv6() {
        return;
    }
    let name = record.name.as_str();
    let unit = unit_name(name);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok((_, active)) = sd.unit_state(&unit).await {
            if active == "active" {
                return;
            }
        }
        // A program that ended at once takes its proxy along.
        if !sd.machine_exists(name).await.unwrap_or(true) {
            return;
        }
        if Instant::now() > deadline {
            note(
                report,
                format!("the IPv6 side of the ports of {name} is not up; see journalctl -u {unit}"),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Stops a machine's proxy and waits for its listeners to be gone.
pub async fn stop(sd: &Systemd, name: &str) -> Result<()> {
    sd.stop_unit(&unit_name(name)).await
}

/// Queues the stop of a machine's proxy, for its release hook.
pub async fn stop_soon(sd: &Systemd, name: &str) -> Result<()> {
    sd.queue_stop(&unit_name(name)).await
}

/// `nspawn port-proxy NAME`, from the transient unit: binds the machine's IPv6
/// listeners, tells systemd it is ready, and relays until the machine's run is over.
pub async fn serve(ctx: &Context, name: &str) -> Result<()> {
    crate::reference::validate_entry_name(name)?;
    let record = ctx
        .store
        .load_image(name)?
        .with_context(|| format!("{name} is not an image managed by nspawn"))?;
    let wanted = listeners(&record);
    if wanted.is_empty() {
        notify_ready();
        return Ok(());
    }
    let sd = ctx.sd().await?;
    let unit = format!("systemd-nspawn@{name}.service");
    // The run it serves: a later run of the machine has a proxy of its own.
    let run = sd
        .invocation_id(&unit)
        .await
        .ok()
        .filter(|id| !id.is_empty());
    let mut bound = Vec::new();
    for listener in &wanted {
        bound.push((*listener, bind_waiting(listener).await?));
    }
    let connections = Arc::new(Semaphore::new(TCP_CONNECTIONS));
    for (listener, fd) in bound {
        match listener.protocol {
            Protocol::Tcp => {
                socket::listen(&fd, Backlog::new(1024).expect("a valid backlog"))
                    .with_context(|| format!("listening on {}", listener.address))?;
                let std_listener = std::net::TcpListener::from(fd);
                let tokio_listener = tokio::net::TcpListener::from_std(std_listener)
                    .with_context(|| format!("listening on {}", listener.address))?;
                tokio::spawn(relay_tcp(
                    tokio_listener,
                    listener.target,
                    connections.clone(),
                ));
            }
            Protocol::Udp => {
                setsockopt(&fd, sockopt::Ipv6RecvPacketInfo, &true)
                    .with_context(|| format!("listening on {}/udp", listener.address))?;
                let socket = AsyncFd::new(fd)
                    .with_context(|| format!("listening on {}/udp", listener.address))?;
                tokio::spawn(relay_udp(Arc::new(socket), listener.target));
            }
        }
    }
    notify_ready();
    loop {
        tokio::time::sleep(WATCH).await;
        // machined restarting or a call that timed out is asked again; only a machine
        // that is gone, or another run of it, ends the proxy, whose unit goes with it.
        match sd.machine_exists(name).await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(_) => continue,
        }
        if let (Some(run), Ok(now)) = (&run, sd.invocation_id(&unit).await) {
            if &now != run {
                return Ok(());
            }
        }
    }
}

/// Binds a listener, waiting for an address of the host that is not there yet: one
/// configured at boot is tentative until duplicate address detection is done.
async fn bind_waiting(listener: &Listener) -> Result<OwnedFd> {
    let started = Instant::now();
    loop {
        match bind_v6(listener.address, listener.protocol) {
            Ok(fd) => return Ok(fd),
            Err(nix::errno::Errno::EADDRNOTAVAIL) if started.elapsed() < ADDRESS_WAIT => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                return Err(anyhow!(
                    "binding {}/{}: {}",
                    listener.address,
                    listener.protocol.name(),
                    e.desc()
                ))
            }
        }
    }
}

/// systemd's readiness notification, for Type=notify: the listeners are bound.
fn notify_ready() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixDatagram;
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(socket) = UnixDatagram::unbound() else {
        return;
    };
    let bytes = path.as_bytes();
    let _ = match bytes.strip_prefix(b"@") {
        Some(abstract_name) => {
            use std::os::linux::net::SocketAddrExt;
            std::os::unix::net::SocketAddr::from_abstract_name(abstract_name)
                .and_then(|address| socket.send_to_addr(b"READY=1", &address))
        }
        None => socket.send_to(b"READY=1", &path),
    };
}

async fn relay_tcp(
    listener: tokio::net::TcpListener,
    target: SocketAddrV4,
    connections: Arc<Semaphore>,
) {
    loop {
        let client = match listener.accept().await {
            Ok((client, _)) => client,
            // Out of descriptors, mostly: the connections in flight free some.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(upstream)) =
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(target)).await
            else {
                return;
            };
            let (mut client, mut upstream) = (client, upstream);
            let _ = client.set_nodelay(true);
            let _ = upstream.set_nodelay(true);
            // Each direction ends on its own, a half-closed connection as it is.
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
    }
}

/// A UDP client: the address it wrote from, and the address and interface of the host
/// it wrote to, which the replies go out from (QUIC, on a host with several addresses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FlowKey {
    client: SocketAddrV6,
    local: Ipv6Addr,
    interface: u32,
}

struct Flow {
    socket: tokio::net::UdpSocket,
    /// Milliseconds since `epoch` of the last datagram either way.
    last: AtomicU64,
}

type Flows = Arc<Mutex<HashMap<FlowKey, Arc<Flow>>>>;

async fn relay_udp(listener: Arc<AsyncFd<OwnedFd>>, target: SocketAddrV4) {
    let flows: Flows = Arc::default();
    let epoch = Instant::now();
    let mut buf = vec![0u8; 65535];
    let mut control = nix::cmsg_space!(nix::libc::in6_pktinfo);
    loop {
        let received = listener
            .async_io(Interest::READABLE, |fd| {
                receive(fd, &mut buf, &mut control).map_err(std::io::Error::from)
            })
            .await;
        let Ok((len, key)) = received else { continue };
        let Some(key) = key else { continue };
        let existing = flows.lock().expect("flows").get(&key).cloned();
        let flow = match existing {
            Some(flow) => flow,
            None => {
                if flows.lock().expect("flows").len() >= UDP_FLOWS {
                    continue;
                }
                let Ok(socket) = tokio::net::UdpSocket::bind("0.0.0.0:0").await else {
                    continue;
                };
                if socket.connect(target).await.is_err() {
                    continue;
                }
                let flow = Arc::new(Flow {
                    socket,
                    last: AtomicU64::new(millis(epoch)),
                });
                flows.lock().expect("flows").insert(key, flow.clone());
                tokio::spawn(replies(
                    listener.clone(),
                    flows.clone(),
                    key,
                    flow.clone(),
                    epoch,
                ));
                flow
            }
        };
        flow.last.store(millis(epoch), Ordering::Relaxed);
        // A refusal the machine sent for an earlier datagram (ICMP) is no reason to drop
        // this one, nor the flow.
        let _ = flow.socket.send(&buf[..len]).await;
    }
}

/// One datagram from the listener: its length and who sent it to which address of the
/// host; None for a datagram without the address it came to.
fn receive(
    fd: &OwnedFd,
    buf: &mut [u8],
    control: &mut [u8],
) -> nix::Result<(usize, Option<FlowKey>)> {
    let mut iov = [IoSliceMut::new(buf)];
    let message =
        socket::recvmsg::<SockaddrIn6>(fd.as_raw_fd(), &mut iov, Some(control), MsgFlags::empty())?;
    let Some(client) = message.address else {
        return Ok((message.bytes, None));
    };
    let mut destination = None;
    for cmsg in message.cmsgs()? {
        if let ControlMessageOwned::Ipv6PacketInfo(info) = cmsg {
            destination = Some((Ipv6Addr::from(info.ipi6_addr.s6_addr), info.ipi6_ifindex));
        }
    }
    let key = destination.map(|(local, interface)| FlowKey {
        client: SocketAddrV6::from(client),
        local,
        interface,
    });
    Ok((message.bytes, key))
}

/// Relays a flow's replies to its client until nothing crossed it for UDP_IDLE.
async fn replies(
    listener: Arc<AsyncFd<OwnedFd>>,
    flows: Flows,
    key: FlowKey,
    flow: Arc<Flow>,
    epoch: Instant,
) {
    let mut buf = vec![0u8; 65535];
    let info = nix::libc::in6_pktinfo {
        ipi6_addr: nix::libc::in6_addr {
            s6_addr: key.local.octets(),
        },
        ipi6_ifindex: key.interface,
    };
    let client = SockaddrIn6::from(key.client);
    loop {
        let idle = Duration::from_millis(millis(epoch) - flow.last.load(Ordering::Relaxed));
        let Some(left) = UDP_IDLE.checked_sub(idle).filter(|d| !d.is_zero()) else {
            break;
        };
        let len = match tokio::time::timeout(left, flow.socket.recv(&mut buf)).await {
            Ok(Ok(len)) => len,
            // The machine refused a datagram (ICMP): the flow goes on.
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
            Ok(Err(_)) => break,
            Err(_) => continue,
        };
        flow.last.store(millis(epoch), Ordering::Relaxed);
        let _ = listener
            .async_io(Interest::WRITABLE, |fd| {
                socket::sendmsg(
                    fd.as_raw_fd(),
                    &[IoSlice::new(&buf[..len])],
                    &[ControlMessage::Ipv6PacketInfo(&info)],
                    MsgFlags::empty(),
                    Some(&client),
                )
                .map_err(std::io::Error::from)
            })
            .await;
    }
    flows.lock().expect("flows").remove(&key);
}

fn millis(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::PortMap;

    fn record(ports: &[&str]) -> ImageRecord {
        let mut r: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "web",
            "reference": "r",
            "manifest_digest": "sha256:00",
            "layers": [],
            "backend": "overlay",
            "created": 0,
        }))
        .unwrap();
        r.address = Some("10.99.0.5".parse().unwrap());
        r.ports = ports
            .iter()
            .map(|p| p.parse::<PortMap>().unwrap())
            .collect();
        r
    }

    #[test]
    fn the_ipv6_side_of_each_port_is_listened_on() {
        let r = record(&[
            "8080:80",
            "0.0.0.0:8081:81",
            "127.0.0.1:8082:82",
            "[::]:8083:83",
            "[2001:db8::1]:53:53/udp",
        ]);
        let got: Vec<String> = listeners(&r)
            .iter()
            .map(|l| format!("{} {} {}", l.address, l.protocol.name(), l.target))
            .collect();
        assert_eq!(
            got,
            [
                "[::]:8080 tcp 10.99.0.5:80",
                "[::]:8083 tcp 10.99.0.5:83",
                "[2001:db8::1]:53 udp 10.99.0.5:53",
            ]
        );
        let mut none = r.clone();
        none.address = None;
        assert!(
            listeners(&none).is_empty(),
            "no address, nowhere to relay to"
        );
    }

    #[test]
    fn an_ipv6_listener_leaves_the_ipv4_port_free() {
        if !host_has_ipv6() {
            return;
        }
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        // The IPv4 side is taken; the IPv6 side binds all the same, for IPv6 alone.
        let fd = bind_v6(
            SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0),
            Protocol::Tcp,
        );
        assert!(fd.is_ok(), "{fd:?}");
        let held = fd.unwrap();
        // A listener holds it, as the proxy's does.
        socket::listen(&held, Backlog::new(1).unwrap()).unwrap();
        assert_eq!(
            bind_v6(
                SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0),
                Protocol::Tcp
            )
            .unwrap_err(),
            nix::errno::Errno::EADDRINUSE
        );
        drop(held);
    }

    #[tokio::test]
    async fn tcp_and_udp_are_relayed_to_the_target() {
        if !host_has_ipv6() || std::net::UdpSocket::bind("[::1]:0").is_err() {
            return;
        }
        // The machine: an echo on 127.0.0.1 for each protocol.
        let tcp_target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_port = tcp_target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = tcp_target.accept().await.unwrap();
            let (mut r, mut w) = s.split();
            tokio::io::copy(&mut r, &mut w).await.unwrap();
        });
        let udp_target = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let udp_port = udp_target.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let (n, from) = udp_target.recv_from(&mut buf).await.unwrap();
                udp_target.send_to(&buf[..n], from).await.unwrap();
            }
        });
        let local = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 0);
        // The proxy, on ::1.
        let tcp_fd = bind_v6(local, Protocol::Tcp).unwrap();
        socket::listen(&tcp_fd, Backlog::new(16).unwrap()).unwrap();
        let tcp_listener =
            tokio::net::TcpListener::from_std(std::net::TcpListener::from(tcp_fd)).unwrap();
        let tcp_at = tcp_listener.local_addr().unwrap();
        tokio::spawn(relay_tcp(
            tcp_listener,
            SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, tcp_port),
            Arc::new(Semaphore::new(4)),
        ));
        let udp_fd = bind_v6(local, Protocol::Udp).unwrap();
        setsockopt(&udp_fd, sockopt::Ipv6RecvPacketInfo, &true).unwrap();
        let udp_std = std::net::UdpSocket::from(udp_fd.try_clone().unwrap());
        let udp_at = udp_std.local_addr().unwrap();
        tokio::spawn(relay_udp(
            Arc::new(AsyncFd::new(udp_fd).unwrap()),
            SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, udp_port),
        ));
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client = tokio::net::TcpStream::connect(tcp_at).await.unwrap();
        client.write_all(b"over tcp").await.unwrap();
        client.shutdown().await.unwrap();
        let mut back = Vec::new();
        client.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"over tcp");
        let udp_client = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
        udp_client.send_to(b"over udp", udp_at).await.unwrap();
        let mut buf = [0u8; 64];
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(5), udp_client.recv_from(&mut buf))
                .await
                .expect("a reply")
                .unwrap();
        assert_eq!(&buf[..n], b"over udp");
        assert_eq!(from, udp_at, "the reply comes from the address written to");
    }
}
