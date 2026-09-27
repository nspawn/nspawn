//! systemd unit name escaping, the same rules as `systemd-escape --path`.

/// Escapes a string into a unit name part, as `systemd-escape` does: "/" becomes "-",
/// letters, digits, ":" and "_" stay, so does "." after the first byte, the rest is
/// "\xNN".
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for (i, b) in text.bytes().enumerate() {
        let c = b as char;
        if c == '/' {
            out.push('-');
        } else if c.is_ascii_alphanumeric() || c == ':' || c == '_' || (c == '.' && i != 0) {
            out.push(c);
        } else {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// Escapes a filesystem path into a unit name prefix: "/var/lib/machines/x-y" becomes
/// "var-lib-machines-x\x2dy".
pub fn escape_path(path: &str) -> String {
    let trimmed: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    escape(&trimmed.join("/"))
}

/// Name of the mount unit that mounts `path`.
pub fn mount_unit_for(path: &str) -> String {
    format!("{}.mount", escape_path(path))
}

/// Name of the device unit udev keeps for a network interface of the host: the escaped
/// sysfs path, where a leading dot of the name is not the first byte.
pub fn device_unit_for(interface: &str) -> String {
    format!(
        "{}.device",
        escape_path(&format!("/sys/subsystem/net/devices/{interface}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_systemd_escape() {
        assert_eq!(
            escape_path("/var/lib/machines/ovl-test"),
            "var-lib-machines-ovl\\x2dtest"
        );
        assert_eq!(
            escape_path("/var/lib/machines/fedora-44"),
            "var-lib-machines-fedora\\x2d44"
        );
        assert_eq!(escape_path("/"), "-");
        assert_eq!(escape_path("//var//lib/"), "var-lib");
        assert_eq!(escape_path("/.hidden"), "\\x2ehidden");
        assert_eq!(escape_path("/a.b_c:d"), "a.b_c:d");
        assert_eq!(escape_path("/x y"), "x\\x20y");
        assert_eq!(
            mount_unit_for("/var/lib/machines/e2e"),
            "var-lib-machines-e2e.mount"
        );
        assert_eq!(
            device_unit_for("wlp11s0f3u2u3"),
            "sys-subsystem-net-devices-wlp11s0f3u2u3.device"
        );
        assert_eq!(
            device_unit_for("e2e-dummy0"),
            "sys-subsystem-net-devices-e2e\\x2ddummy0.device"
        );
        assert_eq!(
            device_unit_for(".x"),
            "sys-subsystem-net-devices-.x.device",
            "a dot is escaped at the start of the whole path alone"
        );
        assert_eq!(escape("a.b_c:d"), "a.b_c:d");
        assert_eq!(escape(".x"), "\\x2ex");
    }
}
