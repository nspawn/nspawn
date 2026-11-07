//! ResolveImage: which local record the name of a machine or an image means. Every command
//! and every method of the bus that takes one reads it with `find` (`name_of`); the Docker
//! API reads a name without a registry as its clients mean it, through `resolve`.

use anyhow::Result;

use crate::api::FailureKind;
use crate::reference::ImageRef;
use crate::store::{ImageRecord, Store};

/// How a client reads the name of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The command line's: a name without a registry under the configured registry, as
    /// nspawn names what it pulls and builds, then as Docker Hub's (`app` is
    /// `docker.io/library/app`), for an image compose took from there.
    Nspawn,
    /// The Docker Engine API's: a name without a registry is Docker Hub's, as its clients
    /// mean it; an image built here goes by it too, under the configured registry, while
    /// Docker Hub's of that name is not here (`nspawn build -t somos/app:1` files the image
    /// as `hub.nspawn.org/somos/app:1`, which a client asks for as `somos/app:1`).
    DockerApi,
}

/// The option of ResolveImage that asks for `Reading::DockerApi`.
pub const DOCKER_API_OPTION: &str = "docker_api";

/// The record a name means, and the reference it goes by there in full: a tag, or
/// `repository@digest` for a name with a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub reference: String,
}

/// A record's reference in full, the way it names an image: None for one pinned by a
/// digest alone.
pub fn full_tag(reference: &str, registry: &str) -> Option<String> {
    let parsed = ImageRef::parse(reference, registry).ok()?;
    // One pulled by a digest is that digest's, whatever tag came with it.
    if parsed.digest.is_some() {
        return None;
    }
    parsed.tag.as_ref()?;
    Some(
        ImageRef {
            digest: None,
            ..parsed
        }
        .to_string(),
    )
}

/// `repository@digest` of the manifest a record was pulled by, in full; a built image,
/// and a container made from one, was pulled by none.
pub fn full_digest(record: &ImageRecord, registry: &str) -> Option<String> {
    if record.origin == "build" || record.built {
        return None;
    }
    let parsed = ImageRef::parse(&record.reference, registry).ok()?;
    let digest = record
        .index_digest
        .clone()
        .unwrap_or_else(|| record.manifest_digest.clone());
    Some(
        ImageRef {
            tag: None,
            digest: Some(digest),
            ..parsed
        }
        .to_string(),
    )
}

/// A reference as it is looked up: by its digest alone when it has one, the tag beside
/// it left out.
pub fn by_digest(reference: ImageRef) -> ImageRef {
    match reference.digest {
        Some(_) => ImageRef {
            tag: None,
            ..reference
        },
        None => reference,
    }
}

/// The references `text` stands for, in the order they are looked for, each with
/// whether only an image built here goes by it.
pub fn spellings(text: &str, registry: &str, reading: Reading) -> Result<Vec<(String, bool)>> {
    // Parsing trims a reference: a name with a stray space or a CR, from a file or a
    // variable, is no reference to look for in its place.
    if text.trim() != text || text.chars().any(char::is_control) {
        return Err(FailureKind::Invalid.error(format!("invalid reference {text:?}")));
    }
    let as_hub = ImageRef::normalized(text).map(by_digest);
    let as_local = ImageRef::parse(text, registry).map(by_digest);
    let ordered = match reading {
        Reading::Nspawn => [(as_local, false), (as_hub, false)],
        Reading::DockerApi => [(as_hub, false), (as_local, true)],
    };
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut refused = None;
    for (parsed, built_only) in ordered {
        match parsed {
            Ok(reference) => {
                let reference = reference.to_string();
                if !out.iter().any(|(r, _)| *r == reference) {
                    out.push((reference, built_only));
                }
            }
            Err(e) => refused = Some(e),
        }
    }
    match (out.is_empty(), refused) {
        (true, Some(e)) => Err(FailureKind::Invalid.error(format!("{e:#}"))),
        _ => Ok(out),
    }
}

/// The record `text`, a reference, means as `reading` reads it: among the records of the
/// first spelling that has any, the images (never run) before the machines made from
/// them and the last made first; with `images_only`, the images alone, and none when that
/// spelling has machines only (never an image of the next spelling, another registry's);
/// never `except`.
pub fn resolve(
    store: &Store,
    registry: &str,
    text: &str,
    reading: Reading,
    images_only: bool,
    except: Option<&str>,
) -> Result<Resolved> {
    let spelled = spellings(text, registry, reading)?;
    let records = store.list_images()?;
    for (wanted, built_only) in &spelled {
        let found = records
            .iter()
            .filter(|r| Some(r.name.as_str()) != except)
            .filter(|r| !built_only || r.origin == "build" || r.built)
            .filter(|r| {
                full_tag(&r.reference, registry).as_deref() == Some(wanted.as_str())
                    || full_digest(r, registry).as_deref() == Some(wanted.as_str())
            })
            .map(|r| (crate::api::disk::image_only(store, r), r))
            .max_by_key(|(image, r)| (*image, r.created));
        // The images go first: the best of the spelling is a machine only when it has
        // no image.
        match found {
            Some((image, record)) if image || !images_only => {
                return Ok(Resolved {
                    name: record.name.clone(),
                    reference: wanted.clone(),
                })
            }
            Some(_) => break,
            None => {}
        }
    }
    Err(FailureKind::NoSuchImage.error(format!("no local image of {text}")))
}

/// The record `text` names wherever nspawn takes a machine or an image: the record of
/// that name when there is one, as a name says which record it is; else the record of the
/// reference `text` is, read as `Reading::Nspawn` reads one, the images alone with
/// `images_only`. `except` is never the answer.
pub fn find(
    store: &Store,
    registry: &str,
    text: &str,
    images_only: bool,
    except: Option<&str>,
) -> Result<Resolved> {
    if except != Some(text) {
        if let Some(record) = store.load_image(text)? {
            let reference = ImageRef::parse(&record.reference, registry)
                .map_or(record.reference, |r| r.to_string());
            return Ok(Resolved {
                name: record.name,
                reference,
            });
        }
    }
    resolve(store, registry, text, Reading::Nspawn, images_only, except)
}

/// The name of the record `text` names, as every method of the bus that takes a machine
/// or an image reads it: a name as it is, the record of that name or none, so that the
/// names a program gives (and those of machines that went meanwhile) never come to mean
/// an image of a reference; a text that can be no machine's name by `find`, or as it is
/// when nothing here goes by it, for the method to say so. A reference names the image
/// pulled or built for it, never a machine made from it, but where the image is only read
/// (`images_only` false: inspect, the source of create, push), which such a machine still
/// holds when no image of it is left. The command line reads every text by `find` first
/// (ResolveImage with names), a bare name as a reference too.
pub fn name_of(store: &Store, registry: &str, text: &str, images_only: bool) -> Result<String> {
    if crate::reference::validate_machine_name(text).is_ok() {
        return Ok(text.to_string());
    }
    match find(store, registry, text, images_only, None) {
        Ok(found) => Ok(found.name),
        Err(e)
            if matches!(
                crate::api::kind_of(&e),
                Some(FailureKind::NoSuchImage | FailureKind::Invalid)
            ) =>
        {
            Ok(text.to_string())
        }
        Err(e) => Err(e),
    }
}

/// `name_of` for a method that takes a machine: a reference of no image here is refused
/// as `rm` refuses it (`no_machine_of`), not as no machine's name; a text that is no
/// reference either goes on as it is, for the method to refuse the name.
pub fn machine_of(store: &Store, registry: &str, text: &str, images_only: bool) -> Result<String> {
    if crate::reference::validate_machine_name(text).is_ok() {
        return Ok(text.to_string());
    }
    match find(store, registry, text, images_only, None) {
        Ok(found) => Ok(found.name),
        Err(e) => match crate::api::kind_of(&e) {
            Some(FailureKind::NoSuchImage) => Err(no_machine_of(text)),
            Some(FailureKind::Invalid) => Ok(text.to_string()),
            _ => Err(e),
        },
    }
}

/// `machine_of` for each of `texts`, the images alone for a reference, each name once.
pub fn machines_of(store: &Store, registry: &str, texts: &[String]) -> Result<Vec<String>> {
    once_each(texts, |text| machine_of(store, registry, text, true))
}

/// What a method that takes a machine says of a reference of no image here.
pub fn no_machine_of(text: &str) -> anyhow::Error {
    FailureKind::NoSuchMachine.error(format!("no machine or image of {text}"))
}

/// The name `read` gives each of `texts`, each name once, in the order given.
fn once_each(texts: &[String], read: impl Fn(&str) -> Result<String>) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    for text in texts {
        let name = read(text)?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "hub.nspawn.org";

    fn record(name: &str, reference: &str, origin: &str, created: u64) -> ImageRecord {
        serde_json::from_value(serde_json::json!({
            "name": name, "reference": reference, "manifest_digest": format!("sha256:{}", "d".repeat(64)),
            "layers": [], "backend": "overlay", "created": created, "origin": origin,
        }))
        .unwrap()
    }

    fn store_of(records: &[ImageRecord]) -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(&tmp.path().join("machines"), &tmp.path().join("state"));
        store.init().unwrap();
        for r in records {
            store.record_image(r).unwrap();
        }
        (tmp, store)
    }

    #[test]
    fn each_reading_spells_a_short_name_its_own_way() {
        assert_eq!(
            spellings("somos/app:1", REGISTRY, Reading::Nspawn).unwrap(),
            [
                ("hub.nspawn.org/somos/app:1".to_string(), false),
                ("docker.io/somos/app:1".to_string(), false)
            ]
        );
        assert_eq!(
            spellings("somos/app:1", REGISTRY, Reading::DockerApi).unwrap(),
            [
                ("docker.io/somos/app:1".to_string(), false),
                ("hub.nspawn.org/somos/app:1".to_string(), true)
            ]
        );
        // A name with its registry is that one alone, whoever reads it.
        for reading in [Reading::Nspawn, Reading::DockerApi] {
            assert_eq!(
                spellings("ghcr.io/x/y:2", REGISTRY, reading).unwrap(),
                [("ghcr.io/x/y:2".to_string(), false)]
            );
        }
        assert_eq!(
            spellings("busybox", "docker.io", Reading::DockerApi).unwrap(),
            [("docker.io/library/busybox:latest".to_string(), false)]
        );
        let pinned = format!("busybox:1.37@sha256:{}", "a".repeat(64));
        assert_eq!(
            spellings(&pinned, REGISTRY, Reading::DockerApi).unwrap()[0].0,
            format!("docker.io/library/busybox@sha256:{}", "a".repeat(64))
        );
        let bad = spellings("Bad Name", REGISTRY, Reading::Nspawn).unwrap_err();
        assert_eq!(crate::api::kind_of(&bad), Some(FailureKind::Invalid));
        // A stray space or a CR, and a first component that starts with a dot, make no
        // reference.
        for text in [
            "web\r",
            " web",
            "web ",
            "busybox:1.37\n",
            "../../../tmp/x",
            ".x/foo",
        ] {
            let refused = spellings(text, REGISTRY, Reading::Nspawn).unwrap_err();
            assert_eq!(
                crate::api::kind_of(&refused),
                Some(FailureKind::Invalid),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_spelling_with_machines_only_is_no_image_and_never_the_next_registry_s() {
        // The configured registry's nginx went into machine web; Docker Hub's nginx is an
        // image pulled under another name.
        let mut web = record("web", "hub.nspawn.org/nginx:latest", "pull", 2);
        web.started = Some(2);
        let hub = record("hubnginx", "docker.io/library/nginx:latest", "pull", 1);
        let (_tmp, store) = store_of(&[web, hub]);
        let read = |images_only| find(&store, REGISTRY, "nginx", images_only, None);
        assert_eq!(read(false).unwrap().name, "web");
        let none = read(true).unwrap_err();
        assert_eq!(crate::api::kind_of(&none), Some(FailureKind::NoSuchImage));
        // Written with its registry, Docker Hub's is that one.
        assert_eq!(
            find(&store, REGISTRY, "docker.io/nginx", true, None)
                .unwrap()
                .name,
            "hubnginx"
        );
    }

    #[test]
    fn a_digest_names_the_record_pulled_by_it_and_an_image_goes_before_a_machine() {
        let digest = format!("sha256:{}", "d".repeat(64));
        let image = record("busybox-1.37", "docker.io/library/busybox:1.37", "pull", 1);
        let mut machine = record("web", "docker.io/library/busybox:1.37", "pull", 2);
        machine.started = Some(2);
        let (_tmp, store) = store_of(&[image.clone(), machine.clone()]);
        let pinned = resolve(
            &store,
            REGISTRY,
            &format!("busybox:1.37@{digest}"),
            Reading::DockerApi,
            false,
            None,
        )
        .unwrap();
        assert_eq!(pinned.name, "busybox-1.37");
        assert_eq!(
            pinned.reference,
            format!("docker.io/library/busybox@{digest}")
        );
        let other = format!("busybox:1.37@sha256:{}", "e".repeat(64));
        assert!(resolve(&store, REGISTRY, &other, Reading::DockerApi, false, None).is_err());
        // The image before the machine made from it, though the machine is newer; the
        // machine when it alone goes by the reference, unless images only are asked for.
        let tagged = resolve(
            &store,
            REGISTRY,
            "busybox:1.37",
            Reading::DockerApi,
            false,
            None,
        );
        assert_eq!(tagged.unwrap().name, "busybox-1.37");
        let (_tmp, store) = store_of(&[machine]);
        let alone = resolve(
            &store,
            REGISTRY,
            "busybox:1.37",
            Reading::DockerApi,
            false,
            None,
        );
        assert_eq!(alone.unwrap().name, "web");
        assert!(resolve(
            &store,
            REGISTRY,
            "busybox:1.37",
            Reading::DockerApi,
            true,
            None
        )
        .is_err());
    }

    #[test]
    fn a_record_name_goes_before_a_reference_and_each_command_reads_both() {
        let built = record(
            "somos-rust-libusb-bookworm",
            "hub.nspawn.org/somos/rust-libusb:bookworm",
            "build",
            1,
        );
        let hub = record("nginx", "docker.io/library/nginx:latest", "pull", 2);
        let mut machine = record("web", "docker.io/library/nginx:latest", "pull", 3);
        machine.started = Some(3);
        // A record named like a reference of another image: its name wins.
        let named = record("app", "hub.nspawn.org/other:1", "pull", 4);
        let (_tmp, store) = store_of(&[built, hub, machine, named]);
        let find = |text: &str, images_only: bool, except: Option<&str>| {
            find(&store, REGISTRY, text, images_only, except).map(|r| (r.name, r.reference))
        };
        let pair = |name: &str, reference: &str| (name.to_string(), reference.to_string());
        let built = pair(
            "somos-rust-libusb-bookworm",
            "hub.nspawn.org/somos/rust-libusb:bookworm",
        );
        // The local name and the reference, short or in full, are the same image.
        for text in [
            "somos-rust-libusb-bookworm",
            "somos/rust-libusb:bookworm",
            "hub.nspawn.org/somos/rust-libusb:bookworm",
        ] {
            assert_eq!(find(text, false, None).unwrap(), built, "{text}");
            assert_eq!(find(text, true, None).unwrap(), built, "{text}");
        }
        // Docker Hub's, by its short name too, after the configured registry's.
        assert_eq!(
            find("nginx:latest", false, None).unwrap(),
            pair("nginx", "docker.io/library/nginx:latest")
        );
        assert_eq!(
            find("docker.io/nginx", false, None).unwrap().0,
            "nginx",
            "Docker Hub's names spelled another way"
        );
        assert_eq!(
            find("app", false, None).unwrap(),
            pair("app", "hub.nspawn.org/other:1")
        );
        // A machine by its name; by a reference only when no image goes by it, and never
        // with images_only.
        assert_eq!(find("web", true, None).unwrap().0, "web");
        let (_tmp, store) = store_of(&[{
            let mut m = record("db", "docker.io/library/postgres:18", "pull", 5);
            m.started = Some(5);
            m
        }]);
        assert_eq!(
            super::find(&store, REGISTRY, "postgres:18", false, None)
                .unwrap()
                .name,
            "db"
        );
        let images = super::find(&store, REGISTRY, "postgres:18", true, None).unwrap_err();
        assert_eq!(crate::api::kind_of(&images), Some(FailureKind::NoSuchImage));
        // `except`, the machine about to be made, is read as a reference instead.
        let excepted = super::find(&store, REGISTRY, "db", false, Some("db")).unwrap_err();
        assert_eq!(
            crate::api::kind_of(&excepted),
            Some(FailureKind::NoSuchImage)
        );
        let none = super::find(&store, REGISTRY, "nothing-here", false, None).unwrap_err();
        assert_eq!(crate::api::kind_of(&none), Some(FailureKind::NoSuchImage));
    }

    #[test]
    fn every_command_reads_a_name_with_name_of() {
        let image = record("busybox-1.37", "docker.io/library/busybox:1.37", "pull", 1);
        let mut machine = record("web", "docker.io/library/nginx:latest", "pull", 2);
        machine.started = Some(2);
        let (_tmp, store) = store_of(&[image, machine]);
        let name_of =
            |text: &str, images_only: bool| name_of(&store, REGISTRY, text, images_only).unwrap();
        // A record by its name, an image by its reference, short or in full.
        for text in [
            "busybox-1.37",
            "busybox:1.37",
            "docker.io/library/busybox:1.37",
        ] {
            assert_eq!(name_of(text, true), "busybox-1.37", "{text}");
        }
        assert_eq!(name_of("web", true), "web");
        // A name is that record's or nothing's, never an image's by its reference: what
        // `find` reads `busybox` as, for the command line, a method takes as it is.
        let (_tmp2, other) =
            store_of(&[record("bb", "docker.io/library/busybox:latest", "pull", 3)]);
        assert_eq!(
            super::name_of(&other, REGISTRY, "busybox", true).unwrap(),
            "busybox"
        );
        assert_eq!(
            find(&other, REGISTRY, "busybox", true, None).unwrap().name,
            "bb"
        );
        assert_eq!(
            super::name_of(&other, REGISTRY, "busybox:latest", true).unwrap(),
            "bb"
        );
        // A reference never names a machine made from the image, but to read the image.
        assert_eq!(name_of("nginx:latest", true), "nginx:latest");
        assert_eq!(name_of("nginx:latest", false), "web");
        // What nothing here goes by stays as it is, for the command to say so; so does
        // what is no reference at all.
        for text in ["gone", "alpine:3.22", "Bad Name", ""] {
            assert_eq!(name_of(text, true), text, "{text:?}");
        }
        let texts = ["busybox:1.37", "web", "busybox-1.37", "gone"].map(String::from);
        // A method that takes a machine reads a name the same way, and refuses a reference
        // of no image here as rm does; what is no reference is the method's to refuse.
        let machine_of =
            |text: &str, images_only: bool| super::machine_of(&store, REGISTRY, text, images_only);
        assert_eq!(machine_of("busybox:1.37", true).unwrap(), "busybox-1.37");
        assert_eq!(machine_of("nginx:latest", false).unwrap(), "web");
        for text in ["gone", "Bad Name", "../../../tmp/x", ""] {
            assert_eq!(machine_of(text, true).unwrap(), text, "{text:?}");
        }
        for (text, images_only) in [("alpine:3.22", true), ("nginx:latest", true)] {
            let refused = machine_of(text, images_only).unwrap_err();
            assert_eq!(
                crate::api::kind_of(&refused),
                Some(FailureKind::NoSuchMachine),
                "{text}"
            );
            assert_eq!(
                refused.to_string(),
                format!("no machine or image of {text}")
            );
        }
        // Each name once, in the order given.
        assert_eq!(
            machines_of(&store, REGISTRY, &texts).unwrap(),
            ["busybox-1.37", "web", "gone"]
        );
        let missing = ["web", "alpine:3.22"].map(String::from);
        assert!(machines_of(&store, REGISTRY, &missing).is_err());
    }

    #[test]
    fn a_name_means_the_record_its_reading_finds_first() {
        let built = record("somos-app-1", "hub.nspawn.org/somos/app:1", "build", 1);
        let (_tmp, store) = store_of(std::slice::from_ref(&built));
        // Both readings find an image built here by its short name.
        for reading in [Reading::Nspawn, Reading::DockerApi] {
            let found = resolve(&store, REGISTRY, "somos/app:1", reading, false, None).unwrap();
            assert_eq!(found.name, "somos-app-1");
            assert_eq!(found.reference, "hub.nspawn.org/somos/app:1");
        }
        // Docker Hub's image of the name goes first for the Docker API, after the
        // configured registry's for the command line.
        let hub = record("somos-app-hub", "docker.io/somos/app:1", "pull", 2);
        let (_tmp, store) = store_of(&[built.clone(), hub]);
        let api = resolve(
            &store,
            REGISTRY,
            "somos/app:1",
            Reading::DockerApi,
            false,
            None,
        )
        .unwrap();
        assert_eq!(api.name, "somos-app-hub");
        let nspawn = resolve(
            &store,
            REGISTRY,
            "somos/app:1",
            Reading::Nspawn,
            false,
            None,
        )
        .unwrap();
        assert_eq!(nspawn.name, "somos-app-1");
        // An image pulled from the configured registry is not what a short name means to
        // a client of the Docker API.
        let pulled = record("busybox", "hub.nspawn.org/busybox:latest", "pull", 3);
        let (_tmp, store) = store_of(&[pulled]);
        let none =
            resolve(&store, REGISTRY, "busybox", Reading::DockerApi, false, None).unwrap_err();
        assert_eq!(crate::api::kind_of(&none), Some(FailureKind::NoSuchImage));
        assert_eq!(
            resolve(&store, REGISTRY, "busybox", Reading::Nspawn, false, None)
                .unwrap()
                .name,
            "busybox"
        );
        // The last made wins, an image before a machine; images_only and except narrow it.
        let older = record("web-old", "hub.nspawn.org/web:1", "pull", 4);
        let newer = record("web-new", "hub.nspawn.org/web:1", "pull", 5);
        let (_tmp, store) = store_of(&[older, newer]);
        let last = resolve(&store, REGISTRY, "web:1", Reading::Nspawn, true, None).unwrap();
        assert_eq!(last.name, "web-new");
        let other = resolve(
            &store,
            REGISTRY,
            "web:1",
            Reading::Nspawn,
            true,
            Some("web-new"),
        );
        assert_eq!(other.unwrap().name, "web-old");
    }
}
