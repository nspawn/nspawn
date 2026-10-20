//! The short syntax of a service volume, `[SOURCE:]TARGET[:OPTIONS]`, read as
//! compose-go's format.ParseVolume reads it.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bind {
    pub propagation: String,
    pub selinux: String,
    pub create_host_path: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    /// "bind" or "volume".
    pub kind: &'static str,
    pub source: String,
    pub target: String,
    pub read_only: bool,
    pub bind: Option<Bind>,
    /// Some for a volume, with its nocopy.
    pub nocopy: Option<bool>,
}

const PROPAGATIONS: &[&str] = &[
    "rprivate", "private", "rshared", "shared", "rslave", "slave",
];

pub fn parse(spec: &str) -> Result<Spec, String> {
    let mut volume = Spec::default();
    match spec.len() {
        0 => return Err("invalid empty volume spec".to_string()),
        1 | 2 => {
            volume.target = spec.to_string();
            volume.kind = "volume";
            return Ok(volume);
        }
        _ => {}
    }
    let bytes = spec.as_bytes();
    let mut buffer = String::new();
    let mut substitution = 0usize;
    // The spec and an end marker, each character with its byte offset.
    let chars = spec.char_indices().map(|(i, c)| (i, Some(c)));
    for (i, c) in chars.chain([(spec.len(), None)]) {
        if c == Some('$') && bytes.get(i + 1) == Some(&b'{') {
            substitution += 1;
            buffer.push('$');
            continue;
        }
        if c == Some('}') && substitution > 0 {
            substitution -= 1;
            buffer.push('}');
            continue;
        }
        let windows_drive = c == Some(':')
            && buffer.chars().count() == 1
            && buffer.chars().next().is_some_and(super::gostr::is_letter);
        // compose-go marks the end with a NUL, and so takes one inside the spec for it.
        let end = c.is_none() || c == Some('\0');
        if windows_drive {
            buffer.push(':');
        } else if (c == Some(':') || end) && substitution == 0 {
            if let Err(e) = field(end, &buffer, &mut volume) {
                populate_kind(&mut volume);
                return Err(format!("invalid spec: {spec}: {e}"));
            }
            buffer.clear();
        } else if let Some(c) = c {
            buffer.push(c);
        }
    }
    populate_kind(&mut volume);
    Ok(volume)
}

fn field(end: bool, buffer: &str, volume: &mut Spec) -> Result<(), &'static str> {
    if buffer.is_empty() {
        return Err("empty section between colons");
    }
    if volume.source.is_empty() && end {
        volume.target = buffer.to_string();
        return Ok(());
    }
    if volume.source.is_empty() {
        volume.source = buffer.to_string();
        return Ok(());
    }
    if volume.target.is_empty() {
        volume.target = buffer.to_string();
        return Ok(());
    }
    if !end {
        return Err("too many colons");
    }
    for option in buffer.split(',') {
        match option {
            "ro" => volume.read_only = true,
            "rw" => volume.read_only = false,
            "nocopy" => volume.nocopy = Some(true),
            option if PROPAGATIONS.contains(&option) => {
                volume.bind.get_or_insert_with(Bind::default).propagation = option.to_string();
            }
            "z" | "Z" => {
                volume.bind.get_or_insert_with(Bind::default).selinux = option.to_string();
            }
            // Unknown options are ignored, as compose-go ignores them.
            _ => {}
        }
    }
    Ok(())
}

fn populate_kind(volume: &mut Spec) {
    if is_file_path(&volume.source) {
        volume.kind = "bind";
        // The short syntax creates a missing host path.
        volume
            .bind
            .get_or_insert_with(Bind::default)
            .create_host_path = true;
    } else {
        volume.kind = "volume";
        volume.nocopy.get_or_insert(false);
    }
}

fn is_file_path(source: &str) -> bool {
    match source.as_bytes().first() {
        None => false,
        Some(b'.' | b'/' | b'~') => true,
        _ if source.starts_with("\\\\") => true,
        _ => {
            // The first rune, then the byte after it.
            let first = source.chars().next().unwrap();
            let bytes = super::gostr::to_bytes(source);
            let next = super::gostr::to_bytes(&first.to_string()).len();
            bytes.get(next) == Some(&b':') && super::gostr::is_letter(first)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_volumes_read_as_compose_go_reads_them() {
        let bind = parse("./data:/data:ro,z").unwrap();
        assert_eq!(bind.kind, "bind");
        assert_eq!(
            (bind.source.as_str(), bind.target.as_str()),
            ("./data", "/data")
        );
        assert!(bind.read_only);
        let b = bind.bind.unwrap();
        assert!(b.create_host_path);
        assert_eq!(b.selinux, "z");
        let named = parse("db:/var/lib/db:nocopy").unwrap();
        assert_eq!(named.kind, "volume");
        assert_eq!(named.nocopy, Some(true));
        let anonymous = parse("/data").unwrap();
        assert_eq!(
            (anonymous.kind, anonymous.target.as_str()),
            ("volume", "/data")
        );
        assert_eq!(parse("ab").unwrap().target, "ab");
        let windows = parse("c:\\data:/data").unwrap();
        assert_eq!(windows.source, "c:\\data");
        let substituted = parse("${A:-x}:/data").unwrap();
        assert_eq!(substituted.source, "${A:-x}");
    }

    #[test]
    fn broken_specs_are_refused_with_compose_go_s_words() {
        assert_eq!(parse("").unwrap_err(), "invalid empty volume spec");
        assert_eq!(
            parse("/a:/b:ro:x").unwrap_err(),
            "invalid spec: /a:/b:ro:x: too many colons"
        );
        assert_eq!(
            parse("/a::/b").unwrap_err(),
            "invalid spec: /a::/b: empty section between colons"
        );
        // A letter and a colon read as a Windows drive.
        let drive = parse("a:b:c:d").unwrap();
        assert_eq!(
            (drive.source.as_str(), drive.target.as_str()),
            ("a:b", "c:d")
        );
    }
}
