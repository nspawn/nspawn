# Guidelines for AI agents

nspawn is a docker-like command line tool built on systemd-nspawn and
systemd-machined: images come from an OCI registry, machines are driven over
D-Bus, and the tool runs its own bridge network. Read these before changing
anything:

- README.md: what the tool does and how it is used.
- docs/ARCHITECTURE.md: modules, on-disk layout, networking, unit hooks.
- docs/HACKING.md: building, unit tests, clippy, the end-to-end suite, the test VM.
- CONTRIBUTING.md: commit messages and what every change must come with.

Rules of the road:

- Rust 2021. `unsafe` stays inside the namespace and mount helpers
  (`nsenter.rs`, `volmount.rs`). Errors use `anyhow` with a context that names
  the path, unit or machine involved.
- Every behaviour change comes with a unit test, and with an e2e step when it
  touches machines, the network or a registry.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
  must pass. The e2e suite runs as root on a disposable VM, never on a
  workstation.
- Anything that changes host state (bridge, nftables, firewalld, iptables,
  unit drop-ins, files under /etc) must be idempotent and undone by `stop` or
  `images rm`.
- Commit messages: short imperative subject, a body only for the why. No tool
  names, no signatures, no trailers.
- Do not push, tag or release unless asked.
- Documentation and messages are plain ASCII English without emojis.
