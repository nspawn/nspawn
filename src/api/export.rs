//! export: a machine's whole tree as a tar, owners as its image has them. Its volumes,
//! binds and tmpfs are not part of the tree. A running machine goes on running: an entry
//! that goes before it is read is left out, and a file whose size changes while it is
//! read goes in at the size it had when it was listed, cut or filled with zeros.

use std::io::{BufWriter, Write};
use std::os::fd::OwnedFd;

use anyhow::{Context as _, Result};

use crate::api::{line, note, snapshot, Context, Report};
use crate::build::layer;

/// Writes the tree of machine `name` to `out`.
pub async fn export(ctx: &Context, name: &str, out: OwnedFd, report: Report<'_>) -> Result<()> {
    crate::api::require_root("export")?;
    let snap = snapshot::open(ctx, name, "export").await?;
    line(report, format!("exporting {name}"));
    let tree = snap.tree;
    let changed = crate::api::copy::on_thread(move || {
        let mut out = BufWriter::new(std::fs::File::from(out));
        let (_, changed) = layer::write_tree(&tree, &mut out)?;
        out.flush().context("writing the tar")?;
        Ok(changed)
    })
    .await
    .with_context(|| format!("exporting {name}"))?;
    if !changed.is_empty() {
        note(
            report,
            format!(
                "note: changed while they were read (the ones gone left out): {}",
                changed.join(", ")
            ),
        );
    }
    Ok(())
}
