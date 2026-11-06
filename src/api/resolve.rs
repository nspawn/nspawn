//! ResolveImage: which local record the name of an image means. The command line and the
//! Docker API read a name without a registry each their own way, and both ask this.

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
    Docker,
}

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
    let as_hub = ImageRef::normalized(text).map(by_digest);
    let as_local = ImageRef::parse(text, registry).map(by_digest);
    let ordered = match reading {
        Reading::Nspawn => [(as_local, false), (as_hub, false)],
        Reading::Docker => [(as_hub, false), (as_local, true)],
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
/// them and the last made first; with `images_only`, the images alone; never `except`.
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
            .filter(|(image, _)| !images_only || *image)
            .max_by_key(|(image, r)| (*image, r.created));
        if let Some((_, record)) = found {
            return Ok(Resolved {
                name: record.name.clone(),
                reference: wanted.clone(),
            });
        }
    }
    Err(FailureKind::NoSuchImage.error(format!("no local image of {text}")))
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
            spellings("somos/app:1", REGISTRY, Reading::Docker).unwrap(),
            [
                ("docker.io/somos/app:1".to_string(), false),
                ("hub.nspawn.org/somos/app:1".to_string(), true)
            ]
        );
        // A name with its registry is that one alone, whoever reads it.
        for reading in [Reading::Nspawn, Reading::Docker] {
            assert_eq!(
                spellings("ghcr.io/x/y:2", REGISTRY, reading).unwrap(),
                [("ghcr.io/x/y:2".to_string(), false)]
            );
        }
        assert_eq!(
            spellings("busybox", "docker.io", Reading::Docker).unwrap(),
            [("docker.io/library/busybox:latest".to_string(), false)]
        );
        let pinned = format!("busybox:1.37@sha256:{}", "a".repeat(64));
        assert_eq!(
            spellings(&pinned, REGISTRY, Reading::Docker).unwrap()[0].0,
            format!("docker.io/library/busybox@sha256:{}", "a".repeat(64))
        );
        let bad = spellings("Bad Name", REGISTRY, Reading::Nspawn).unwrap_err();
        assert_eq!(crate::api::kind_of(&bad), Some(FailureKind::Invalid));
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
            Reading::Docker,
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
        assert!(resolve(&store, REGISTRY, &other, Reading::Docker, false, None).is_err());
        // The image before the machine made from it, though the machine is newer; the
        // machine when it alone goes by the reference, unless images only are asked for.
        let tagged = resolve(
            &store,
            REGISTRY,
            "busybox:1.37",
            Reading::Docker,
            false,
            None,
        );
        assert_eq!(tagged.unwrap().name, "busybox-1.37");
        let (_tmp, store) = store_of(&[machine]);
        let alone = resolve(
            &store,
            REGISTRY,
            "busybox:1.37",
            Reading::Docker,
            false,
            None,
        );
        assert_eq!(alone.unwrap().name, "web");
        assert!(resolve(
            &store,
            REGISTRY,
            "busybox:1.37",
            Reading::Docker,
            true,
            None
        )
        .is_err());
    }

    #[test]
    fn a_name_means_the_record_its_reading_finds_first() {
        let built = record("somos-app-1", "hub.nspawn.org/somos/app:1", "build", 1);
        let (_tmp, store) = store_of(std::slice::from_ref(&built));
        // Both readings find an image built here by its short name.
        for reading in [Reading::Nspawn, Reading::Docker] {
            let found = resolve(&store, REGISTRY, "somos/app:1", reading, false, None).unwrap();
            assert_eq!(found.name, "somos-app-1");
            assert_eq!(found.reference, "hub.nspawn.org/somos/app:1");
        }
        // Docker Hub's image of the name goes first for the Docker API, after the
        // configured registry's for the command line.
        let hub = record("somos-app-hub", "docker.io/somos/app:1", "pull", 2);
        let (_tmp, store) = store_of(&[built.clone(), hub]);
        let docker = resolve(
            &store,
            REGISTRY,
            "somos/app:1",
            Reading::Docker,
            false,
            None,
        )
        .unwrap();
        assert_eq!(docker.name, "somos-app-hub");
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
        let none = resolve(&store, REGISTRY, "busybox", Reading::Docker, false, None).unwrap_err();
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
