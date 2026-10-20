//! `--mount`, the long form of `-v` and `--tmpfs`: comma-separated KEY=VALUE fields,
//! read as the reference's command line reads them, and given to the machine as the
//! volume or tmpfs they describe.

use anyhow::{bail, Context as _, Result};

/// What one `--mount` becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// SOURCE:TARGET[:ro], as -v takes it.
    Volume(String),
    /// PATH[:OPTIONS], as --tmpfs takes it.
    Tmpfs(String),
}

/// The fields of a CSV record, as Go's encoding/csv reads one line of it: a field in
/// double quotes may hold commas, and two quotes stand for one.
pub(crate) fn csv_fields(line: &str) -> Result<Vec<String>> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        let mut field = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    Some('"') => break,
                    Some(c) => field.push(c),
                    None => bail!("parse error on line 1, column {}: extraneous or missing \" in quoted-field", line.len() + 1),
                }
            }
            match chars.next() {
                None => {
                    fields.push(field);
                    return Ok(fields);
                }
                Some(',') => {}
                Some(_) => bail!("parse error on line 1: extraneous or missing \" in quoted-field"),
            }
        } else {
            loop {
                match chars.next() {
                    None => {
                        fields.push(field);
                        return Ok(fields);
                    }
                    Some(',') => break,
                    Some('"') => bail!("parse error on line 1: bare \" in non-quoted-field"),
                    Some(c) => field.push(c),
                }
            }
        }
        fields.push(field);
    }
}

fn bool_value(key: &str, value: &str, given: bool) -> Result<bool> {
    if !given {
        return Ok(true);
    }
    match value {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => bail!(
            "invalid value for '{key}': invalid boolean value ({value:?}): must be one of \"true\", \"1\", \"false\", or \"0\" (default \"true\")"
        ),
    }
}

/// One `--mount` value. A bind's relative source starting with `.` is taken from the
/// current directory of the command line.
pub fn parse(text: &str) -> Result<Mount> {
    let text = text.trim();
    if text.is_empty() {
        bail!("value is empty");
    }
    let mut kind = "volume".to_string();
    let (mut source, mut target) = (String::new(), String::new());
    let mut read_only = false;
    let mut create_source = false;
    let (mut bind_options, mut volume_options, mut tmpfs_options) = (false, false, false);
    let (mut size, mut mode) = (None::<u64>, None::<u32>);
    for field in csv_fields(text)? {
        let (key, value, given) = match field.split_once('=') {
            Some((k, v)) => (k, v, true),
            None => (field.as_str(), "", false),
        };
        if key.trim() != key {
            bail!(
                "invalid option '{}' in '{field}': option should not have whitespace",
                key.trim()
            );
        }
        if given {
            if value.trim().is_empty() {
                bail!("invalid value for '{key}': value is empty");
            }
            if value.trim() != value {
                bail!("invalid value for '{key}' in '{field}': value should not have whitespace");
            }
        }
        let key = key.to_lowercase();
        if !given
            && !matches!(
                key.as_str(),
                "readonly" | "ro" | "volume-nocopy" | "bind-nonrecursive" | "bind-create-src"
            )
        {
            bail!("invalid field '{field}' must be a key=value pair");
        }
        match key.as_str() {
            "type" => kind = value.to_lowercase(),
            "source" | "src" => {
                source = value.to_string();
                if value.starts_with('.') {
                    // filepath.Abs: joined and cleaned, ../ taken against the directory.
                    if let Ok(cwd) = std::env::current_dir() {
                        source = crate::api::copy::clean(&cwd.join(value), false)
                            .to_string_lossy()
                            .into_owned();
                    }
                }
            }
            "target" | "dst" | "destination" => target = value.to_string(),
            "readonly" | "ro" => read_only = bool_value(&key, value, given)?,
            // A setting of file sharing with a VM; nothing here.
            "consistency" => {}
            // A bind into a machine propagates nothing either way; a mode docker's daemon
            // refuses is refused all the same.
            "bind-propagation" => {
                bind_options = true;
                let mode = value.to_lowercase();
                if !matches!(
                    mode.as_str(),
                    "private" | "rprivate" | "shared" | "rshared" | "slave" | "rslave"
                ) {
                    bail!(
                        "invalid mount config for type \"bind\": invalid propagation mode: {mode}"
                    );
                }
            }
            "bind-nonrecursive" => {
                bail!("bind-nonrecursive is deprecated, use bind-recursive=disabled instead")
            }
            "bind-recursive" => {
                // Enabled is no bind option of its own, as docker's CLI has it.
                bind_options |= value != "enabled";
                match value {
                    "enabled" => {}
                    "disabled" | "writable" | "readonly" => bail!(
                        "bind-recursive={value} is not supported: a bind into an nspawn machine is recursive"
                    ),
                    _ => bail!(
                        "invalid value for {key}: {value} (must be \"enabled\", \"disabled\", \"writable\", or \"readonly\")"
                    ),
                }
            }
            // Made once every field is known good, whatever their order.
            "bind-create-src" => {
                bind_options = true;
                create_source = bool_value(&key, value, given)?;
            }
            "volume-subpath" => bail!("volume-subpath is not supported: a volume is mounted whole"),
            // A new volume is filled from the image all the same; nspawn volumes carry no
            // labels.
            "volume-nocopy" => {
                volume_options = true;
                bool_value(&key, value, given)?;
            }
            "volume-label" => volume_options = true,
            "volume-driver" => {
                volume_options = true;
                if value != "local" {
                    bail!("volume driver {value} is not supported: nspawn volumes are directories of the host");
                }
            }
            "volume-opt" => {
                bail!("volume-opt is not supported: nspawn volumes are directories of the host")
            }
            "image-subpath" => bail!("an image is not mounted as a volume"),
            // 0 is no size and no mode, as the engine takes them: the tmpfs default.
            "tmpfs-size" => {
                tmpfs_options = true;
                size = Some(
                    crate::policy::parse_size(value)
                        .map_err(|_| anyhow::anyhow!("invalid value for {key}: {value}"))?,
                )
                .filter(|s| *s > 0);
            }
            "tmpfs-mode" => {
                tmpfs_options = true;
                mode = Some(
                    u32::from_str_radix(value, 8)
                        .map_err(|_| anyhow::anyhow!("invalid value for {key}: {value}"))?,
                )
                .filter(|m| *m > 0);
            }
            _ => bail!("unknown option '{key}' in '{field}'"),
        }
    }
    for (used, prefix, of) in [
        (bind_options, "bind", "bind"),
        (volume_options, "volume", "volume"),
        (tmpfs_options, "tmpfs", "tmpfs"),
    ] {
        if used && kind != of {
            bail!("cannot mix '{prefix}-*' options with mount type '{kind}'");
        }
    }
    if target.is_empty() {
        bail!("invalid mount config for type \"{kind}\": field Target must not be empty");
    }
    // The mount travels on as -v and --tmpfs write it, where a colon separates.
    if let Some(path) = [&source, &target].into_iter().find(|p| p.contains(':')) {
        bail!("a mount path with ':' is not supported: {path}");
    }
    let ro = if read_only { ":ro" } else { "" };
    match kind.as_str() {
        "bind" => {
            if source.is_empty() {
                bail!("invalid mount config for type \"bind\": field Source must not be empty");
            }
            if !source.starts_with('/') {
                bail!("invalid mount config for type \"bind\": invalid mount path: '{source}' mount path must be absolute");
            }
            let spec = format!("{source}:{target}{ro}");
            // Made only for a mount the service will take.
            spec.parse::<crate::volume::Volume>()?;
            if create_source {
                std::fs::create_dir_all(&source)
                    .with_context(|| format!("creating {source} (bind-create-src)"))?;
            }
            Ok(Mount::Volume(spec))
        }
        "volume" => {
            if source.is_empty() {
                bail!("a volume without a source is an anonymous one, which nspawn does not have: name it (source=NAME), or what is written there goes with the machine");
            }
            if source.starts_with('/') {
                bail!("invalid mount config for type \"volume\": invalid volume name: '{source}' is a path; type=bind mounts one");
            }
            Ok(Mount::Volume(format!("{source}:{target}{ro}")))
        }
        "tmpfs" => {
            if !source.is_empty() {
                bail!(
                    "invalid mount config for type \"tmpfs\": field Source must not be specified"
                );
            }
            let mut options = Vec::new();
            if let Some(size) = size {
                options.push(format!("size={size}"));
            }
            if let Some(mode) = mode {
                options.push(format!("mode={mode:o}"));
            }
            if read_only {
                options.push("ro".to_string());
            }
            Ok(Mount::Tmpfs(if options.is_empty() {
                target
            } else {
                format!("{target}:{}", options.join(","))
            }))
        }
        "image" => bail!("mount type image is not supported: an image is not mounted as a volume"),
        "npipe" => {
            bail!("mount type npipe is not supported: named pipes are for Windows containers")
        }
        "cluster" => bail!("mount type cluster is not supported: cluster volumes are Swarm's"),
        other => bail!("invalid mount config for type \"{other}\": mount type unknown"),
    }
}

/// Every `--mount`, split into what `-v` and `--tmpfs` take.
pub fn split(values: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let (mut volumes, mut tmpfs) = (Vec::new(), Vec::new());
    for value in values {
        match parse(value)? {
            Mount::Volume(v) => volumes.push(v),
            Mount::Tmpfs(t) => tmpfs.push(t),
        }
    }
    Ok((volumes, tmpfs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Mount {
        parse(text).unwrap_or_else(|e| panic!("{text}: {e:#}"))
    }

    fn err(text: &str) -> String {
        format!("{:#}", parse(text).unwrap_err())
    }

    #[test]
    fn the_forms_docker_takes() {
        assert_eq!(
            ok("type=bind,source=/srv,target=/data"),
            Mount::Volume("/srv:/data".into())
        );
        assert_eq!(
            ok("type=bind,src=/srv,dst=/data,readonly"),
            Mount::Volume("/srv:/data:ro".into())
        );
        assert_eq!(
            ok("type=bind,src=/srv,destination=/data,ro=true"),
            Mount::Volume("/srv:/data:ro".into())
        );
        assert_eq!(
            ok("type=bind,src=/srv,target=/data,ro=0"),
            Mount::Volume("/srv:/data".into())
        );
        assert_eq!(
            ok("source=data,target=/var/lib/db"),
            Mount::Volume("data:/var/lib/db".into())
        );
        assert_eq!(
            ok("type=volume,src=data,dst=/d,volume-nocopy"),
            Mount::Volume("data:/d".into())
        );
        assert_eq!(
            ok("TYPE=BIND,src=/srv,target=/data,bind-propagation=rslave"),
            Mount::Volume("/srv:/data".into())
        );
        assert_eq!(
            ok("type=tmpfs,target=/cache"),
            Mount::Tmpfs("/cache".into())
        );
        assert_eq!(
            ok("type=tmpfs,dst=/cache,tmpfs-size=64m,tmpfs-mode=1770"),
            Mount::Tmpfs("/cache:size=67108864,mode=1770".into())
        );
        assert_eq!(
            ok("\"type=bind\",\"source=/a,b\",target=/c"),
            Mount::Volume("/a,b:/c".into())
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            ok("type=bind,source=./here,target=/c"),
            Mount::Volume(format!("{}/here:/c", cwd.display()))
        );
        // ../ is taken against the directory, as filepath.Abs cleans it.
        assert_eq!(
            ok("type=bind,source=../shared,target=/s"),
            Mount::Volume(format!("{}/shared:/s", cwd.parent().unwrap().display()))
        );
    }

    #[test]
    fn what_docker_refuses_and_what_nspawn_cannot_do() {
        assert_eq!(err(""), "value is empty");
        assert_eq!(
            err("type=bind,target"),
            "invalid field 'target' must be a key=value pair"
        );
        assert_eq!(
            err("type=bind,target= /x"),
            "invalid value for 'target' in 'target= /x': value should not have whitespace"
        );
        assert_eq!(
            err("type=bind,src="),
            "invalid value for 'src': value is empty"
        );
        assert_eq!(
            err("type=bind,bogus=1,target=/x"),
            "unknown option 'bogus' in 'bogus=1'"
        );
        assert_eq!(err("type=bind,ro=yes,target=/x"), "invalid value for 'ro': invalid boolean value (\"yes\"): must be one of \"true\", \"1\", \"false\", or \"0\" (default \"true\")");
        assert_eq!(
            err("type=volume,src=a,target=/x,tmpfs-size=1m"),
            "cannot mix 'tmpfs-*' options with mount type 'volume'"
        );
        assert_eq!(
            err("type=tmpfs,target=/x,bind-propagation=rprivate"),
            "cannot mix 'bind-*' options with mount type 'tmpfs'"
        );
        assert_eq!(
            err("type=bind,src=/a"),
            "invalid mount config for type \"bind\": field Target must not be empty"
        );
        assert_eq!(
            err("type=bind,target=/a"),
            "invalid mount config for type \"bind\": field Source must not be empty"
        );
        assert!(err("target=/a").contains("anonymous"));
        assert!(err("type=volume,src=a,target=/x,volume-subpath=sub").contains("mounted whole"));
        assert!(err("type=image,src=a,target=/x").contains("not supported"));
        assert!(err("type=bind,src=/a,target=/x,bind-recursive=disabled").contains("recursive"));
        assert_eq!(
            err("type=tmpfs,target=/x,tmpfs-mode=99"),
            "invalid value for tmpfs-mode: 99"
        );
        assert_eq!(
            err("type=bind,src=/a,target=/x,bind-nonrecursive"),
            "bind-nonrecursive is deprecated, use bind-recursive=disabled instead"
        );
        assert!(err("type=\"bind,src=/a").contains("quoted-field"));
    }

    #[test]
    fn a_source_is_made_only_for_a_good_bind() {
        let dir = tempfile::tempdir().unwrap();
        let made = dir.path().join("made");
        let text = format!(
            "bind-create-src,type=bind,source={},target=/x",
            made.display()
        );
        parse(&text).unwrap();
        assert!(made.is_dir(), "the order of the fields does not matter");
        let refused = dir.path().join("refused");
        let text = format!(
            "type=volume,bind-create-src,source={},target=/x",
            refused.display()
        );
        assert!(parse(&text).is_err());
        assert!(!refused.exists(), "nothing is made for a refused mount");
        // Nor for one the service would refuse.
        let climbing = dir.path().join("up");
        let text = format!(
            "type=bind,bind-create-src,source={}/../x,target=/x",
            climbing.display()
        );
        assert!(parse(&text).is_err());
        assert!(!climbing.exists());
    }

    #[test]
    fn keys_read_as_docker_s_cli_reads_them() {
        // enabled is no bind option: a volume takes it.
        assert!(parse("type=volume,src=v,dst=/d,bind-recursive=enabled").is_ok());
        // The raw value, as docker's CLI matches it: no bool stands for a mode.
        assert!(err("type=volume,src=v,dst=/d,bind-recursive=true")
            .contains("invalid value for bind-recursive: true"));
        assert!(err("type=bind,src=/a,dst=/d,bind-recursive=false")
            .contains("invalid value for bind-recursive: false"));
        // A propagation mode docker's daemon does not know is refused.
        assert!(err("type=bind,src=/srv,dst=/data,bind-propagation=bogus")
            .contains("invalid propagation mode: bogus"));
        assert!(err("type=volume,src=v,dst=/d,volume-nocopy=maybe").contains("boolean"));
        // 0 is the tmpfs default, not an unbounded size or a mode nobody can use.
        assert_eq!(
            ok("type=tmpfs,dst=/cache,tmpfs-size=0,tmpfs-mode=0"),
            Mount::Tmpfs("/cache".into())
        );
    }

    #[test]
    fn a_colon_in_a_mount_path_is_refused() {
        assert_eq!(
            err("type=bind,src=/backups/2026-10-18T10:00,dst=/restore"),
            "a mount path with ':' is not supported: /backups/2026-10-18T10:00"
        );
        assert_eq!(
            err("type=bind,src=/srv/x,dst=/data:ro"),
            "a mount path with ':' is not supported: /data:ro"
        );
        assert!(err("type=tmpfs,dst=/a:b").contains("':'"));
    }

    #[test]
    fn split_by_kind() {
        let (v, t) = split(&["src=data,dst=/d".into(), "type=tmpfs,dst=/t".into()]).unwrap();
        assert_eq!(v, ["data:/d"]);
        assert_eq!(t, ["/t"]);
    }
}
