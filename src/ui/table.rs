//! Table rendering.
//!
//! On a tty this draws an aligned, bordered table; off a tty it degrades to tab-separated plain
//! text, so `agit log | cut -f2` works.

use comfy_table::{Cell, ContentArrangement, Table, presets};
use unicode_width::UnicodeWidthStr;

/// Render a table.
///
/// A row whose column count does not match is padded or truncated rather than panicking — a
/// rendering problem must not take the command down.
pub fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    if !super::is_tty() {
        return plain(headers, rows);
    }
    bordered(headers, rows)
}

/// The tty form: bordered, laid out to the terminal width.
fn bordered(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut t = Table::new();
    t.load_preset(presets::UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic);
    t.set_header(
        headers
            .iter()
            .map(|h| Cell::new(h).add_attribute(comfy_table::Attribute::Bold))
            .collect::<Vec<_>>(),
    );
    for r in rows {
        let mut cells: Vec<Cell> = r.iter().map(Cell::new).collect();
        cells.resize_with(headers.len(), || Cell::new(""));
        t.add_row(cells);
    }
    t.to_string()
}

/// The non-tty form: tab-separated, no borders and no color.
fn plain(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = headers.join("\t");
    out.push('\n');
    for r in rows {
        let mut cols = r.clone();
        cols.resize(headers.len(), String::new());
        // A tab or a newline inside a cell breaks the format.
        let cleaned: Vec<String> = cols.iter().map(|c| c.replace(['\t', '\n'], " ")).collect();
        out.push_str(&cleaned.join("\t"));
        out.push('\n');
    }
    out
}

/// A `key: value` list for detail output. Keys are padded to the widest key so the values form
/// one column.
pub fn key_values(pairs: &[(&str, String)]) -> String {
    let width = pairs.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
    pairs
        .iter()
        .map(|(k, v)| {
            let pad = " ".repeat(width - k.width());
            format!("{}{pad}  {v}\n", super::dim(k))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_is_tab_separated_and_parseable() {
        let rows = vec![
            vec!["frontend".to_string(), "3".to_string()],
            vec!["api".to_string(), "0".to_string()],
        ];
        let out = plain(&["AGENT", "sessions"], &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "AGENT\tsessions");
        // The property that matters: every line carries the same column count, so a script can
        // cut -f2 without looking.
        for l in &lines {
            assert_eq!(l.split('\t').count(), 2, "column counts must match: {l}");
        }
    }

    #[test]
    fn ragged_rows_padded_not_panicking() {
        let rows = vec![
            vec!["a".to_string()],
            vec!["b".to_string(), "1".to_string(), "extra".to_string()],
        ];
        for l in plain(&["X", "Y"], &rows).lines() {
            assert_eq!(l.split('\t').count(), 2);
        }
    }

    #[test]
    fn embedded_tabs_cleaned() {
        let rows = vec![vec!["a\tb".to_string(), "c\nd".to_string()]];
        let data = plain(&["X", "Y"], &rows)
            .lines()
            .nth(1)
            .unwrap()
            .to_string();
        assert_eq!(data.split('\t').count(), 2);
        assert!(!data.contains('\n'));
    }

    #[test]
    fn bordered_rows_with_ansi_keep_the_right_border_aligned() {
        // `status` colors states such as "never pushed".  The table must measure
        // the visible text, not the bytes in the ANSI escape sequences.
        let rows = vec![vec![
            "alice/agent".to_string(),
            "\u{1b}[33mnever pushed\u{1b}[39m".to_string(),
        ]];
        let out = bordered(&["AGENT", "state"], &rows);
        let widths: Vec<usize> = out.lines().map(visible_width).collect();
        assert!(!widths.is_empty());
        assert!(
            widths.iter().all(|width| *width == widths[0]),
            "every line must have the same visible width: {widths:?}\n{out}"
        );
    }

    #[test]
    fn key_values_aligns_wide_unicode_keys() {
        // CJK width fixture: the padding arithmetic below depends on a key two columns wide
        // per character.
        let out = key_values(&[("路径", "one".into()), ("id", "two".into())]);
        let lines: Vec<&str> = out.lines().collect();
        let value_columns: Vec<usize> = lines
            .iter()
            .zip(["one", "two"])
            .map(|(line, value)| {
                let index = line.find(value).expect("every line carries its value");
                line[..index].width()
            })
            .collect();
        assert_eq!(value_columns, vec![6, 6]);
    }

    /// Test-only width helper for ASCII table fixtures.  It deliberately strips
    /// SGR sequences so the assertion observes what a terminal displays.
    fn visible_width(line: &str) -> usize {
        let mut width = 0;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                if chars.next() == Some('[') {
                    for code in chars.by_ref() {
                        if code.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                width += 1;
            }
        }
        width
    }
}
