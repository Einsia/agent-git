// A renderer emits only changed cells. Replaying cursor movements keeps unchanged characters
// in a status line visible to assertions when adjacent characters arrive in another frame.
pub fn screen_text(output: &str, csi: &regex::Regex, rows: usize, columns: usize) -> String {
    assert!(rows > 0 && columns > 0);
    let mut cells = vec![vec![' '; columns]; rows];
    let (mut row, mut col, mut previous) = (0usize, 0usize, 0usize);
    let mut primary = None;
    for escape in csi.find_iter(output) {
        paint_text(
            &output[previous..escape.start()],
            &mut cells,
            &mut row,
            &mut col,
        );
        let sequence = escape.as_str();
        let params = &sequence[2..sequence.len() - 1];
        let command = sequence.as_bytes().last().copied().unwrap();
        // Giving the terminal back restores the primary cells and cursor. Ordinary output
        // must not inherit the final cursor position or stale labels of the alternate screen.
        if params == "?1049" {
            match command {
                b'h' if primary.is_none() => {
                    primary = Some((cells, row, col));
                    cells = vec![vec![' '; columns]; rows];
                    (row, col) = (0, 0);
                }
                b'l' => {
                    if let Some((saved, saved_row, saved_col)) = primary.take() {
                        (cells, row, col) = (saved, saved_row, saved_col);
                    }
                }
                _ => {}
            }
            previous = escape.end();
            continue;
        }
        let values: Vec<usize> = params.split(';').map(|n| n.parse().unwrap_or(0)).collect();
        let first = values.first().copied().unwrap_or(0);
        match command {
            b'H' | b'f' => {
                row = first.max(1).saturating_sub(1).min(cells.len() - 1);
                col = values
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .saturating_sub(1)
                    .min(columns - 1);
            }
            b'A' => row = row.saturating_sub(first.max(1)),
            b'B' => row = (row + first.max(1)).min(cells.len() - 1),
            b'C' => col = (col + first.max(1)).min(columns - 1),
            b'D' => col = col.saturating_sub(first.max(1)),
            b'G' => col = first.max(1).saturating_sub(1).min(columns - 1),
            b'J' if first == 2 => cells.iter_mut().for_each(|line| line.fill(' ')),
            b'K' => match first {
                0 => cells[row][col.min(columns - 1)..].fill(' '),
                1 => cells[row][..=col.min(columns - 1)].fill(' '),
                2 => cells[row].fill(' '),
                _ => {}
            },
            _ => {}
        }
        previous = escape.end();
    }
    paint_text(&output[previous..], &mut cells, &mut row, &mut col);
    cells
        .into_iter()
        .map(|line| line.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn paint_text(text: &str, cells: &mut [Vec<char>], row: &mut usize, col: &mut usize) {
    for character in text.chars() {
        match character {
            '\r' => *col = 0,
            '\n' => {
                if *row + 1 < cells.len() {
                    *row += 1;
                } else {
                    cells.rotate_left(1);
                    cells.last_mut().unwrap().fill(' ');
                }
            }
            '\u{8}' => *col = col.saturating_sub(1),
            c if !c.is_control() => {
                if *col >= cells[0].len() {
                    *col = 0;
                    *row = (*row + 1).min(cells.len() - 1);
                }
                cells[*row][*col] = c;
                *col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            }
            _ => {}
        }
    }
}

// A checkpoint compares cells, not byte offsets: a wide character elsewhere in the line
// must not turn an unchanged instruction into evidence of a new interaction.
pub fn changed_text_present(current: &str, previous: Option<&str>, text: &str) -> bool {
    let wanted: Vec<_> = text.chars().collect();
    assert!(!wanted.is_empty());
    let before: Vec<Vec<char>> = previous
        .into_iter()
        .flat_map(str::lines)
        .map(|line| line.chars().collect())
        .collect();
    for (row, line) in current.lines().enumerate() {
        let cells: Vec<_> = line.chars().collect();
        for (column, candidate) in cells.windows(wanted.len()).enumerate() {
            if candidate == wanted.as_slice()
                && before
                    .get(row)
                    .and_then(|line| line.get(column..column + wanted.len()))
                    != Some(wanted.as_slice())
            {
                return true;
            }
        }
    }
    false
}

// A previous screen's restoration cannot prove that the current screen has been released.
pub fn control_present_after(output: &[u8], after: usize, sequence: &[u8]) -> bool {
    assert!(!sequence.is_empty());
    output[after..]
        .windows(sequence.len())
        .any(|window| window == sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(output: &[u8], columns: usize) -> String {
        let csi = regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap();
        screen_text(&String::from_utf8_lossy(output), &csi, 24, columns)
    }

    #[test]
    fn cursor_addressed_words_form_visible_text_across_transport_chunks() {
        let output = concat!(
            "\x1b[2J\x1b[1;1Hagit\x1b[1;6Himport\x1b[1;13H·",
            "\x1b[1;15Hchoose\x1b[1;22Ha\x1b[1;24Hruntime\x1b[1;32Hscope",
        );
        assert!(!output.contains("choose a runtime scope"));
        for split in 0..=output.len() {
            let mut captured = output.as_bytes()[..split].to_vec();
            let _ = render(&captured, 100);
            captured.extend_from_slice(&output.as_bytes()[split..]);
            let screen = render(&captured, 100);
            assert!(
                screen
                    .lines()
                    .next()
                    .unwrap()
                    .starts_with("agit import · choose a runtime scope")
            );
        }
    }

    #[test]
    fn differential_redraw_keeps_unchanged_characters_but_discards_overwritten_text() {
        let initial = "\x1b[24;1H enter name · s skip · a projects · r runtime · tab repo · q";
        let delta = concat!(
            "\x1b[24;2Htyp\x1b[24;6H br\x1b[24;10Hnch \x1b[24;15H enter",
            "\x1b[24;22Hadopt   \x1b[24;31Hsc stop\x1b[24;39Hedi\x1b[24;44Hng",
            "\x1b[24;47H              ",
        );
        assert!(!delta.contains("esc stop editing"));
        assert!(!render(delta.as_bytes(), 60).contains("esc stop editing"));
        let before = render(initial.as_bytes(), 60);
        let output = format!("{initial}{delta}");
        let editing = render(output.as_bytes(), 60);
        assert!(changed_text_present(
            &editing,
            Some(&before),
            "esc stop editing"
        ));
        assert!(!editing.contains("s skip"));
        let replaced = render(
            format!("{output}\x1b[24;1H\x1b[2K enter name · s skip").as_bytes(),
            60,
        );
        assert!(changed_text_present(&replaced, Some(&editing), "s skip"));
        assert!(!replaced.contains("esc stop editing"));
    }

    #[test]
    fn checkpoint_rejects_an_unchanged_hint_until_the_action_has_a_new_visible_result() {
        let initial = "\x1b[1;1Hsaved runtime.default\x1b[24;1Hu unset";
        let before = render(initial.as_bytes(), 60);
        let styled = render(format!("{initial}\x1b[?25l\x1b[39m").as_bytes(), 60);
        assert!(!changed_text_present(&styled, Some(&before), "unset"));
        let unchanged = render(format!("{initial}\x1b[2;1Hother redraw").as_bytes(), 60);
        assert!(!changed_text_present(&unchanged, Some(&before), "unset"));
        let changed = render(
            format!("{initial}\x1b[1;1Hunset runtime.default").as_bytes(),
            60,
        );
        assert!(changed_text_present(&changed, Some(&before), "unset"));
        assert!(!changed.contains("saved runtime.default"));
    }

    #[test]
    fn alternate_screen_exit_restores_primary_output_and_its_cursor() {
        let primary = "\x1b[3;1Hprimary prompt: ";
        let alternate = format!("{primary}\x1b[?1049h\x1b[2J\x1b[24;58HOLD");
        assert!(!render(alternate.as_bytes(), 60).contains("primary prompt"));
        let result = format!("{alternate}\x1b[?1049l\x1b[?25h✓ initialized local/tui-created\r\n");
        let restored = render(result.as_bytes(), 60);
        assert!(
            restored
                .lines()
                .nth(2)
                .unwrap()
                .starts_with("primary prompt: ✓ initialized local/tui-created")
        );
        assert!(!restored.contains("OLD"));
        let reentered = format!("{result}\x1b[?1049h\x1b[1;1Hnext picker");
        let current = render(reentered.as_bytes(), 60);
        assert!(current.contains("next picker"));
        assert!(!current.contains("local/tui-created"));
        assert_eq!(
            render(format!("{reentered}\x1b[?1049l").as_bytes(), 60),
            restored
        );
    }

    #[test]
    fn raw_checkpoint_requires_the_current_screen_to_leave() {
        let leave = b"\x1b[?1049l";
        let mut output = b"\x1b[?1049hfirst picker\x1b[?1049l\x1b[?1049hshare settings".to_vec();
        let after = output.len();
        assert!(control_present_after(&output, 0, leave));
        assert!(!control_present_after(&output, after, leave));
        output.extend_from_slice(b"\x1b[?25l\x1b[1;1Hunrelated redraw");
        assert!(!control_present_after(&output, after, leave));
        output.extend_from_slice(&leave[..leave.len() - 1]);
        assert!(!control_present_after(&output, after, leave));
        output.extend_from_slice(&leave[leave.len() - 1..]);
        assert!(control_present_after(&output, after, leave));
    }
}
