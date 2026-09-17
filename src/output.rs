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

/// "12s", "5m", "3h", "2d": how long ago, in the largest sensible unit.
pub fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(human_duration(5), "5s");
        assert_eq!(human_duration(125), "2m");
        assert_eq!(human_duration(7300), "2h");
        assert_eq!(human_duration(200_000), "2d");
    }

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(89_641_584), "85.5 MiB");
    }
}
