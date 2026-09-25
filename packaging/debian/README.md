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
