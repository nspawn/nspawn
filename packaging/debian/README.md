# Debian and Ubuntu package

`debian/` is the packaging; the `packages` workflow builds it in an Ubuntu
24.04 container (systemd 255, the oldest supported) with the crates vendored
beforehand:

```
cargo vendor vendor
mkdir -p .cargo && printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "vendor"\n' > .cargo/config.toml
cp -r packaging/debian/debian .
dpkg-buildpackage -us -uc -b -d
```

The build needs rust 1.88 or newer (`rust-version` in Cargo.toml). Ubuntu
24.04 ships 1.75, so the workflow installs rustup there and passes `-d`, which
leaves the `cargo` and `rustc` build dependencies unchecked; on a distribution
whose own rust is new enough they are what gets used, and `-d` can go.

The binary links against the glibc of the distribution it was built on, so a
package built on 24.04 runs on later releases and not the other way round.

## The APT repository

Every stable release is published at <https://apt.nspawn.org>. Once the
`release` job has attached the deb, the `apt` job tells the server to look:
the server takes the deb and `SHA256SUMS` from the GitHub release, checks the
one against the other and the package's name, version and architecture
against its file name, and keeps the last five releases in the suite `stable`,
component `main`, for amd64. Pre-releases are left out. The indexes are signed
with the Ed25519 key `1294612AB2DC4AE2F6118240FC5E133778B4B3B3`.

A host adds it with one file, which carries the key itself:

```
sudo curl -fsSLo /etc/apt/sources.list.d/nspawn.sources https://apt.nspawn.org/nspawn.sources
sudo apt update
sudo apt install nspawn
```

The same key is next to it as a keyring (`nspawn.gpg`) and armored
(`nspawn.asc`). The suite serves Ubuntu 24.04, Debian 13 and later releases
of both; on an older one apt refuses the package for its libc6.
