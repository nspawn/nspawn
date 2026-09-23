# The vendored crates come as a second source, since the builders have no network.
%global selinuxtype targeted

# The one version to change: the upstream tag (Cargo.toml's). A pre-release tag
# like 1.0.0-beta.1 becomes Version 1.0.0, Release 0.1.beta1, as Fedora wants.
%global upstream_version 1.0.0-beta.1
%{lua:
  local tag = rpm.expand("%{upstream_version}")
  local base, pre = tag:match("^([%d.]+)%-beta%.(%d+)$")
  if base then
    rpm.define("base_version " .. base)
    rpm.define("base_release 0.1.beta" .. pre)
  else
    rpm.define("base_version " .. tag)
    rpm.define("base_release 1")
  end
}

Name:           nspawn
Version:        %{base_version}
Release:        %{base_release}%{?dist}
Summary:        Docker-like management of systemd-nspawn machines
License:        GPL-3.0-or-later
URL:            https://github.com/nspawn/nspawn
Source0:        %{name}-%{upstream_version}.tar.gz
Source1:        %{name}-%{upstream_version}-vendor.tar.xz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  systemd-rpm-macros
Requires:       systemd-container
Requires:       iproute
Requires:       nftables
Recommends:     (%{name}-selinux if selinux-policy-%{selinuxtype})
# Without it only root can call the service.
Recommends:     polkit
%{?systemd_requires}

%description
nspawn pulls OCI images from a registry, keeps them as shared layers, and
runs them as systemd-nspawn machines with a bridge network, published ports
and volumes, driven over the D-Bus APIs of systemd and machined. The work is
done by a service on the system bus, org.nspawn; the command line is its
client.

%package selinux
Summary:        SELinux policy for the nspawn service
BuildArch:      noarch
BuildRequires:  selinux-policy-devel
BuildRequires:  bzip2
Requires:       selinux-policy-%{selinuxtype}
Requires(post): selinux-policy-%{selinuxtype}
%{?selinux_requires}

%description selinux
The nspawn_t domain the org.nspawn service runs in on hosts with SELinux,
with the rules that let the system bus relay the pipes and pseudo terminals
it hands to its clients.

%prep
%setup -q -n %{name}-%{upstream_version}
tar xf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'CARGO'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
CARGO

%build
cargo build --release --offline
# Written from the command line's own definition, so they cannot drift from it.
for shell in bash zsh fish; do
    ./target/release/%{name} completions ${shell} > %{name}.${shell}
done
./target/release/%{name} manpage > %{name}.1
make -f %{_datadir}/selinux/devel/Makefile -C packaging/selinux %{name}.pp
bzip2 -9 packaging/selinux/%{name}.pp

%install
install -D -m 0755 target/release/%{name} %{buildroot}%{_bindir}/%{name}
install -D -m 0644 packaging/systemd/%{name}.service %{buildroot}%{_unitdir}/%{name}.service
install -D -m 0644 packaging/dbus/org.nspawn.service %{buildroot}%{_datadir}/dbus-1/system-services/org.nspawn.service
install -D -m 0644 packaging/dbus/org.nspawn.conf %{buildroot}%{_datadir}/dbus-1/system.d/org.nspawn.conf
install -D -m 0644 packaging/polkit/org.nspawn.policy %{buildroot}%{_datadir}/polkit-1/actions/org.nspawn.policy
install -D -m 0644 %{name}.bash %{buildroot}%{_datadir}/bash-completion/completions/%{name}
install -D -m 0644 %{name}.zsh %{buildroot}%{_datadir}/zsh/site-functions/_%{name}
install -D -m 0644 %{name}.fish %{buildroot}%{_datadir}/fish/vendor_completions.d/%{name}.fish
install -D -m 0644 %{name}.1 %{buildroot}%{_mandir}/man1/%{name}.1
install -D -m 0644 packaging/selinux/%{name}.pp.bz2 %{buildroot}%{_datadir}/selinux/packages/%{selinuxtype}/%{name}.pp.bz2
install -D -m 0644 packaging/selinux/%{name}.if %{buildroot}%{_datadir}/selinux/devel/include/contrib/%{name}.if
install -d -m 0755 %{buildroot}%{_sysconfdir}/%{name}
install -d -m 0755 %{buildroot}%{_sharedstatedir}/%{name}

%check
cargo test --release --offline

%post
%systemd_post %{name}.service

%preun
%systemd_preun %{name}.service

%postun
%systemd_postun_with_restart %{name}.service

%pre selinux
%selinux_relabel_pre -s %{selinuxtype}

%preun selinux
if [ $1 -eq 0 ]; then
    # Before the module goes: a service left running would be in a domain the policy
    # no longer defines, where every access is denied and not even systemd can
    # signal it.
    systemctl stop %{name}.service >/dev/null 2>&1 || :
fi

%post selinux
%selinux_modules_install -s %{selinuxtype} %{_datadir}/selinux/packages/%{selinuxtype}/%{name}.pp.bz2

%postun selinux
if [ $1 -eq 0 ]; then
    %selinux_modules_uninstall -s %{selinuxtype} %{name}
fi

%posttrans selinux
%selinux_relabel_post -s %{selinuxtype}

%files
%license LICENSE
%doc README.md docs/ARCHITECTURE.md docs/DBUS.md packaging/polkit/nspawn-wheel.rules
%{_bindir}/%{name}
%{_unitdir}/%{name}.service
%{_datadir}/dbus-1/system-services/org.nspawn.service
%{_datadir}/dbus-1/system.d/org.nspawn.conf
%{_datadir}/polkit-1/actions/org.nspawn.policy
%{_datadir}/bash-completion/completions/%{name}
%{_datadir}/zsh/site-functions/_%{name}
%{_datadir}/fish/vendor_completions.d/%{name}.fish
%{_mandir}/man1/%{name}.1*
%dir %{_sysconfdir}/%{name}
%dir %{_sharedstatedir}/%{name}

%files selinux
%{_datadir}/selinux/packages/%{selinuxtype}/%{name}.pp.bz2
%{_datadir}/selinux/devel/include/contrib/%{name}.if

%changelog
* Tue Sep 22 2026 Eduard Tolosa <tolosaeduard@gmail.com> - 1.0.0-0.1.beta1
- First package: the org.nspawn service, its command line and the SELinux policy.
