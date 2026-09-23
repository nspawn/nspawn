# SELinux policy for the nspawn service

`nspawn.te`, `nspawn.fc` and `nspawn.if` define the `nspawn_t` domain the
service runs in on hosts with SELinux (Fedora, RHEL), the way machined and
the container runtimes have domains of their own. The `nspawn-selinux`
package ships the compiled module and loads it; to build it by hand:

```
dnf install selinux-policy-devel
make -f /usr/share/selinux/devel/Makefile nspawn.pp
semodule -i nspawn.pp
restorecon -Rv /usr/bin/nspawn /var/lib/nspawn /etc/nspawn
```

A binary outside `/usr/bin` needs its label given by hand:
`semanage fcontext -a -t nspawn_exec_t /usr/local/bin/nspawn` and
`restorecon` on it.

Without the domain the service runs as `unconfined_service_t`, whose pipes and
pseudo terminals the bus may not relay (a dontaudit rule of the base policy),
so `exec`, `shell` and `logs` fail there while everything else works.

What the domain covers: the store and configuration (`nspawn_var_lib_t`,
`nspawn_etc_t`), the machines' trees and units, the bus (its own name, the
descriptors it hands over, systemd, machined, firewalld and polkit), registries
over TLS, the bridge through ip, nft and iptables in their domains, the
journal through journalctl, and the machines' namespaces. A command run inside
a machine takes the machine's own context (`unconfined_service_t` on Fedora,
what systemd-nspawn@.service runs as), entered through the files of the
machine's tree; mkosi runs unconfined, as it does from a shell. It was written
against the end-to-end suite with the domain permissive and its denials
collected, then enforced; a change in what the service does is a change here.
