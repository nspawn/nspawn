//! Named volumes as the Docker API shows them: the local driver's, kept under the
//! store's volumes directory.

use serde_json::{json, Value};

use super::filters::{Filters, Invalid};
use crate::api::volumes::VolumeInfo;

/// The keys GET /volumes takes.
const FILTERS: &[&str] = &["dangling", "driver", "label", "name"];

/// The volumes `filters` selects, as the engine reads them.
pub fn select<'a>(
    volumes: &'a [VolumeInfo],
    filters: &Filters,
) -> Result<Vec<&'a VolumeInfo>, Invalid> {
    filters.only(FILTERS)?;
    if let Some(values) = filters.values("dangling") {
        if let Some(bad) = values
            .iter()
            .find(|v| !matches!(v.as_str(), "true" | "false" | "1" | "0"))
        {
            return Err(Invalid(format!("invalid filter 'dangling={bad}'")));
        }
    }
    Ok(volumes
        .iter()
        .filter(|v| match filters.values("dangling") {
            Some(values) => {
                let want = values.iter().any(|x| x == "true" || x == "1");
                v.used_by.is_empty() == want
            }
            None => true,
        })
        .filter(|_| filters.values("driver").is_none() || filters.matches("driver", "local"))
        .filter(|v| filters.values("name").is_none() || filters.matches("name", &v.name))
        .filter(|_| {
            filters.values("label").is_none() || filters.labels("label", &Default::default())
        })
        .collect())
}

/// A volume, as GET /volumes/{name} and the list show it.
pub fn json(volume: &VolumeInfo) -> Value {
    json!({
        "CreatedAt": super::util::rfc3339_nano(volume.created * 1_000_000, false),
        "Driver": "local",
        "Labels": null,
        "Mountpoint": volume.path.to_string_lossy(),
        "Name": volume.name,
        "Options": null,
        "Scope": "local",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn volume(name: &str, used_by: &[&str]) -> VolumeInfo {
        VolumeInfo {
            name: name.to_string(),
            path: PathBuf::from(format!("/var/lib/nspawn/volumes/{name}")),
            used_by: used_by.iter().map(|u| u.to_string()).collect(),
            created: 1_700_000_000,
        }
    }

    #[test]
    fn volumes_are_selected_as_the_engine_filters_them() {
        let volumes = [volume("data", &["web"]), volume("cache", &[])];
        let names = |text: &str| -> Vec<String> {
            let filters = Filters::parse(Some(text)).unwrap();
            select(&volumes, &filters)
                .unwrap()
                .iter()
                .map(|v| v.name.clone())
                .collect()
        };
        assert_eq!(names("{}"), ["data", "cache"]);
        assert_eq!(names(r#"{"dangling":["true"]}"#), ["cache"]);
        assert_eq!(names(r#"{"dangling":["false"]}"#), ["data"]);
        assert_eq!(names(r#"{"name":["ach"]}"#), ["cache"]);
        assert_eq!(names(r#"{"driver":["local"]}"#).len(), 2);
        assert!(names(r#"{"driver":["nfs"]}"#).is_empty());
        assert!(names(r#"{"label":["a"]}"#).is_empty());
        let bad = Filters::parse(Some(r#"{"nope":["x"]}"#)).unwrap();
        assert!(select(&volumes, &bad).is_err());
    }

    #[test]
    fn shapes_against_docker() {
        use crate::dockerapi::shapes::{diff, golden};
        // The dangling one: labels are not kept here.
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/volumes-dangling.body"
        ));
        let ours = json!({ "Volumes": [json(&volume("loose", &[]))], "Warnings": null });
        assert_eq!(diff(&ours, &docker), (vec![], vec![]), "list");
        let docker = golden(include_str!(
            "../../tests/dockerapi/golden/volume-missing.body"
        ));
        assert_eq!(docker["message"], "get nope: no such volume");
    }

    #[test]
    fn a_volume_is_the_local_driver_s() {
        let v = json(&volume("data", &[]));
        assert_eq!(v["Driver"], "local");
        assert_eq!(v["Scope"], "local");
        assert_eq!(v["Mountpoint"], "/var/lib/nspawn/volumes/data");
        assert_eq!(v["Name"], "data");
    }
}
