# Releasing

A release is a tag. The `packages` workflow builds the RPM, the deb and the
Arch package from it and attaches them to the GitHub release together with the
plain binary and a `SHA256SUMS`, so the only things done by hand are the
version and the documentation; the AUR packages are the maintainer's.

## Before the tag

1. `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
   pass on every commit, and `tests/e2e.sh` passes on every host of the matrix
   (see [HACKING](HACKING.md)): the oldest supported systemd, a host with
   SELinux enforcing, and one with the mstack backend. It runs against the
   packages of the release commit, each built on its distribution (the rpm with
   `nspawn-selinux`, the deb, the Arch package) and installed there, with
   `NSPAWN=/usr/bin/nspawn`, and against a registry of our own: the suite logs
   in and pushes, which a public registry is not for.
2. The documentation in this repository says what the code does. The pages that
   go stale first are the ones that quote paths, flags or defaults: README.md,
   docs/USAGE.md, docs/ARCHITECTURE.md, docs/DBUS.md and the `--help` texts in
   `src/cli.rs`.
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

4. Write `docs/releases/X.Y.Z.md`: what is in, what changed since the last
   release, the requirements and the gaps. The workflow makes it the body of
   the release, and falls back to GitHub's summary of the commits when the file
   is not there.
5. Bump `version` in Cargo.toml and build so Cargo.lock follows. The packaging
   carries the same version for whoever reads it: `%global upstream_version`
   and a `%changelog` entry in `packaging/fedora/nspawn.spec`, a new entry in
   `packaging/debian/debian/changelog`, and `pkgver` (with `pkgrel=1`) in
   `packaging/arch/PKGBUILD`. The files under `packaging/arch` are copies of
   the maintainer's AUR recipes (`nspawn`, and `nspawn-git` in its own
   directory), which are the format; their checksum stays `SKIP` here, since
   the tarball of the tag does not exist yet, and the workflow builds with
   `--skipchecksums`.
6. Commit that as the last commit, subject `Release X.Y.Z`.
7. Tag it and push the tag. The workflow does the rest; check the release page
   afterwards, and install from one of the packages on a host that never had a
   build of its own.

## After the tag

8. The maintainer updates the AUR recipes (`nspawn` and `nspawn-git`). Their
   checksums come from the published tarball, which GitHub builds from wherever
   the tag points, so they are taken once the tag is final: moving it
   invalidates them and every `makepkg` then stops at the validity check. When
   the recipes change beyond the version, the copies under `packaging/arch`
   follow.
