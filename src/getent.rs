//! A stand-in for getent in app machines that run as a user.
//!
//! systemd-nspawn resolves the user an app runs as (`User=`) by running `getent passwd`
//! and `getent initgroups` inside the machine. busybox and static images have no
//! getent, musl's (alpine) has no `initgroups`, and neither takes a uid the image's
//! passwd does not list, which docker allows, nor the group of docker's USER:GROUP.
//! So nspawn binds a shell script over the path systemd-nspawn looks at that answers
//! those two from `/etc/passwd` and `/etc/group`, with the group asked for as the
//! primary and only one, and hands anything else to the image's own getent, bound at
//! `/run/nspawn/getent` where the image has one.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::backend::BackendChoice;
use crate::settings::Bind;
use crate::store::{ImageRecord, Store};

/// Where the image's own getent is bound, for what the stand-in does not answer.
pub const REAL: &str = "/run/nspawn/getent";

/// Answers `passwd KEY` and `initgroups KEY` by name or by uid; a uid the file does not
/// list runs all the same, with gid 0 and no home, as docker has it, and a group asked
/// for (`group=`, filled in by `script`) is the primary one and the only one. Nothing
/// but shell builtins: systemd-nspawn runs it with an empty environment.
pub const SCRIPT: &str = r#"#!/bin/sh
# Written by nspawn: systemd-nspawn resolves the user an app runs as with getent, and
# this answers passwd and initgroups from /etc/passwd and /etc/group, by name or by
# uid, as docker reads them. Anything else goes to the image's own getent.
# The gid of --user USER:GROUP; empty for the passwd entry's own group and the ones
# /etc/group adds.
group=''
case "$1:$#" in
passwd:2)
    while IFS=: read -r name pw uid gid rest; do
        if [ "$name" = "$2" ] || [ "$uid" = "$2" ]; then
            printf '%s:%s:%s:%s:%s\n' "$name" "$pw" "$uid" "${group:-$gid}" "$rest"
            exit 0
        fi
    done < /etc/passwd
    case "$2" in
    ''|*[!0-9]*) exit 2 ;;
    esac
    printf '%s:x:%s:%s::/:/bin/sh\n' "$2" "$2" "${group:-0}"
    ;;
initgroups:2)
    if [ -n "$group" ]; then
        printf '%s %s\n' "$2" "$group"
        exit 0
    fi
    name=$2
    while IFS=: read -r n pw uid gid rest; do
        if [ "$n" = "$2" ] || [ "$uid" = "$2" ]; then
            name=$n
            break
        fi
    done < /etc/passwd
    printf '%s' "$2"
    while IFS=: read -r group pw gid members; do
        case ",$members," in
        *",$name,"*) printf ' %s' "$gid" ;;
        esac
    done < /etc/group
    echo
    ;;
*)
    if [ -x /run/nspawn/getent ]; then
        exec /run/nspawn/getent "$@"
    fi
    exit 2
    ;;
esac
"#;

/// The binds of the stand-in for an app machine: none when it runs as root; the script
/// where systemd-nspawn looks first, and the image's own getent at `REAL` when the root
/// is assembled ahead of the start (an mstack tree is not). Refused when the image has
/// no /bin/sh to run a stand-in and no getent of its own either.
pub fn shim(store: &Store, name: &str, record: &ImageRecord) -> Result<Vec<Bind>> {
    let Some(user) = switches_to(record.effective_user()) else {
        return Ok(Vec::new());
    };
    let group = match group_of(record.effective_user()) {
        Some(group) => Some(resolve_group(store, name, record.backend, group)?),
        None => None,
    };
    let has = |path: &str| {
        crate::backend::root_has(store, name, record.backend, path, |m| {
            // A mount point made ahead of an earlier start is an empty file, not a
            // program.
            !m.is_file() || m.len() > 0
        })
    };
    let Some(placement) = target(name, user, has)? else {
        return Ok(Vec::new());
    };
    let dir = store.machine_files_dir(name);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut binds = vec![Bind {
        source: write(&dir, group)?,
        target: placement.target.to_string(),
        read_only: true,
    }];
    // A bind follows a symlink on the host, so only a program of the image itself is
    // handed over.
    if let (Some(real), BackendChoice::Overlay | BackendChoice::Flat | BackendChoice::Auto) =
        (placement.real, record.backend)
    {
        let source = store.machines_dir.join(name).join(real);
        if source.symlink_metadata().is_ok_and(|m| m.is_file()) {
            binds.push(Bind {
                source,
                target: REAL.to_string(),
                read_only: true,
            });
        }
    }
    Ok(binds)
}

/// The user systemd-nspawn switches to, if any: root and 0 are no switch unless a group
/// comes with them, which only the stand-in can hand over.
fn switches_to(user: Option<&str>) -> Option<&str> {
    let user = user?;
    let (user, group) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    if user.is_empty() || (group.is_none() && (user == "root" || user == "0")) {
        return None;
    }
    Some(user)
}

/// The group of docker's USER:GROUP, when given.
fn group_of(user: Option<&str>) -> Option<&str> {
    user?
        .split_once(':')
        .map(|(_, g)| g)
        .filter(|g| !g.is_empty())
}

/// The gid `group` stands for: itself when numeric, else the image's /etc/group entry
/// of that name, refused as docker refuses it when there is none.
fn resolve_group(store: &Store, name: &str, backend: BackendChoice, group: &str) -> Result<u32> {
    if let Ok(gid) = group.parse::<u32>() {
        return Ok(gid);
    }
    let text = crate::backend::root_path(store, name, backend, "etc/group")
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    gid_in(&text, group)
        .with_context(|| format!("unable to find group {group}: no matching entries in group file"))
}

/// The gid of `group` in a group file's text.
fn gid_in(text: &str, group: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == group).then(|| fields.nth(1)?.parse().ok())?
    })
}

/// The stand-in with the group filled in, when one was asked for.
pub fn script(group: Option<u32>) -> String {
    match group {
        Some(gid) => SCRIPT.replacen("group=''", &format!("group='{gid}'"), 1),
        None => SCRIPT.to_string(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Placement {
    /// Where the script goes: the first place systemd-nspawn looks that the root has.
    target: &'static str,
    /// The image's own getent, relative to the root, for what the script does not
    /// answer.
    real: Option<&'static str>,
}

/// Where the stand-in goes, from what the root has. None when the image has no /bin/sh
/// for it but a getent of its own, which is then systemd-nspawn's to run.
fn target(name: &str, user: &str, has: impl Fn(&str) -> bool) -> Result<Option<Placement>> {
    let real = ["usr/bin/getent", "bin/getent"]
        .into_iter()
        .find(|path| has(path));
    if !has("bin/sh") {
        if real.is_some() {
            return Ok(None);
        }
        bail!("{name} runs as {user}, which systemd-nspawn resolves with getent inside the machine, and the image has neither getent nor a /bin/sh to stand in for it; run it as root (-u root) or add getent to the image");
    }
    Ok(Some(Placement {
        target: if has("usr/bin") {
            "/usr/bin/getent"
        } else {
            "/bin/getent"
        },
        real,
    }))
}

fn write(dir: &Path, group: Option<u32>) -> Result<PathBuf> {
    let path = dir.join("getent");
    fs::write(&path, script(group)).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_needs_no_stand_in() {
        assert_eq!(switches_to(None), None);
        assert_eq!(switches_to(Some("")), None);
        assert_eq!(switches_to(Some("root")), None);
        assert_eq!(switches_to(Some("0")), None);
        assert_eq!(switches_to(Some("nobody")), Some("nobody"));
        assert_eq!(switches_to(Some("1000:1000")), Some("1000"));
        // A group for root goes through the stand-in too.
        assert_eq!(switches_to(Some("0:0")), Some("0"));
        assert_eq!(switches_to(Some("root:1000")), Some("root"));
        assert_eq!(group_of(Some("root:1000")), Some("1000"));
        assert_eq!(group_of(Some("nobody")), None);
        assert_eq!(group_of(Some("nobody:")), None);
    }

    #[test]
    fn the_group_is_read_from_the_image_or_taken_as_a_number() {
        let text = "root:x:0:\nnogroup:x:65534:\nusers:x:100:alice,bob\nbroken\n";
        assert_eq!(gid_in(text, "nogroup"), Some(65534));
        assert_eq!(gid_in(text, "users"), Some(100));
        assert_eq!(gid_in(text, "alice"), None);
        assert_eq!(gid_in(text, "broken"), None);
        assert_eq!(gid_in("", "root"), None);
        assert!(script(None).contains("\ngroup=''\n"));
        let filled = script(Some(65534));
        assert!(filled.contains("\ngroup='65534'\n"));
        assert!(!filled.contains("group=''"));
    }

    #[test]
    fn the_stand_in_goes_where_systemd_nspawn_looks_first() {
        let with = |files: &'static [&str]| move |p: &str| files.contains(&p);
        let place = |target, real| Some(Placement { target, real });
        // The image's own getent is kept for what the script does not answer.
        assert_eq!(
            target(
                "m",
                "nobody",
                with(&["bin/sh", "usr/bin", "usr/bin/getent"])
            )
            .unwrap(),
            place("/usr/bin/getent", Some("usr/bin/getent"))
        );
        assert_eq!(
            target("m", "nobody", with(&["bin/sh", "bin/getent"])).unwrap(),
            place("/bin/getent", Some("bin/getent"))
        );
        assert_eq!(
            target("m", "nobody", with(&["bin/sh", "usr/bin"])).unwrap(),
            place("/usr/bin/getent", None)
        );
        assert_eq!(
            target("m", "nobody", with(&["bin/sh"])).unwrap(),
            place("/bin/getent", None)
        );
        // No shell: the image's getent is systemd-nspawn's to run, or nothing is.
        assert_eq!(
            target("m", "nobody", with(&["usr/bin/getent"])).unwrap(),
            None
        );
        let err = target("m", "nobody", with(&["usr/bin"])).unwrap_err();
        assert!(
            err.to_string()
                .contains("m runs as nobody, which systemd-nspawn resolves with getent"),
            "{err}"
        );
    }

    #[test]
    fn the_script_is_written_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SCRIPT);
        write(tmp.path(), Some(7)).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("group='7'"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(SCRIPT.starts_with("#!/bin/sh\n"));
        assert!(SCRIPT.contains(&format!("exec {REAL} \"$@\"")));
        // Only builtins: systemd-nspawn runs getent with an empty environment.
        for word in ["awk", "grep", "sed", "cut", "cat"] {
            assert!(!SCRIPT.contains(&format!("\n    {word} ")), "{word}");
        }
    }
}
