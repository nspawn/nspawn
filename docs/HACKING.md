# Hacking

## Building

```
cargo build --release
```

The binary is `target/release/nspawn`. Dependencies are pinned in Cargo.lock;
`cargo update` is run deliberately, not as a side effect.

## Checks

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Unit tests need no root and touch nothing outside temporary directories.

## End-to-end suite

`tests/e2e.sh` exercises the whole thing against a real registry and
systemd-machined: pulls with every backend, boots, execs, stops, removes,
builds with mkosi, pushes, creates, publishes ports, mounts volumes, runs apps
from Docker Hub and logs in. It changes host state (bridge, nftables,
firewall zones, units), so it runs as root on a disposable VM only.

Requirements on the VM: systemd-nspawn and machined, overlayfs, `ip`, `nft`,
`curl`, `python3`, access to Docker Hub, a registry with the test image
(`fedora:44` by default), and mkosi for the build step. Install the binary
where a system service may execute it, since the unit hooks run it:

```
sudo install -m 755 target/release/nspawn /usr/local/bin/nspawn
sudo env NSPAWN=/usr/local/bin/nspawn NSPAWN_REGISTRY=hub.example:8443 \
    NSPAWN_CA_CERT=/etc/zot/ca.crt tests/e2e.sh
```

The suite cleans up before and after itself and ends with `ALL OK` or a
failure count. Every FAIL line names the check.

## Debugging a machine

- `journalctl -u systemd-nspawn@NAME.service` shows nspawn, the hooks and the
  machine's console; `nspawn logs NAME --all` is the same without the noise.
- `/etc/systemd/nspawn/NAME.nspawn` is what the machine was started with; it is
  regenerated on every start from `/var/lib/nspawn/images/NAME.json`.
- `nft list table ip nspawn` shows the DNAT map and NAT rules;
  `nspawn network ls` the addresses and ports.
- `ip netns list` shows the namespaces prepared for app machines.

## Releasing

1. Bump `version` in Cargo.toml and build so Cargo.lock follows.
2. Run the checks and the e2e suite.
3. Tag the commit with the version and publish the binary with its
   SHA256SUMS.
