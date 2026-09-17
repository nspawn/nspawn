//! Table rendering shared by the listing commands.

use std::io::IsTerminal;

use comfy_table::{presets::NOTHING, Cell, ContentArrangement, Table};

pub fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let mut t = Table::new();
    t.load_style(NOTHING)
        .set_content_arrangement(if std::io::stdout().is_terminal() {
            ContentArrangement::Dynamic
        } else {
            // One line per row when piped, so that scripts can grep the output.
            ContentArrangement::Disabled
        })
        .set_header(headers.iter().map(Cell::new));
    for row in rows {
        t.add_row(row.into_iter().map(Cell::new));
    }
    t.to_string()
}

/// Human readable byte count.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(89_641_584), "85.5 MiB");
    }
}
