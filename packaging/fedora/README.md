# Fedora package

`nspawn.spec` is the packaging; the `packages` workflow builds it in a Fedora 44
container from two sources, the tree of the commit and its crates vendored
beforehand:

```
dnf install git rpm-build rpmdevtools cargo rust systemd systemd-rpm-macros selinux-policy-devel bzip2 tar xz
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rpmdev-setuptree
git archive --format=tar.gz --prefix="nspawn-${version}/" -o ~/rpmbuild/SOURCES/nspawn-${version}.tar.gz HEAD
cargo vendor vendor > /dev/null
tar cJf ~/rpmbuild/SOURCES/nspawn-${version}-vendor.tar.xz vendor
rpmbuild -ba packaging/fedora/nspawn.spec
```

`upstream_version` in the spec follows Cargo.toml; a pre-release tag like
1.0.0-beta.1 becomes Version 1.0.0, Release 0.1.beta1. The build makes two
packages: `nspawn`, and `nspawn-selinux` with the policy module of
`packaging/selinux` (see its README), which `nspawn` recommends on hosts with
the targeted policy.

The binary links against the glibc of the distribution it was built on, so a
package built on Fedora 44 runs on later releases and not the other way round.

## The dnf repository

Every stable release is published at <https://rpm.nspawn.org>. Once the
`release` job has attached the RPMs, the `packages` job tells the server to
look: the server takes `nspawn`, `nspawn-selinux` and `SHA256SUMS` from the
GitHub release, checks each RPM against its checksum and its name, version,
release and architecture against its file name, signs it, and keeps the last
five releases. Pre-releases are left out. The RPMs and the repository metadata
are signed with the Ed25519 key `1294612AB2DC4AE2F6118240FC5E133778B4B3B3`,
the key of apt.nspawn.org; an RPM of the repository therefore differs from the
one attached to the release by its signature.

A host adds it with one file, which turns on `gpgcheck` and `repo_gpgcheck`
and names the key:

```
sudo curl -fsSLo /etc/yum.repos.d/nspawn.repo https://rpm.nspawn.org/nspawn.repo
sudo dnf install nspawn
```

dnf asks to import the key and shows its fingerprint. The repository
serves Fedora 44 and later releases.
