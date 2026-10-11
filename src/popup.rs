//! The suggestion popup, as bytes for the user's terminal.
//!
//! The popup occupies the rows directly below the cursor. Everything here is positioned
//! relative to the cursor: row moves are relative and only columns are absolute, so the
//! drawing stays correct even if our model of the screen has drifted vertically from the real
//! terminal. Every function leaves the cursor exactly where it found it.

use std::fmt::Write as _;

use unicode_width::UnicodeWidthChar;
use vt100::{Cell, Color, Screen};

use crate::engine::Suggestion;

/// Most suggestions visible at once; the list scrolls beyond that.
pub const MAX_ROWS: u16 = 8;

const MAX_LABEL: u16 = 36;
const MAX_DESCRIPTION: u16 = 44;
/// Terminals smaller than this get no popup.
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 3;

const STYLE_ROW: &str = "\x1b[0;38;5;252;48;5;237m";
const STYLE_ROW_DESCRIPTION: &str = "\x1b[38;5;245m";
const STYLE_SELECTED: &str = "\x1b[0;1;38;5;231;48;5;25m";
const STYLE_SELECTED_DESCRIPTION: &str = "\x1b[22;38;5;252m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Leftmost column, counted from zero.
    pub x: u16,
    pub width: u16,
    /// Rows used, starting on the row below the cursor.
    pub height: u16,
    label_width: u16,
}

pub struct Frame<'a> {
    pub items: &'a [Suggestion],
    pub selected: Option<usize>,
    /// Index of the first visible item.
    pub scroll: usize,
}

/// Decides where a popup for `items` goes, or that there is no room for one.
///
/// `token_width` is the displayed width of the token being completed, so the labels can line up
/// with where it starts.
pub fn layout(items: &[Suggestion], token_width: u16, cursor_col: u16, rows: u16, cols: u16) -> Option<Geometry> {
    if items.is_empty() || cols < MIN_COLS || rows < MIN_ROWS {
        return None;
    }
    let widest = |text: fn(&Suggestion) -> &str, cap: u16| -> u16 {
        items
            .iter()
            .map(|item| display_width(text(item)))
            .max()
            .unwrap_or(0)
            .min(cap)
    };
    let label = widest(|item| item.label.as_str(), MAX_LABEL);
    let description = widest(|item| item.description.as_str(), MAX_DESCRIPTION);
    // One column of padding on each side of the label, and of the description if there is one.
    let width = (label + 2 + if description > 0 { description + 2 } else { 0 }).min(cols);
    Some(Geometry {
        x: cursor_col.saturating_sub(token_width + 1).min(cols - width),
        width,
        height: u16::try_from(items.len())
            .unwrap_or(u16::MAX)
            .min(MAX_ROWS)
            .min(rows - 1),
        label_width: label.min(width - 2),
    })
}

/// Columns `text` takes up on screen, as the popup renders it.
pub fn display_width(text: &str) -> u16 {
    u16::try_from(text.chars().map(char_width).sum::<usize>()).unwrap_or(u16::MAX)
}

/// Control characters are shown as a space.
fn char_width(c: char) -> usize {
    if c.is_control() { 1 } else { c.width().unwrap_or(0) }
}

/// Makes sure `height` rows exist below the cursor, scrolling the screen up if they do not.
///
/// This is the one thing the popup does that changes the screen for good, so the caller must
/// feed the same bytes to its model.
pub fn reserve(out: &mut Vec<u8>, height: u16) {
    out.extend(std::iter::repeat_n(b'\n', usize::from(height)));
    out.extend_from_slice(format!("\x1b[{height}A").as_bytes());
}

/// Draws the popup. `screen` is the model of the terminal as it is without the popup.
pub fn draw(out: &mut Vec<u8>, geometry: Geometry, frame: &Frame, screen: &Screen) {
    let mut text = String::from("\x1b[?25l");
    let label_width = usize::from(geometry.label_width);
    let rest = usize::from(geometry.width) - label_width - 2;
    // Always exactly `height` rows, so the way back up to the cursor is known.
    for index in (frame.scroll..).take(usize::from(geometry.height)) {
        let (label, description) = frame
            .items
            .get(index)
            .map_or(("", ""), |item| (item.label.as_str(), item.description.as_str()));
        let (style, description_style) = match frame.selected == Some(index) {
            true => (STYLE_SELECTED, STYLE_SELECTED_DESCRIPTION),
            false => (STYLE_ROW, STYLE_ROW_DESCRIPTION),
        };
        let _ = write!(
            text,
            "\x1b[1B\x1b[{}G{style} {} ",
            geometry.x + 1,
            fit(label, label_width)
        );
        if rest > 0 {
            let _ = write!(text, "{description_style}{}", fit(&format!(" {description}"), rest));
        }
    }
    out.extend_from_slice(text.as_bytes());
    finish(out, geometry, screen);
}

/// Removes the popup by putting back what `screen` says was underneath it.
pub fn erase(out: &mut Vec<u8>, geometry: Geometry, screen: &Screen) {
    let (cursor_row, _) = screen.cursor_position();
    let mut text = String::from("\x1b[?25l");
    for offset in 1..=geometry.height {
        let _ = write!(text, "\x1b[1B\x1b[{}G\x1b[0m\x1b[{}X", geometry.x + 1, geometry.width);
        restore_row(&mut text, screen, cursor_row + offset, geometry);
    }
    out.extend_from_slice(text.as_bytes());
    finish(out, geometry, screen);
}

/// Blanks the popup's rectangle without consulting any model of the screen. This is the fallback
/// for when the model has failed: it loses whatever was underneath, and it borrows the
/// terminal's one saved-cursor slot, which the normal path stays clear of.
pub fn blank(out: &mut Vec<u8>, geometry: Geometry) {
    let mut text = String::from("\x1b7");
    for _ in 0..geometry.height {
        let _ = write!(text, "\x1b[1B\x1b[{}G\x1b[0m\x1b[{}X", geometry.x + 1, geometry.width);
    }
    text.push_str("\x1b8");
    out.extend_from_slice(text.as_bytes());
}

/// Returns to the cursor and puts back the terminal state our drawing changed.
fn finish(out: &mut Vec<u8>, geometry: Geometry, screen: &Screen) {
    let (_, cursor_col) = screen.cursor_position();
    out.extend_from_slice(format!("\x1b[0m\x1b[{}A\x1b[{}G", geometry.height, cursor_col + 1).as_bytes());
    out.extend_from_slice(&screen.attributes_formatted());
    if !screen.hide_cursor() {
        out.extend_from_slice(b"\x1b[?25h");
    }
}

/// Rewrites the cells of one row that the popup covered. The cursor starts at the popup's left
/// edge, on a stretch that has just been blanked.
fn restore_row(text: &mut String, screen: &Screen, row: u16, geometry: Geometry) {
    let mut skipped = 0;
    let mut style = String::new();
    let mut col = geometry.x;
    while col < geometry.x + geometry.width {
        let cell = screen.cell(row, col).filter(|cell| !is_blank(cell));
        let Some(cell) = cell else {
            skipped += 1;
            col += 1;
            continue;
        };
        if cell.is_wide_continuation() {
            // The left half lies outside the popup and was never overwritten.
            skipped += 1;
            col += 1;
            continue;
        }
        if skipped > 0 {
            let _ = write!(text, "\x1b[{skipped}C");
            skipped = 0;
        }
        let wanted = sgr(cell);
        if wanted != style {
            text.push_str(&wanted);
            style = wanted;
        }
        text.push_str(if cell.has_contents() { cell.contents() } else { " " });
        col += if cell.is_wide() { 2 } else { 1 };
    }
}

fn is_blank(cell: &Cell) -> bool {
    !cell.has_contents() && !cell.is_wide_continuation() && cell.bgcolor() == Color::Default && !cell.inverse()
}

fn sgr(cell: &Cell) -> String {
    let mut codes = String::from("\x1b[0");
    for (on, code) in [
        (cell.bold(), 1),
        (cell.dim(), 2),
        (cell.italic(), 3),
        (cell.underline(), 4),
        (cell.inverse(), 7),
    ] {
        if on {
            let _ = write!(codes, ";{code}");
        }
    }
    for (color, base) in [(cell.fgcolor(), 38), (cell.bgcolor(), 48)] {
        match color {
            Color::Default => {}
            Color::Idx(index) => {
                let _ = write!(codes, ";{base};5;{index}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(codes, ";{base};2;{r};{g};{b}");
            }
        }
    }
    codes.push('m');
    codes
}

/// Cuts or pads `text` to exactly `width` columns, marking a cut with an ellipsis.
fn fit(text: &str, width: usize) -> String {
    let fits = usize::from(display_width(text)) <= width;
    let budget = if fits { width } else { width.saturating_sub(1) };
    let mut out = String::with_capacity(width);
    let mut used = 0;
    for c in text.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        out.push(if c.is_control() { ' ' } else { c });
        used += w;
    }
    if !fits && used < width {
        out.push('…');
        used += 1;
    }
    out.extend(std::iter::repeat_n(' ', width - used));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Kind;

    fn item(label: &str, description: &str) -> Suggestion {
        Suggestion {
            label: label.to_owned(),
            description: description.to_owned(),
            insert: label.to_owned(),
            cursor_back: 0,
            kind: Kind::Subcommand,
        }
    }

    /// A stand-in for the real terminal next to the model the popup code is given.
    struct Rig {
        terminal: vt100::Parser,
        model: vt100::Parser,
    }

    impl Rig {
        fn new(rows: u16, cols: u16, shell_output: &[u8]) -> Rig {
            let mut rig = Rig {
                terminal: vt100::Parser::new(rows, cols, 0),
                model: vt100::Parser::new(rows, cols, 0),
            };
            rig.shell(shell_output);
            rig
        }

        fn shell(&mut self, bytes: &[u8]) {
            self.terminal.process(bytes);
            self.model.process(bytes);
        }

        fn show(&mut self, items: &[Suggestion], selected: Option<usize>, scroll: usize, token_width: u16) -> Geometry {
            let (rows, cols) = self.model.screen().size();
            let (_, col) = self.model.screen().cursor_position();
            let geometry = layout(items, token_width, col, rows, cols).expect("room for a popup");
            let mut reserved = Vec::new();
            reserve(&mut reserved, geometry.height);
            self.shell(&reserved);
            let mut out = Vec::new();
            draw(
                &mut out,
                geometry,
                &Frame {
                    items,
                    selected,
                    scroll,
                },
                self.model.screen(),
            );
            self.terminal.process(&out);
            geometry
        }

        fn hide(&mut self, geometry: Geometry) {
            let mut out = Vec::new();
            erase(&mut out, geometry, self.model.screen());
            self.terminal.process(&out);
        }

        fn row(&self, row: u16) -> String {
            self.terminal
                .screen()
                .rows(0, 80)
                .nth(usize::from(row))
                .unwrap()
                .trim_end()
                .to_owned()
        }

        fn assert_terminal_matches_model(&self) {
            assert_eq!(
                String::from_utf8_lossy(&self.terminal.screen().state_formatted()),
                String::from_utf8_lossy(&self.model.screen().state_formatted())
            );
        }
    }

    #[test]
    fn layout_sizes_to_content_and_lines_up_with_the_token() {
        let items = [item("checkout", "Switch branches"), item("cherry-pick", "")];
        let geometry = layout(&items, 2, 8, 24, 80).unwrap();
        // "cherry-pick" is 11 wide, "Switch branches" 15, plus four columns of padding.
        assert_eq!(geometry.width, 30);
        assert_eq!(geometry.height, 2);
        // The token starts at column 6; the label sits one padding column right of the edge.
        assert_eq!(geometry.x, 5);

        let plain = [item("main", "")];
        assert_eq!(layout(&plain, 0, 3, 24, 80).unwrap().width, 6);
    }

    #[test]
    fn layout_respects_the_terminal_size() {
        let many: Vec<Suggestion> = (0..50).map(|n| item(&format!("item{n}"), "")).collect();
        assert_eq!(layout(&many, 0, 0, 24, 80).unwrap().height, MAX_ROWS);
        assert_eq!(layout(&many, 0, 0, 5, 80).unwrap().height, 4);
        assert_eq!(layout(&many, 0, 0, 2, 80), None);
        assert_eq!(layout(&many, 0, 0, 24, 10), None);
        assert_eq!(layout(&[], 0, 0, 24, 80), None);

        // Pushed left so it does not run off the right edge.
        let geometry = layout(&many, 0, 78, 24, 80).unwrap();
        assert_eq!(geometry.x + geometry.width, 80);

        let wide = [item(&"x".repeat(100), &"y".repeat(100))];
        let geometry = layout(&wide, 0, 0, 24, 30).unwrap();
        assert_eq!((geometry.x, geometry.width), (0, 30));
    }

    #[test]
    fn draws_below_the_cursor_and_leaves_it_in_place() {
        let mut rig = Rig::new(10, 60, b"$ git ch");
        let items = [
            item("checkout", "Switch branches"),
            item("cherry-pick", "Apply commits"),
        ];
        rig.show(&items, Some(1), 0, 2);

        assert_eq!(rig.row(0), "$ git ch");
        assert_eq!(rig.row(1), "      checkout     Switch branches");
        assert_eq!(rig.row(2), "      cherry-pick  Apply commits");
        assert_eq!(rig.terminal.screen().cursor_position(), (0, 8));
        // The selected row is the highlighted one.
        assert_eq!(rig.terminal.screen().cell(2, 6).unwrap().bgcolor(), Color::Idx(25));
        assert_eq!(rig.terminal.screen().cell(1, 6).unwrap().bgcolor(), Color::Idx(237));
    }

    #[test]
    fn scrolls_the_list() {
        let mut rig = Rig::new(6, 40, b"$ x ");
        let items: Vec<Suggestion> = (0..20).map(|n| item(&format!("item{n}"), "")).collect();
        let geometry = rig.show(&items, Some(7), 5, 0);
        assert_eq!(geometry.height, 5);
        assert_eq!(rig.row(1).trim(), "item5");
        assert_eq!(rig.row(5).trim(), "item9");
    }

    #[test]
    fn erase_restores_what_was_underneath() {
        // Coloured text, a wide character and a styled blank below the prompt line.
        let below = "\r\n\x1b[1;31mred\x1b[0m plain \x1b[48;5;22m  \x1b[0m 日本語 text\r\nsecond line here";
        let mut rig = Rig::new(10, 40, format!("$ git {below}").as_bytes());
        rig.shell(b"\x1b[1;7H");
        let items = [
            item("checkout", "Switch branches"),
            item("cherry-pick", "Apply commits"),
        ];
        let geometry = rig.show(&items, None, 0, 0);
        assert!(rig.row(1).contains("checkout"));

        rig.hide(geometry);
        assert_eq!(rig.row(1), "red plain    日本語 text");
        assert_eq!(rig.row(2), "second line here");
        rig.assert_terminal_matches_model();
    }

    #[test]
    fn makes_room_at_the_bottom_of_the_screen() {
        let mut rig = Rig::new(5, 40, b"one\r\ntwo\r\nthree\r\nfour\r\n$ git ");
        assert_eq!(rig.model.screen().cursor_position(), (4, 6));
        let items = [item("add", ""), item("commit", ""), item("push", "")];
        let geometry = rig.show(&items, None, 0, 0);

        // The screen scrolled up by three rows; the prompt line is intact above the popup.
        assert_eq!(rig.row(0), "four");
        assert_eq!(rig.row(1), "$ git");
        assert_eq!(rig.row(2).trim(), "add");
        assert_eq!(rig.row(4).trim(), "push");
        assert_eq!(rig.terminal.screen().cursor_position(), (1, 6));

        rig.hide(geometry);
        assert_eq!(rig.row(2), "");
        rig.assert_terminal_matches_model();
    }

    #[test]
    fn keeps_the_shells_text_attributes() {
        let mut rig = Rig::new(10, 40, b"$ \x1b[1;32mgit ");
        let geometry = rig.show(&[item("add", "")], None, 0, 0);
        rig.hide(geometry);
        // Whatever the shell prints next is still bold green.
        rig.shell(b"x");
        let cell = rig.terminal.screen().cell(0, 6).unwrap().clone();
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), Color::Idx(2));
        rig.assert_terminal_matches_model();
    }

    #[test]
    fn blank_clears_the_popup_without_a_model() {
        let mut rig = Rig::new(10, 40, b"$ \x1b[1;32mgit ");
        let geometry = rig.show(&[item("add", "Stage"), item("commit", "Record")], Some(0), 0, 0);
        let mut out = Vec::new();
        blank(&mut out, geometry);
        rig.terminal.process(&out);
        rig.assert_terminal_matches_model();
    }

    #[test]
    fn fit_pads_cuts_and_sanitises() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 5), "abcd…");
        assert_eq!(fit("abcde", 5), "abcde");
        assert_eq!(fit("a\tb\x1b[m", 6), "a b [m");
        assert_eq!(fit("日本語", 5), "日本…");
        assert_eq!(fit("日本語", 4), "日… ");
        assert_eq!(fit("anything", 0), "");
    }
}
