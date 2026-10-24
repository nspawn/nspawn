//! A stand-in for getent in app machines that run as a user.
//!
//! systemd-nspawn resolves the user an app runs as (`User=`) by running `getent passwd`
//! and `getent initgroups` inside the machine. busybox and static images have no
//! getent, musl's (alpine) has no `initgroups`, and neither takes a uid the image's
//! passwd does not list, which --user allows, nor the group of USER:GROUP.
//! So nspawn binds a shell script over the path systemd-nspawn looks at that answers
//! those two from `/etc/passwd` and `/etc/group`, with the group asked for as the
//! primary and only one, and hands anything else to the image's own getent, bound at
//! `/run/nspawn/getent` where the image has one.
//!
//! An image without /bin/sh (distroless, scratch) gets a program instead: a few hundred
//! bytes of machine code that print the two answers, worked out by nspawn from the same
//! files ahead of the start, and hand anything else to the image's own getent the same
//! way. It runs without a libc, so it runs in any image.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::backend::BackendChoice;
use crate::settings::Bind;
use crate::store::{ImageRecord, Store};

/// Where the image's own getent is bound, for what the stand-in does not answer.
pub const REAL: &str = "/run/nspawn/getent";

/// Where the stand-in is bound: systemd-nspawn 255 runs /usr/bin/getent, else
/// /bin/getent; later ones look getent up on the service's PATH, which on a host whose
/// sbin is merged is /usr/local/bin:/usr/bin. systemd-nspawn makes /usr/bin when the
/// image has none.
const TARGET: &str = "/usr/bin/getent";

/// Answers `passwd KEY` and `initgroups KEY` by name or by uid; a uid the file does not
/// list runs all the same, with gid 0 and no home, and a group asked
/// for (`group=`, filled in by `script`) is the primary one and the only one. Nothing
/// but shell builtins: systemd-nspawn runs it with an empty environment.
pub const SCRIPT: &str = r#"#!/bin/sh
# Written by nspawn: systemd-nspawn resolves the user an app runs as with getent, and
# this answers passwd and initgroups from /etc/passwd and /etc/group, by name or by
# uid. Anything else goes to the image's own getent.
# The gid of --user USER:GROUP; empty for the passwd entry's own group and the ones
# /etc/group adds.
group=''
case "$1:$#" in
passwd:2)
    while IFS=: read -r name pw uid gid rest || [ -n "$name" ]; do
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
    while IFS=: read -r n pw uid gid rest || [ -n "$n" ]; do
        if [ "$n" = "$2" ] || [ "$uid" = "$2" ]; then
            name=$n
            break
        fi
    done < /etc/passwd
    printf '%s' "$2"
    while IFS=: read -r group pw gid members || [ -n "$group" ]; do
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

/// The binds of the stand-in for an app machine: none when it runs as root; the script,
/// or the program in an image without /bin/sh, at `TARGET`, and the
/// image's own getent at `REAL` when the root is assembled ahead of the start (an mstack
/// tree is not). Refused when the image has no /bin/sh, no getent of its own and this
/// architecture has no program.
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
    let arch = native();
    let Some(placement) = target(name, user, has, arch.is_some())? else {
        return Ok(Vec::new());
    };
    let content = match (placement.kind, arch) {
        (Kind::Program, Some(arch)) => {
            let passwd = root_text(store, name, record.backend, "etc/passwd");
            let groups = root_text(store, name, record.backend, "etc/group");
            let (passwd, initgroups) =
                answers(&passwd, &groups, user, group).with_context(|| {
                    format!("unable to find user {user}: no matching entries in passwd file")
                })?;
            program(arch, user, &passwd, &initgroups)
        }
        _ => script(group).into_bytes(),
    };
    let dir = store.machine_files_dir(name);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut binds = vec![Bind {
        source: write(&dir, &content)?,
        target: TARGET.to_string(),
        read_only: true,
    }];
    // A bind follows a symlink on the host, so only a program of the image itself is
    // handed over, found through the image's links inside its root.
    if let (Some(real), BackendChoice::Overlay | BackendChoice::Flat | BackendChoice::Auto) =
        (placement.real, record.backend)
    {
        let Some(source) = crate::backend::root_path(store, name, record.backend, real) else {
            return Ok(binds);
        };
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

/// The binds of the stand-in for a program run as `user` (USER[:GROUP]) in the plain
/// directory `root`, written to `dir`: what `shim` gives an app machine, for a build
/// step.
pub fn shim_in_dir(root: &Path, user: &str, dir: &Path) -> Result<Vec<Bind>> {
    let Some(name) = switches_to(Some(user)) else {
        return Ok(Vec::new());
    };
    let resolve = |r: &str| crate::backend::chase(root, Path::new(r));
    let group = match group_of(Some(user)) {
        Some(group) => Some(match group.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => gid_in(&text_in(&resolve, "etc/group"), group).with_context(|| {
                format!("unable to find group {group}: no matching entries in group file")
            })?,
        }),
        None => None,
    };
    let has = |path: &str| {
        resolve(path)
            .and_then(|p| p.symlink_metadata().ok())
            .is_some_and(|m| !m.is_file() || m.len() > 0)
    };
    let arch = native();
    let Some(placement) = target("the step", name, has, arch.is_some())? else {
        return Ok(Vec::new());
    };
    let content = match (placement.kind, arch) {
        (Kind::Program, Some(arch)) => {
            let passwd = text_in(&resolve, "etc/passwd");
            let groups = text_in(&resolve, "etc/group");
            let (passwd, initgroups) =
                answers(&passwd, &groups, name, group).with_context(|| {
                    format!("unable to find user {name}: no matching entries in passwd file")
                })?;
            program(arch, name, &passwd, &initgroups)
        }
        _ => script(group).into_bytes(),
    };
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut binds = vec![Bind {
        source: write(dir, &content)?,
        target: TARGET.to_string(),
        read_only: true,
    }];
    if let Some(real) = placement.real {
        // Inside the root whatever its links say: the step's image is anybody's.
        let Some(source) = resolve(real) else {
            return Ok(binds);
        };
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

/// Where the stand-in and the image's own getent go, mount points a run may leave.
pub const TARGETS: [&str; 2] = [TARGET, REAL];

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

/// The group of USER:GROUP, when given.
fn group_of(user: Option<&str>) -> Option<&str> {
    user?
        .split_once(':')
        .map(|(_, g)| g)
        .filter(|g| !g.is_empty())
}

/// The gid `group` stands for: itself when numeric, else the image's /etc/group entry
/// of that name, refused when there is none.
fn resolve_group(store: &Store, name: &str, backend: BackendChoice, group: &str) -> Result<u32> {
    if let Ok(gid) = group.parse::<u32>() {
        return Ok(gid);
    }
    let text = root_text(store, name, backend, "etc/group");
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

/// A file of the machine's root as text, its links followed inside the root and never
/// onto the host; empty when there is none, or it is not a regular file.
fn root_text(store: &Store, name: &str, backend: BackendChoice, relative: &str) -> String {
    text_in(
        &|r: &str| crate::backend::root_path(store, name, backend, r),
        relative,
    )
}

/// `root_text` over a root `resolve` finds paths in.
fn text_in(resolve: &dyn Fn(&str) -> Option<PathBuf>, relative: &str) -> String {
    let mut relative = PathBuf::from(relative);
    for _ in 0..40 {
        let Some(path) = relative.to_str().and_then(resolve) else {
            return String::new();
        };
        let Ok(link) = fs::read_link(&path) else {
            let mut text = String::new();
            if let Ok(file) = crate::volume::open_regular(&path) {
                let _ = file.take(1 << 20).read_to_string(&mut text);
            }
            return text;
        };
        relative = match link.strip_prefix("/") {
            Ok(inside) => inside.to_path_buf(),
            Err(_) => relative
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(link),
        };
    }
    String::new()
}

/// What the script prints for `passwd KEY` and `initgroups KEY`, from the text of the
/// image's passwd and group files: the program prints these. None for a name the passwd
/// file does not list.
fn answers(passwd: &str, groups: &str, key: &str, group: Option<u32>) -> Option<(String, String)> {
    let entry = passwd.lines().find(|line| {
        let mut fields = line.split(':');
        let name = fields.next();
        name == Some(key) || fields.nth(1) == Some(key)
    });
    let user = match entry {
        Some(line) => {
            let fields: Vec<&str> = line.splitn(5, ':').collect();
            let field = |i: usize| fields.get(i).copied().unwrap_or("");
            let gid = group.map_or_else(|| field(3).to_string(), |g| g.to_string());
            format!(
                "{}:{}:{}:{gid}:{}\n",
                field(0),
                field(1),
                field(2),
                field(4)
            )
        }
        None if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{key}:x:{key}:{}::/:/bin/sh\n", group.unwrap_or(0))
        }
        None => return None,
    };
    let initgroups = match group {
        Some(gid) => format!("{key} {gid}\n"),
        None => {
            let name = entry.and_then(|line| line.split(':').next()).unwrap_or(key);
            let mut line = key.to_string();
            for entry in groups.lines() {
                let fields: Vec<&str> = entry.splitn(4, ':').collect();
                if fields.len() == 4 && fields[3].split(',').any(|m| m == name) {
                    line.push(' ');
                    line.push_str(fields[2]);
                }
            }
            line.push('\n');
            line
        }
    };
    Some((user, initgroups))
}

/// The machine code of the program for one architecture. It reads a table of eight
/// words that follows the code: the addresses of the key, "passwd", "initgroups", the
/// answer to passwd and its length, the answer to initgroups and its length, and
/// `REAL`. For `passwd KEY` or `initgroups KEY` it writes the answer and exits 0 (1 when
/// the write fell short); for anything else it executes `REAL` with the same arguments
/// and environment, and exits 2 when that fails.
struct Arch {
    name: &'static str,
    /// The ELF machine number.
    machine: u16,
    /// The largest page size of the architecture, for the program header.
    align: u64,
    /// The code, up to where the table goes.
    code: &'static [u8],
}

const ARCHES: [Arch; 2] = [
    Arch {
        name: "x86_64",
        machine: 62,
        align: 0x1000,
        code: X86_64,
    },
    Arch {
        name: "aarch64",
        machine: 183,
        align: 0x1_0000,
        code: AARCH64,
    },
];

/// The architecture of this build, which the machines share, when there is code for it.
fn native() -> Option<&'static Arch> {
    ARCHES.iter().find(|a| a.name == std::env::consts::ARCH)
}

/// Assembled by GNU as from the instructions in the comments, at offset 0.
#[rustfmt::skip]
const X86_64: &[u8] = &[
    0x48, 0x89, 0xe3,                         // mov rbx, rsp: argc, then argv and envp
    0x48, 0x83, 0x3b, 0x03,                   // cmp qword ptr [rbx], 3
    0x75, 0x69,                               // jne other
    0x48, 0x8b, 0x7b, 0x18,                   // mov rdi, [rbx + 24]: argv[2]
    0x48, 0x8b, 0x35, 0x9c, 0x00, 0x00, 0x00, // mov rsi, [rip + key]
    0xe8, 0x7f, 0x00, 0x00, 0x00,             // call same
    0x75, 0x57,                               // jne other
    0x48, 0x8b, 0x7b, 0x10,                   // mov rdi, [rbx + 16]: argv[1]
    0x48, 0x8b, 0x35, 0x92, 0x00, 0x00, 0x00, // mov rsi, [rip + passwd]
    0xe8, 0x6d, 0x00, 0x00, 0x00,             // call same
    0x48, 0x8b, 0x35, 0x96, 0x00, 0x00, 0x00, // mov rsi, [rip + passwd_answer]
    0x48, 0x8b, 0x15, 0x97, 0x00, 0x00, 0x00, // mov rdx, [rip + passwd_length]
    0x74, 0x20,                               // je answer
    0x48, 0x8b, 0x7b, 0x10,                   // mov rdi, [rbx + 16]: argv[1]
    0x48, 0x8b, 0x35, 0x7a, 0x00, 0x00, 0x00, // mov rsi, [rip + initgroups]
    0xe8, 0x4d, 0x00, 0x00, 0x00,             // call same
    0x48, 0x8b, 0x35, 0x86, 0x00, 0x00, 0x00, // mov rsi, [rip + initgroups_answer]
    0x48, 0x8b, 0x15, 0x87, 0x00, 0x00, 0x00, // mov rdx, [rip + initgroups_length]
    0x75, 0x17,                               // jne other
    // answer:
    0xbf, 0x01, 0x00, 0x00, 0x00,             // mov edi, 1
    0xb8, 0x01, 0x00, 0x00, 0x00,             // mov eax, 1: write
    0x0f, 0x05,                               // syscall
    0x31, 0xff,                               // xor edi, edi
    0x48, 0x39, 0xd0,                         // cmp rax, rdx
    0x40, 0x0f, 0x95, 0xc7,                   // setne dil: 1 when the write fell short
    0xeb, 0x1f,                               // jmp done
    // other:
    0x48, 0x8b, 0x3d, 0x6f, 0x00, 0x00, 0x00, // mov rdi, [rip + real]
    0x48, 0x8d, 0x73, 0x08,                   // lea rsi, [rbx + 8]: argv
    0x48, 0x8b, 0x0b,                         // mov rcx, [rbx]
    0x48, 0x8d, 0x54, 0xcb, 0x10,             // lea rdx, [rbx + rcx * 8 + 16]: envp
    0xb8, 0x3b, 0x00, 0x00, 0x00,             // mov eax, 59: execve
    0x0f, 0x05,                               // syscall
    0xbf, 0x02, 0x00, 0x00, 0x00,             // mov edi, 2
    // done:
    0xb8, 0x3c, 0x00, 0x00, 0x00,             // mov eax, 60: exit
    0x0f, 0x05,                               // syscall
    // same:
    0x8a, 0x07,                               // mov al, [rdi]: ZF when rdi and rsi match up to their NUL
    0x3a, 0x06,                               // cmp al, [rsi]
    0x75, 0x0a,                               // jne 1f
    0x48, 0xff, 0xc7,                         // inc rdi
    0x48, 0xff, 0xc6,                         // inc rsi
    0x84, 0xc0,                               // test al, al
    0x75, 0xf0,                               // jnz same
    // 1:
    0xc3,                                     // ret
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // up to the table, aligned to 8
];

/// Assembled by GNU as from the instructions in the comments, at offset 0.
#[rustfmt::skip]
const AARCH64: &[u8] = &[
    0xf3, 0x03, 0x00, 0x91, // mov x19, sp: argc, then argv and envp
    0x60, 0x02, 0x40, 0xf9, // ldr x0, [x19]
    0x1f, 0x0c, 0x00, 0xf1, // cmp x0, #3
    0xe1, 0x02, 0x00, 0x54, // b.ne other
    0x60, 0x0e, 0x40, 0xf9, // ldr x0, [x19, #24]: argv[2]
    0xa1, 0x04, 0x00, 0x58, // ldr x1, key
    0x1e, 0x00, 0x00, 0x94, // bl same
    0x61, 0x02, 0x00, 0x54, // b.ne other
    0x60, 0x0a, 0x40, 0xf9, // ldr x0, [x19, #16]: argv[1]
    0x61, 0x04, 0x00, 0x58, // ldr x1, passwd
    0x1a, 0x00, 0x00, 0x94, // bl same
    0xa1, 0x04, 0x00, 0x58, // ldr x1, passwd_answer
    0xc2, 0x04, 0x00, 0x58, // ldr x2, passwd_length
    0xe0, 0x00, 0x00, 0x54, // b.eq answer
    0x60, 0x0a, 0x40, 0xf9, // ldr x0, [x19, #16]: argv[1]
    0xe1, 0x03, 0x00, 0x58, // ldr x1, initgroups
    0x14, 0x00, 0x00, 0x94, // bl same
    0x61, 0x04, 0x00, 0x58, // ldr x1, initgroups_answer
    0x82, 0x04, 0x00, 0x58, // ldr x2, initgroups_length
    0xe1, 0x00, 0x00, 0x54, // b.ne other
    // answer:
    0x20, 0x00, 0x80, 0xd2, // mov x0, #1
    0x08, 0x08, 0x80, 0xd2, // mov x8, #64: write
    0x01, 0x00, 0x00, 0xd4, // svc #0
    0x1f, 0x00, 0x02, 0xeb, // cmp x0, x2
    0xe0, 0x07, 0x9f, 0x9a, // cset x0, ne: 1 when the write fell short
    0x09, 0x00, 0x00, 0x14, // b done
    // other:
    0xc0, 0x03, 0x00, 0x58, // ldr x0, real
    0x61, 0x22, 0x00, 0x91, // add x1, x19, #8: argv
    0x63, 0x02, 0x40, 0xf9, // ldr x3, [x19]
    0x62, 0x0e, 0x03, 0x8b, // add x2, x19, x3, lsl #3
    0x42, 0x40, 0x00, 0x91, // add x2, x2, #16: envp
    0xa8, 0x1b, 0x80, 0xd2, // mov x8, #221: execve
    0x01, 0x00, 0x00, 0xd4, // svc #0
    0x40, 0x00, 0x80, 0xd2, // mov x0, #2
    // done:
    0xa8, 0x0b, 0x80, 0xd2, // mov x8, #93: exit
    0x01, 0x00, 0x00, 0xd4, // svc #0
    // same:
    0x02, 0x14, 0x40, 0x38, // ldrb w2, [x0], #1: Z when x0 and x1 match up to their NUL
    0x23, 0x14, 0x40, 0x38, // ldrb w3, [x1], #1
    0x5f, 0x00, 0x03, 0x6b, // cmp w2, w3
    0x41, 0x00, 0x00, 0x54, // b.ne 1f
    0x82, 0xff, 0xff, 0x35, // cbnz w2, same
    // 1:
    0xc0, 0x03, 0x5f, 0xd6, // ret
];

/// Where the program is loaded: a fixed address, as the code is not position-independent
/// beyond the table that follows it.
const BASE: u64 = 0x40_0000;
/// The ELF header and the two program headers, ahead of the code.
const HEADERS: usize = 64 + 2 * 56;

/// The program for `arch`, with the answers for `key` in it.
fn program(arch: &Arch, key: &str, passwd: &str, initgroups: &str) -> Vec<u8> {
    let table_at = HEADERS + arch.code.len();
    let data_at = table_at + 8 * 8;
    let mut data: Vec<u8> = Vec::new();
    let mut put = |bytes: &[u8]| {
        let address = BASE + (data_at + data.len()) as u64;
        data.extend_from_slice(bytes);
        address
    };
    let table = [
        put(&[key.as_bytes(), b"\0"].concat()),
        put(b"passwd\0"),
        put(b"initgroups\0"),
        put(passwd.as_bytes()),
        passwd.len() as u64,
        put(initgroups.as_bytes()),
        initgroups.len() as u64,
        put(&[REAL.as_bytes(), b"\0"].concat()),
    ];
    let size = (data_at + data.len()) as u64;
    let mut out = Vec::with_capacity(size as usize);
    // 64-bit, little-endian, version 1, an executable of `arch`.
    out.extend_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&arch.machine.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    // The entry point, the program headers right after this header, no sections.
    for word in [BASE + HEADERS as u64, 64, 0] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    for half in [64u16, 56, 2, 64, 0, 0] {
        out.extend_from_slice(&half.to_le_bytes());
    }
    // The whole file, loaded readable and executable.
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&5u32.to_le_bytes());
    for word in [0, BASE, BASE, size, size, arch.align] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    // A stack that is not executable (PT_GNU_STACK, read and write).
    out.extend_from_slice(&0x6474_e551u32.to_le_bytes());
    out.extend_from_slice(&6u32.to_le_bytes());
    for word in [0u64, 0, 0, 0, 0, 16] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(arch.code);
    for word in table {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&data);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Script,
    Program,
}

#[derive(Debug, PartialEq, Eq)]
struct Placement {
    /// The image's own getent, relative to the root, for what the stand-in does not
    /// answer.
    real: Option<&'static str>,
    kind: Kind,
}

/// Which stand-in, from what the root has: the script where there is a /bin/sh to run
/// it, else the program when there is one for this architecture.
/// None when neither can run but the image has a getent of its own, which is then
/// systemd-nspawn's to run.
fn target(
    name: &str,
    user: &str,
    has: impl Fn(&str) -> bool,
    program: bool,
) -> Result<Option<Placement>> {
    let real = ["usr/bin/getent", "bin/getent"]
        .into_iter()
        .find(|path| has(path));
    let kind = if has("bin/sh") {
        Kind::Script
    } else if program {
        Kind::Program
    } else if real.is_some() {
        return Ok(None);
    } else {
        bail!("{name} runs as {user}, which systemd-nspawn resolves with getent inside the machine, and the image has neither getent nor a /bin/sh to stand in for it; run it as root (-u root) or add getent to the image");
    };
    Ok(Some(Placement { real, kind }))
}

fn write(dir: &Path, content: &[u8]) -> Result<PathBuf> {
    let path = dir.join("getent");
    fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
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
        let place = |real, kind| Some(Placement { real, kind });
        // The image's own getent is kept for what the script does not answer.
        assert_eq!(
            target(
                "m",
                "nobody",
                with(&["bin/sh", "usr/bin", "usr/bin/getent"]),
                true
            )
            .unwrap(),
            place(Some("usr/bin/getent"), Kind::Script)
        );
        assert_eq!(
            target("m", "nobody", with(&["bin/sh", "bin/getent"]), true).unwrap(),
            place(Some("bin/getent"), Kind::Script)
        );
        assert_eq!(
            target("m", "nobody", with(&["bin/sh", "usr/bin"]), true).unwrap(),
            place(None, Kind::Script)
        );
        assert_eq!(
            target("m", "nobody", with(&["bin/sh"]), true).unwrap(),
            place(None, Kind::Script)
        );
        // No shell: the program, which hands the rest to the image's getent too.
        assert_eq!(
            target("m", "nobody", with(&["usr/bin", "usr/bin/getent"]), true).unwrap(),
            place(Some("usr/bin/getent"), Kind::Program)
        );
        assert_eq!(
            target("m", "1000", with(&[]), true).unwrap(),
            place(None, Kind::Program)
        );
        // Without a program for the architecture: the image's getent is
        // systemd-nspawn's to run, or nothing is.
        assert_eq!(
            target("m", "nobody", with(&["usr/bin/getent"]), false).unwrap(),
            None
        );
        let err = target("m", "nobody", with(&["usr/bin"]), false).unwrap_err();
        assert!(
            err.to_string()
                .contains("m runs as nobody, which systemd-nspawn resolves with getent"),
            "{err}"
        );
    }

    #[test]
    fn the_program_answers_as_the_script_would() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\nalice:x:1001:1001::/home/alice:/bin/sh\nshort:x:7:7";
        let groups = "root:x:0:\nusers:x:100:alice,bob\nwheel:x:10:alice\nnobody:x:65534:\nodd";
        let ask = |key, group| answers(passwd, groups, key, group);
        let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
        // By name or by uid, with the groups that list the name.
        assert_eq!(
            ask("alice", None),
            pair("alice:x:1001:1001::/home/alice:/bin/sh\n", "alice 100 10\n")
        );
        assert_eq!(
            ask("1001", None),
            pair("alice:x:1001:1001::/home/alice:/bin/sh\n", "1001 100 10\n")
        );
        assert_eq!(
            ask("nobody", None),
            pair(
                "nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n",
                "nobody\n"
            )
        );
        // The group of USER:GROUP is the primary and only one.
        assert_eq!(
            ask("alice", Some(4242)),
            pair("alice:x:1001:4242::/home/alice:/bin/sh\n", "alice 4242\n")
        );
        // A uid the file does not list runs with gid 0, or the group asked for, and /.
        assert_eq!(
            ask("1000", None),
            pair("1000:x:1000:0::/:/bin/sh\n", "1000\n")
        );
        assert_eq!(
            ask("1000", Some(1000)),
            pair("1000:x:1000:1000::/:/bin/sh\n", "1000 1000\n")
        );
        assert_eq!(
            answers("", "", "1000", None),
            pair("1000:x:1000:0::/:/bin/sh\n", "1000\n")
        );
        // A line with fewer fields is printed as the script's read splits it.
        assert_eq!(ask("short", None), pair("short:x:7:7:\n", "short\n"));
        assert_eq!(ask("ghost", None), None);
        assert_eq!(ask("", None), None);
    }

    /// The eight words of the table, and the bytes a word points at, of a program.
    fn table_of<'a>(elf: &'a [u8], arch: &Arch) -> (Vec<u64>, impl Fn(u64, usize) -> Vec<u8> + 'a) {
        let at = HEADERS + arch.code.len();
        let words = (0..8)
            .map(|i| u64::from_le_bytes(elf[at + 8 * i..at + 8 * i + 8].try_into().unwrap()))
            .collect();
        let read = move |address: u64, len: usize| {
            let offset = (address - BASE) as usize;
            elf[offset..offset + len].to_vec()
        };
        (words, read)
    }

    #[test]
    fn the_program_is_an_executable_with_its_answers_where_the_code_reads_them() {
        let u16_at =
            |elf: &[u8], at: usize| u16::from_le_bytes(elf[at..at + 2].try_into().unwrap());
        let u32_at =
            |elf: &[u8], at: usize| u32::from_le_bytes(elf[at..at + 4].try_into().unwrap());
        let u64_at =
            |elf: &[u8], at: usize| u64::from_le_bytes(elf[at..at + 8].try_into().unwrap());
        // The code reads the table at these offsets: x86_64's rip-relative loads and
        // aarch64's literal loads were assembled for them.
        assert_eq!(X86_64.len(), 0xb0);
        assert_eq!(AARCH64.len(), 0xa8);
        assert_eq!(AARCH64.len() % 4, 0);
        for arch in &ARCHES {
            let elf = program(arch, "1000", "1000:x:1000:0::/:/bin/sh\n", "1000\n");
            assert_eq!(&elf[..7], b"\x7fELF\x02\x01\x01", "{}", arch.name);
            assert_eq!(u16_at(&elf, 16), 2, "an executable");
            assert_eq!(u16_at(&elf, 18), arch.machine);
            assert_eq!(
                u64_at(&elf, 24),
                BASE + HEADERS as u64,
                "the entry is the code"
            );
            assert_eq!(u64_at(&elf, 32), 64);
            assert_eq!(u16_at(&elf, 54), 56);
            assert_eq!(u16_at(&elf, 56), 2);
            // One load of the whole file, readable and executable, and a stack that is
            // not executable.
            assert_eq!((u32_at(&elf, 64), u32_at(&elf, 68)), (1, 5));
            assert_eq!(u64_at(&elf, 64 + 8), 0);
            assert_eq!(u64_at(&elf, 64 + 16), BASE);
            assert_eq!(u64_at(&elf, 64 + 32), elf.len() as u64);
            assert_eq!(u64_at(&elf, 64 + 40), elf.len() as u64);
            assert_eq!(u64_at(&elf, 64 + 48), arch.align);
            assert_eq!(BASE % arch.align, 0);
            assert_eq!((u32_at(&elf, 120), u32_at(&elf, 124)), (0x6474_e551, 6));
            assert_eq!(&elf[HEADERS..HEADERS + arch.code.len()], arch.code);
            assert_eq!((HEADERS + arch.code.len()) % 8, 0, "the table is aligned");
            let (words, read) = table_of(&elf, arch);
            assert_eq!(read(words[0], 5), b"1000\0");
            assert_eq!(read(words[1], 7), b"passwd\0");
            assert_eq!(read(words[2], 11), b"initgroups\0");
            assert_eq!(words[4], 25);
            assert_eq!(read(words[3], 25), b"1000:x:1000:0::/:/bin/sh\n");
            assert_eq!(words[6], 5);
            assert_eq!(read(words[5], 5), b"1000\n");
            assert_eq!(
                read(words[7], REAL.len() + 1),
                format!("{REAL}\0").as_bytes()
            );
            assert_eq!(words[7] - BASE + REAL.len() as u64 + 1, elf.len() as u64);
        }
        assert!(native().is_some() == cfg!(any(target_arch = "x86_64", target_arch = "aarch64")));
    }

    #[test]
    fn the_image_s_files_are_read_inside_its_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let root = store.machines_dir.join("m");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(root.join("usr/share/base")).unwrap();
        let host = tmp.path().join("host-secret");
        fs::write(&host, "secret:x:1:1::/:/bin/sh\n").unwrap();
        let text = |relative| root_text(&store, "m", BackendChoice::Flat, relative);
        // The file itself, and links followed inside the root, absolute or relative.
        fs::write(
            root.join("usr/share/base/passwd"),
            "alice:x:1001:1001::/:/bin/sh\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("/usr/share/base/passwd", root.join("etc/passwd")).unwrap();
        assert_eq!(text("etc/passwd"), "alice:x:1001:1001::/:/bin/sh\n");
        std::os::unix::fs::symlink("../usr/share/base/passwd", root.join("etc/rel")).unwrap();
        assert_eq!(text("etc/rel"), "alice:x:1001:1001::/:/bin/sh\n");
        // A link that names a host path, or climbs out, stays in the root, where there
        // is nothing.
        std::os::unix::fs::symlink(&host, root.join("etc/group")).unwrap();
        assert_eq!(text("etc/group"), "");
        std::os::unix::fs::symlink("../../../host-secret", root.join("etc/climb")).unwrap();
        assert_eq!(text("etc/climb"), "");
        // A loop, a directory or a fifo is nothing to read.
        std::os::unix::fs::symlink("loop", root.join("etc/loop")).unwrap();
        assert_eq!(text("etc/loop"), "");
        assert_eq!(text("usr/share"), "");
        nix::unistd::mkfifo(&root.join("etc/fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert_eq!(text("etc/fifo"), "");
        assert_eq!(text("etc/missing"), "");
    }

    #[test]
    fn the_image_s_getent_is_bound_from_inside_its_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        let root = store.machines_dir.join("m");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/sh"), "#!sh").unwrap();
        fs::write(root.join("bin/getent"), "getent").unwrap();
        // usr is the root itself: usr/bin/getent is the image's bin/getent, never the
        // host's through the link.
        std::os::unix::fs::symlink("/", root.join("usr")).unwrap();
        let record: ImageRecord = serde_json::from_value(serde_json::json!({
            "name": "m", "reference": "r", "manifest_digest": "sha256:a", "layers": [],
            "backend": "flat", "created": 1, "mode": "app", "run": { "user": "nobody" },
        }))
        .unwrap();
        let binds = shim(&store, "m", &record).unwrap();
        let real: Vec<&Bind> = binds.iter().filter(|b| b.target == REAL).collect();
        assert_eq!(real.len(), 1, "{binds:?}");
        assert_eq!(real[0].source, root.join("bin/getent"));
    }

    #[test]
    fn the_script_is_written_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), script(None).as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SCRIPT);
        write(tmp.path(), script(Some(7)).as_bytes()).unwrap();
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
