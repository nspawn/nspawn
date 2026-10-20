//! The RUN flags --mount, --network, --security and --device
//! (commands_runmount.go, commands_runnetwork.go, commands_runsecurity.go,
//! commands_rundevice.go).

use std::fmt;

use super::compat::{
    csv_fields, parse_bool, parse_uint, quote, ram_in_bytes, to_lower, with_suggestion,
};
use super::instructions::SingleWordExpander;
use super::Error;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MountType {
    #[default]
    Bind,
    Cache,
    Tmpfs,
    Secret,
    Ssh,
}

impl MountType {
    const ALL: [MountType; 5] = [
        MountType::Bind,
        MountType::Cache,
        MountType::Tmpfs,
        MountType::Secret,
        MountType::Ssh,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MountType::Bind => "bind",
            MountType::Cache => "cache",
            MountType::Tmpfs => "tmpfs",
            MountType::Secret => "secret",
            MountType::Ssh => "ssh",
        }
    }
}

impl fmt::Display for MountType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sharing mode of a cache mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShareMode {
    Shared,
    Private,
    Locked,
}

impl ShareMode {
    const ALL: [ShareMode; 3] = [ShareMode::Shared, ShareMode::Private, ShareMode::Locked];

    pub fn as_str(self) -> &'static str {
        match self {
            ShareMode::Shared => "shared",
            ShareMode::Private => "private",
            ShareMode::Locked => "locked",
        }
    }
}

impl fmt::Display for ShareMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `--mount` of RUN.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mount {
    pub mount_type: MountType,
    pub from: String,
    pub source: String,
    pub target: String,
    pub read_only: bool,
    pub size_limit: i64,
    pub cache_id: String,
    pub cache_sharing: Option<ShareMode>,
    pub required: bool,
    /// The environment variable a secret is exposed as; `Some("")` asks
    /// for the default name.
    pub env: Option<String>,
    pub mode: Option<u64>,
    pub uid: Option<u64>,
    pub gid: Option<u64>,
}

const MOUNT_KEYS: [&str; 19] = [
    "type",
    "from",
    "source",
    "target",
    "readonly",
    "id",
    "sharing",
    "required",
    "size",
    "mode",
    "uid",
    "gid",
    "src",
    "dst",
    "destination",
    "ro",
    "rw",
    "readwrite",
    "env",
];

/// Parses the CSV value of a `--mount` flag.
///
/// Without an expander only the CSV syntax and the `from` key are checked
/// and every other key is left for later, as the reference does at parse
/// time; with one, every value is expanded first and all keys are applied.
pub(crate) fn parse_mount(
    val: &str,
    mut expander: Option<&mut SingleWordExpander<'_>>,
) -> Result<Mount, Error> {
    let fields =
        csv_fields(val).map_err(|e| Error::new(format!("failed to parse csv mounts: {e}")))?;

    let mut m = Mount::default();
    let mut ro_auto = true;
    let unexpected_for_type = |key: &str, t: MountType| {
        Error::new(format!("unexpected key '{key}' for mount type '{t}'"))
    };
    let invalid_value =
        |key: &str, value: &str| Error::new(format!("invalid value for {key}: {value}"));

    for field in &fields {
        let (key, value, has_value) = match field.split_once('=') {
            Some((k, v)) => (k, v.to_string(), true),
            None => (field.as_str(), String::new(), false),
        };
        let key = to_lower(key);

        if !has_value {
            if expander.is_none() {
                continue;
            }
            match key.as_str() {
                "readonly" | "ro" => {
                    m.read_only = true;
                    ro_auto = false;
                    continue;
                }
                "readwrite" | "rw" => {
                    m.read_only = false;
                    ro_auto = false;
                    continue;
                }
                "required" => {
                    if matches!(m.mount_type, MountType::Secret | MountType::Ssh) {
                        m.required = true;
                        continue;
                    }
                    return Err(unexpected_for_type(&key, m.mount_type));
                }
                _ => {
                    return Err(Error::new(format!(
                        "invalid field '{field}' must be a key=value pair"
                    )))
                }
            }
        }

        let value = if let Some(expand) = expander.as_mut() {
            expand(&value)?
        } else if key == "from" {
            if let Some(idx) = value.find('$') {
                if idx != value.len() - 1 {
                    return Err(Error::new(format!(
                        "'{key}' doesn't support variable expansion, define alias stage instead"
                    )));
                }
            }
            value
        } else {
            continue;
        };

        match key.as_str() {
            "type" => {
                let v = to_lower(&value);
                let Some(t) = MountType::ALL.into_iter().find(|t| t.as_str() == v) else {
                    let names = MountType::ALL.map(MountType::as_str);
                    return Err(Error::new(with_suggestion(
                        format!("unsupported mount type {}", quote(&value)),
                        &value,
                        &names,
                        true,
                    )));
                };
                m.mount_type = t;
            }
            "from" => m.from = value,
            "source" | "src" => m.source = value,
            "target" | "dst" | "destination" => m.target = value,
            "readonly" | "ro" => {
                m.read_only = parse_bool(&value).ok_or_else(|| invalid_value(&key, &value))?;
                ro_auto = false;
            }
            "readwrite" | "rw" => {
                let rw = parse_bool(&value).ok_or_else(|| invalid_value(&key, &value))?;
                m.read_only = !rw;
                ro_auto = false;
            }
            "required" => {
                if !matches!(m.mount_type, MountType::Secret | MountType::Ssh) {
                    return Err(unexpected_for_type(&key, m.mount_type));
                }
                m.required = parse_bool(&value).ok_or_else(|| invalid_value(&key, &value))?;
            }
            "size" => {
                if m.mount_type != MountType::Tmpfs {
                    return Err(unexpected_for_type(&key, m.mount_type));
                }
                m.size_limit = ram_in_bytes(&value).map_err(|_| invalid_value(&key, &value))?;
            }
            "id" => m.cache_id = value,
            "sharing" => {
                let v = to_lower(&value);
                let Some(s) = ShareMode::ALL.into_iter().find(|s| s.as_str() == v) else {
                    let names = ShareMode::ALL.map(ShareMode::as_str);
                    return Err(Error::new(with_suggestion(
                        format!("unsupported sharing value {}", quote(&value)),
                        &value,
                        &names,
                        true,
                    )));
                };
                m.cache_sharing = Some(s);
            }
            "mode" => {
                let mode = parse_uint(&value, 8, 32)
                    .map_err(|_| Error::new(format!("invalid value {value} for mode")))?;
                m.mode = Some(mode);
            }
            "uid" => {
                let uid = parse_uint(&value, 10, 32)
                    .map_err(|_| Error::new(format!("invalid value {value} for uid")))?;
                m.uid = Some(uid);
            }
            "gid" => {
                let gid = parse_uint(&value, 10, 32)
                    .map_err(|_| Error::new(format!("invalid value {value} for gid")))?;
                m.gid = Some(gid);
            }
            "env" => m.env = Some(value),
            _ => {
                return Err(Error::new(with_suggestion(
                    format!("unexpected key '{key}' in '{field}'"),
                    &key,
                    &MOUNT_KEYS,
                    true,
                )))
            }
        }
    }

    let file_info_allowed = matches!(
        m.mount_type,
        MountType::Secret | MountType::Ssh | MountType::Cache
    );
    if !file_info_allowed {
        let t = quote(m.mount_type.as_str());
        if m.mode.is_some() {
            return Err(Error::new(format!("mode not allowed for {t} type mounts")));
        }
        if m.uid.is_some() {
            return Err(Error::new(format!("uid not allowed for {t} type mounts")));
        }
        if m.gid.is_some() {
            return Err(Error::new(format!("gid not allowed for {t} type mounts")));
        }
    }

    if ro_auto {
        m.read_only = !matches!(m.mount_type, MountType::Cache | MountType::Tmpfs);
    }

    if m.mount_type == MountType::Secret {
        if !m.from.is_empty() {
            return Err(Error::new("secret mount should not have a from"));
        }
        if m.cache_sharing.is_some() {
            return Err(Error::new("secret mount should not define sharing"));
        }
        if m.source.is_empty() && m.target.is_empty() && m.cache_id.is_empty() {
            return Err(Error::new(
                "invalid secret mount. one of source, target required",
            ));
        }
        if !m.source.is_empty() && !m.cache_id.is_empty() {
            return Err(Error::new("both source and id can't be set"));
        }
    }

    if m.cache_sharing.is_some() && m.mount_type != MountType::Cache {
        return Err(Error::new(format!(
            "invalid cache sharing set for {} mount",
            m.mount_type
        )));
    }

    Ok(m)
}

/// `--network` of RUN.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum NetworkMode {
    #[default]
    Default,
    None,
    Host,
}

impl NetworkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::Default => "default",
            NetworkMode::None => "none",
            NetworkMode::Host => "host",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<NetworkMode, Error> {
        match value {
            "default" => Ok(NetworkMode::Default),
            "none" => Ok(NetworkMode::None),
            "host" => Ok(NetworkMode::Host),
            _ => Err(Error::new(format!("invalid network mode {}", quote(value)))),
        }
    }
}

/// `--security` of RUN.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Security {
    Insecure,
    #[default]
    Sandbox,
}

impl Security {
    pub fn as_str(self) -> &'static str {
        match self {
            Security::Insecure => "insecure",
            Security::Sandbox => "sandbox",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Security, Error> {
        match value {
            "insecure" => Ok(Security::Insecure),
            "sandbox" => Ok(Security::Sandbox),
            _ => Err(Error::new(format!(
                "security {} is not valid",
                quote(value)
            ))),
        }
    }
}

/// One `--device` of RUN.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub required: bool,
}

/// Parses the CSV value of a `--device` flag: a device name (which may
/// contain `=`), or `name=...`, and an optional `required[=bool]`.
pub fn parse_device(val: &str) -> Result<Device, Error> {
    let fields =
        csv_fields(val).map_err(|e| Error::new(format!("failed to parse csv devices: {e}")))?;
    let mut d = Device::default();
    for field in &fields {
        let (key, value, has_value) = match field.split_once('=') {
            Some((k, v)) => (k, v, true),
            None => (field.as_str(), "", false),
        };
        let key = to_lower(key);

        if !has_value {
            if key == "required" {
                d.required = true;
                continue;
            }
            if d.name.is_empty() {
                d.name.clone_from(field);
                continue;
            }
            return Err(Error::new(format!(
                "invalid field '{field}' must be a key=value pair"
            )));
        }

        match key.as_str() {
            "name" => {
                if !d.name.is_empty() {
                    return Err(Error::new(format!("device name already set to {}", d.name)));
                }
                d.name = value.to_string();
            }
            "required" => {
                d.required = parse_bool(value)
                    .ok_or_else(|| Error::new(format!("invalid value for {key}: {value}")))?;
            }
            _ => {
                if d.name.is_empty() {
                    d.name.clone_from(field);
                    continue;
                }
                return Err(Error::new(with_suggestion(
                    format!("unexpected key '{key}' in '{field}'"),
                    &key,
                    &["name", "required"],
                    true,
                )));
            }
        }
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded(val: &str) -> Result<Mount, Error> {
        let mut identity = |w: &str| -> Result<String, Error> { Ok(w.to_string()) };
        parse_mount(val, Some(&mut identity))
    }

    #[test]
    fn parse_device_cases() {
        let ok = |input: &str, name: &str, required: bool| {
            assert_eq!(
                parse_device(input).unwrap(),
                Device {
                    name: name.to_string(),
                    required
                },
                "{input}"
            );
        };
        ok("vendor1.com/device=foo", "vendor1.com/device=foo", false);
        ok("vendor1.com/device", "vendor1.com/device", false);
        ok(
            "vendor1.com/device=foo,required",
            "vendor1.com/device=foo",
            true,
        );
        ok(
            "vendor1.com/device=foo,required=true",
            "vendor1.com/device=foo",
            true,
        );
        ok(
            "vendor1.com/device=foo,required=false",
            "vendor1.com/device=foo",
            false,
        );
        ok(
            "name=vendor1.com/device=foo",
            "vendor1.com/device=foo",
            false,
        );
        ok(
            "name=vendor1.com/device=foo,required",
            "vendor1.com/device=foo",
            true,
        );
        assert_eq!(
            parse_device("vendor1.com/device=foo,name=vendor2.com/device=bar")
                .unwrap_err()
                .to_string(),
            "device name already set to vendor1.com/device=foo"
        );
        assert_eq!(
            parse_device("a,b").unwrap_err().to_string(),
            "invalid field 'b' must be a key=value pair"
        );
        assert_eq!(
            parse_device("a,nme=b").unwrap_err().to_string(),
            "unexpected key 'nme' in 'nme=b' (did you mean name?)"
        );
        assert_eq!(
            parse_device("a,required=maybe").unwrap_err().to_string(),
            "invalid value for required: maybe"
        );
        assert_eq!(
            parse_device("").unwrap_err().to_string(),
            "failed to parse csv devices: EOF"
        );
    }

    #[test]
    fn mount_defaults() {
        let m = expanded("target=/x").unwrap();
        assert_eq!(m.mount_type, MountType::Bind);
        assert!(m.read_only);
        let m = expanded("type=cache,target=/x").unwrap();
        assert!(!m.read_only);
        let m = expanded("type=tmpfs,target=/x,size=64m").unwrap();
        assert!(!m.read_only);
        assert_eq!(m.size_limit, 64 << 20);
        let m = expanded("type=secret,id=foo,required,env=TOKEN,mode=0400,uid=1,gid=2").unwrap();
        assert!(m.read_only && m.required);
        assert_eq!(m.env.as_deref(), Some("TOKEN"));
        assert_eq!((m.mode, m.uid, m.gid), (Some(0o400), Some(1), Some(2)));
        let m = expanded("type=bind,from=base,src=/a,dst=/b,rw").unwrap();
        assert_eq!(
            (m.from.as_str(), m.source.as_str(), m.target.as_str()),
            ("base", "/a", "/b")
        );
        assert!(!m.read_only);
        let m = expanded("type=cache,id=c,sharing=LOCKED,readonly=true").unwrap();
        assert_eq!(m.cache_sharing, Some(ShareMode::Locked));
        assert!(m.read_only);
        let m = expanded("TYPE=SSH,REQUIRED=1").unwrap();
        assert_eq!(m.mount_type, MountType::Ssh);
        assert!(m.required);
    }

    #[test]
    fn mount_errors() {
        let err = |val: &str| expanded(val).unwrap_err().to_string();
        assert_eq!(
            err("type=bnd"),
            "unsupported mount type \"bnd\" (did you mean bind?)"
        );
        assert_eq!(err("type=nfs,target=/x"), "unsupported mount type \"nfs\"");
        assert_eq!(
            err("target=/x,zzz=bar"),
            "unexpected key 'zzz' in 'zzz=bar'"
        );
        assert_eq!(
            err("target=/x,foo=bar"),
            "unexpected key 'foo' in 'foo=bar' (did you mean from?)"
        );
        assert_eq!(
            err("target=/x,trget=/y"),
            "unexpected key 'trget' in 'trget=/y' (did you mean target?)"
        );
        assert_eq!(err("target=/x,ro=maybe"), "invalid value for ro: maybe");
        assert_eq!(err("target=/x,rw=maybe"), "invalid value for rw: maybe");
        assert_eq!(
            err("target=/x,required"),
            "unexpected key 'required' for mount type 'bind'"
        );
        assert_eq!(
            err("target=/x,required=true"),
            "unexpected key 'required' for mount type 'bind'"
        );
        assert_eq!(
            err("target=/x,size=1m"),
            "unexpected key 'size' for mount type 'bind'"
        );
        assert_eq!(err("type=tmpfs,size=lots"), "invalid value for size: lots");
        assert_eq!(
            err("target=/x,bogus"),
            "invalid field 'bogus' must be a key=value pair"
        );
        assert_eq!(
            err("type=cache,sharing=shred"),
            "unsupported sharing value \"shred\" (did you mean shared?)"
        );
        assert_eq!(err("type=cache,mode=9"), "invalid value 9 for mode");
        assert_eq!(err("type=cache,uid=x"), "invalid value x for uid");
        assert_eq!(err("type=cache,gid=-1"), "invalid value -1 for gid");
        assert_eq!(
            err("target=/x,mode=600"),
            "mode not allowed for \"bind\" type mounts"
        );
        assert_eq!(
            err("type=tmpfs,uid=0"),
            "uid not allowed for \"tmpfs\" type mounts"
        );
        assert_eq!(
            err("type=tmpfs,gid=0"),
            "gid not allowed for \"tmpfs\" type mounts"
        );
        assert_eq!(
            err("type=secret,id=a,from=b"),
            "secret mount should not have a from"
        );
        assert_eq!(
            err("type=secret,id=a,sharing=shared"),
            "secret mount should not define sharing"
        );
        assert_eq!(
            err("type=secret"),
            "invalid secret mount. one of source, target required"
        );
        assert_eq!(
            err("type=secret,id=a,source=b"),
            "both source and id can't be set"
        );
        assert_eq!(
            err("target=/x,sharing=private"),
            "invalid cache sharing set for bind mount"
        );
        assert_eq!(
            err("target=/x,\"a"),
            "failed to parse csv mounts: parse error on line 1, column 12: extraneous or missing \" in quoted-field"
        );
    }

    #[test]
    fn mount_without_expander() {
        let m = parse_mount("type=cache,target=/x,bogus,mode=1", None).unwrap();
        assert_eq!(
            m,
            Mount {
                read_only: true,
                ..Mount::default()
            }
        );
        assert_eq!(
            parse_mount("type=bind,from=$STAGE", None)
                .unwrap_err()
                .to_string(),
            "'from' doesn't support variable expansion, define alias stage instead"
        );
        assert_eq!(parse_mount("from=stage$", None).unwrap().from, "stage$");
        assert_eq!(
            parse_mount("", None).unwrap_err().to_string(),
            "failed to parse csv mounts: EOF"
        );
    }

    #[test]
    fn mount_values_are_expanded() {
        let mut expand = |w: &str| -> Result<String, Error> { Ok(w.replace("$T", "/target")) };
        let m = parse_mount("target=$T,from=$T", Some(&mut expand)).unwrap();
        assert_eq!(m.target, "/target");
        assert_eq!(m.from, "/target");
        let mut fail = |_: &str| -> Result<String, Error> { Err(Error::new("boom")) };
        assert_eq!(
            parse_mount("target=x", Some(&mut fail))
                .unwrap_err()
                .to_string(),
            "boom"
        );
    }

    #[test]
    fn network_and_security() {
        assert_eq!(NetworkMode::parse("host").unwrap(), NetworkMode::Host);
        assert_eq!(
            NetworkMode::parse("bridge").unwrap_err().to_string(),
            "invalid network mode \"bridge\""
        );
        assert_eq!(Security::parse("insecure").unwrap(), Security::Insecure);
        assert_eq!(
            Security::parse("open").unwrap_err().to_string(),
            "security \"open\" is not valid"
        );
    }
}
