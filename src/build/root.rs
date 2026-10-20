//! The root a build step works in: an overlay of the layers so far with a fresh upper
//! directory, mounted by a transient mount unit of systemd's, so that a step left behind
//! by a service that went away is still a unit to stop.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use zbus::zvariant::{OwnedValue, Value};

use crate::systemd::Systemd;

/// A mounted step root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRoot {
    /// What the step sees as /.
    pub merged: PathBuf,
    /// What it changed, for the layer.
    pub upper: PathBuf,
    pub unit: String,
}

/// The mount options: `lowers` bottom first, each once (the topmost occurrence kept), an
/// empty directory under them for a stage from scratch. Without metacopy or redirects
/// every change in the upper directory is the file itself, which the layer is made of.
pub fn options(lowers: &[PathBuf], empty: &Path, upper: &Path, work: &Path) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut top_first: Vec<&Path> = Vec::new();
    for dir in lowers.iter().rev() {
        if seen.insert(dir.as_path()) {
            top_first.push(dir);
        }
    }
    top_first.push(empty);
    let lower: Vec<String> = top_first
        .iter()
        .map(|d| format!("lowerdir+={}", d.display()))
        .collect();
    format!(
        "{},upperdir={},workdir={},metacopy=off,redirect_dir=off,index=off",
        lower.join(","),
        upper.display(),
        work.display()
    )
}

/// Mounts a step root under `dir` (which it makes: merged, upper, work, empty).
pub async fn mount(sd: &Systemd, dir: &Path, lowers: &[PathBuf]) -> Result<StepRoot> {
    let (merged, upper, work, empty) = (
        dir.join("merged"),
        dir.join("upper"),
        dir.join("work"),
        dir.join("empty"),
    );
    for d in [&merged, &upper, &work, &empty] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let where_ = merged.to_string_lossy().into_owned();
    let unit = crate::unitname::mount_unit_for(&where_);
    let value = |v: Value<'static>| OwnedValue::try_from(v).expect("a plain value");
    let properties = vec![
        (
            "Description".to_string(),
            value(Value::from(format!("nspawn build step {}", dir.display()))),
        ),
        ("What".to_string(), value(Value::from("overlay"))),
        ("Where".to_string(), value(Value::from(where_.clone()))),
        ("Type".to_string(), value(Value::from("overlay"))),
        (
            "Options".to_string(),
            value(Value::from(options(lowers, &empty, &upper, &work))),
        ),
        // Gone from systemd's units once unmounted, failed or not.
        (
            "CollectMode".to_string(),
            value(Value::from("inactive-or-failed")),
        ),
    ];
    sd.start_transient_wait(&unit, properties)
        .await
        .with_context(|| format!("mounting the step root {where_}"))?;
    Ok(StepRoot {
        merged,
        upper,
        unit,
    })
}

/// Unmounts a step root; the upper directory stays for the layer.
pub async fn unmount(sd: &Systemd, root: &StepRoot) -> Result<()> {
    sd.stop_unit(&root.unit)
        .await
        .with_context(|| format!("unmounting {}", root.merged.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowers_go_top_first_once_with_the_empty_one_last() {
        let l = |p: &str| PathBuf::from(p);
        assert_eq!(
            options(
                &[l("/l/a"), l("/l/b"), l("/l/a"), l("/l/c")],
                Path::new("/s/empty"),
                Path::new("/s/upper"),
                Path::new("/s/work"),
            ),
            "lowerdir+=/l/c,lowerdir+=/l/a,lowerdir+=/l/b,lowerdir+=/s/empty,upperdir=/s/upper,workdir=/s/work,metacopy=off,redirect_dir=off,index=off"
        );
    }
}
