# Debian and Ubuntu package

`debian/` is the packaging; the `packages` workflow builds it in an Ubuntu
24.04 container (systemd 255, the oldest supported) with the crates vendored
beforehand:

```
cargo vendor vendor
mkdir -p .cargo && printf '[source.crates-io]\nreplace-with = "vendored-sources"\n[source.vendored-sources]\ndirectory = "vendor"\n' > .cargo/config.toml
cp -r packaging/debian/debian .
dpkg-buildpackage -us -uc -b
```
