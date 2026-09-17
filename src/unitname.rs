//! systemd unit name escaping, the same rules as `systemd-escape --path`.

/// Escapes a filesystem path into a unit name prefix: "/var/lib/machines/x-y" becomes
/// "var-lib-machines-x\x2dy".
pub fn escape_path(path: &str) -> String {
    let trimmed: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    let joined = trimmed.join("/");
    let mut out = String::with_capacity(joined.len() * 2);
    for (i, b) in joined.bytes().enumerate() {
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

/// Name of the mount unit that mounts `path`.
pub fn mount_unit_for(path: &str) -> String {
    format!("{}.mount", escape_path(path))
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
    }
}
