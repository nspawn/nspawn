# Releasing

A release is a tag. The `packages` workflow builds the RPM, the deb and the
Arch package from it and attaches them to the GitHub release together with the
plain binary and a `SHA256SUMS`, so the only things done by hand are the
version, the documentation and the AUR.

## Before the tag

1. `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
   pass, and `tests/e2e.sh` passes on every host of the matrix (see
   [HACKING](HACKING.md)): the oldest supported systemd, a host with SELinux
   enforcing, and one with the mstack backend.
2. The documentation in this repository says what the code does. The pages that
   go stale first are the ones that quote paths, flags or defaults: README.md,
   docs/ARCHITECTURE.md, docs/DBUS.md and the `--help` texts in `src/cli.rs`.
3. The website follows, in the same pass. It is a separate repository
   (`github.com/nspawn/website`), maintained from here because this is where
   the change was made:

   | Page | Follows |
   | --- | --- |
   | `docs/reference.md` | `src/cli.rs`: every command, flag, default and value name |
   | `docs/configuration.md` | `src/config.rs` and the per-machine record |
   | `docs/getting-started.md` | the packaging, `src/daemon/install.rs`, the requirements |
   | `docs/overview.md` | `src/daemon/`, the polkit actions, the architecture |
   | `docs/images.md`, `docs/machines.md`, `docs/networking.md` | `src/api/`, `src/store.rs`, `src/bridge.rs`, `src/settings.rs` |
   | `docs/building.md` | `src/api/build.rs` and the mkosi arguments it passes |

   Its examples are checked against a real run, not from memory: the tables and
   the lines nspawn prints are easy to get subtly wrong.

## The tag

4. Bump `version` in Cargo.toml and build so Cargo.lock follows. The packaging
   carries the same version for whoever reads it: `%global upstream_version`
   and a `%changelog` entry in `packaging/fedora/nspawn.spec`, a new entry in
   `packaging/debian/debian/changelog`, and `pkgver` in
   `packaging/arch/PKGBUILD` (a release is its own tag, so the pre-release
   substitution in `source=` and the `cd` lines goes with it).
5. Commit that as the last commit, subject `Release X.Y.Z`.
6. Tag it and push the tag. The workflow does the rest; check the release page
   afterwards, and install from one of the packages on a host that never had a
   build of its own.

## After the tag

7. The AUR recipes (`nspawn` and `nspawn-git`) take the new `pkgver`,
   `updpkgsums` against the published tarball, a regenerated `.SRCINFO`, and a
   push each.
