use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vte::{Params, Perform};

use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Style {
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    pub text: String,
    pub style: Style,
    /// True for the trailing column of a wide character.
    pub continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: " ".into(),
            style: Style::default(),
            continuation: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub col: u16,
    pub row: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(col: u16, row: u16, width: u16, height: u16) -> Self {
        Self {
            col,
            row,
            width,
            height,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Buffer {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cursor_col: u16,
    cursor_row: u16,
    saved_cursor: (u16, u16),
    style: Style,
    pending_wrap: bool,
}

impl Buffer {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            cursor_col: 0,
            cursor_row: 0,
            saved_cursor: (0, 0),
            style: Style::default(),
            pending_wrap: false,
        }
    }

    fn index(&self, col: u16, row: u16) -> Option<usize> {
        (col < self.cols && row < self.rows)
            .then_some(row as usize * self.cols as usize + col as usize)
    }

    fn scroll_up(&mut self) {
        if self.rows == 0 {
            return;
        }
        let width = self.cols as usize;
        self.cells.rotate_left(width);
        let start = self.cells.len().saturating_sub(width);
        self.cells[start..].fill(Cell::default());
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        self.pending_wrap = false;
        if self.cursor_row + 1 >= self.rows {
            self.scroll_up();
        } else {
            self.cursor_row += 1;
        }
    }

    fn print(&mut self, ch: char) -> bool {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width == 0 {
            let previous = if self.pending_wrap {
                self.index(self.cursor_col, self.cursor_row)
            } else if self.cursor_col > 0 {
                self.index(self.cursor_col - 1, self.cursor_row)
            } else if self.cursor_row > 0 {
                self.index(self.cols.saturating_sub(1), self.cursor_row - 1)
            } else {
                None
            };
            if let Some(index) = previous {
                if self.cells[index].continuation && index > 0 {
                    self.cells[index - 1].text.push(ch);
                } else {
                    self.cells[index].text.push(ch);
                }
                return true;
            }
            return false;
        }
        if self.pending_wrap
            || self.cursor_col >= self.cols
            || (width == 2 && self.cursor_col + 1 >= self.cols)
        {
            self.newline();
        }
        if let Some(index) = self.index(self.cursor_col, self.cursor_row) {
            self.clear_wide_glyph_at(index);
            if width == 2 {
                self.clear_wide_glyph_at(index + 1);
            }
            self.cells[index] = Cell {
                text: ch.to_string(),
                style: self.style,
                continuation: false,
            };
            if width == 2 {
                if let Some(next) = self.index(self.cursor_col + 1, self.cursor_row) {
                    self.cells[next] = Cell {
                        text: String::new(),
                        style: self.style,
                        continuation: true,
                    };
                }
            }
        }
        let next_col = self.cursor_col.saturating_add(width as u16);
        if next_col >= self.cols {
            self.cursor_col = self.cols.saturating_sub(1);
            self.pending_wrap = true;
        } else {
            self.cursor_col = next_col;
        }
        true
    }

    fn clear_wide_glyph_at(&mut self, index: usize) {
        if self.cells[index].continuation {
            self.cells[index] = Cell::default();
            if index > 0 {
                self.cells[index - 1] = Cell::default();
            }
            return;
        }
        let next = index + 1;
        if next < self.cells.len()
            && next / self.cols as usize == index / self.cols as usize
            && self.cells[next].continuation
        {
            self.cells[index] = Cell::default();
            self.cells[next] = Cell::default();
        }
    }

    fn erase(&mut self, start: usize, end: usize) -> bool {
        let mut start = start;
        let mut end = end;
        if start < end && self.cells[start].continuation && start > 0 {
            start -= 1;
        }
        if end < self.cells.len() && self.cells[end].continuation {
            end += 1;
        }
        let changed = self.cells[start..end]
            .iter()
            .any(|cell| *cell != Cell::default());
        if changed {
            self.cells[start..end].fill(Cell::default());
        }
        changed
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let mut replacement = Buffer::new(cols, rows);
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let old = self.index(col, row).unwrap();
                let new = replacement.index(col, row).unwrap();
                replacement.cells[new] = self.cells[old].clone();
            }
        }
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let index = replacement.index(col, row).unwrap();
                let valid = if replacement.cells[index].continuation {
                    col > 0
                        && UnicodeWidthStr::width(replacement.cells[index - 1].text.as_str()) == 2
                } else if UnicodeWidthStr::width(replacement.cells[index].text.as_str()) == 2 {
                    col + 1 < cols && replacement.cells[index + 1].continuation
                } else {
                    true
                };
                if !valid {
                    replacement.cells[index] = Cell::default();
                }
            }
        }
        replacement.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        replacement.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        replacement.style = self.style;
        replacement.pending_wrap = self.pending_wrap;
        *self = replacement;
    }
}

#[derive(Debug)]
struct ScreenState {
    primary: Buffer,
    alternate: Option<Buffer>,
    version: u64,
}

impl ScreenState {
    fn active(&self) -> &Buffer {
        self.alternate.as_ref().unwrap_or(&self.primary)
    }
    fn active_mut(&mut self) -> &mut Buffer {
        self.alternate.as_mut().unwrap_or(&mut self.primary)
    }
}

#[derive(Debug)]
struct SharedScreen {
    state: RwLock<ScreenState>,
    changed_version: Mutex<u64>,
    changed: Condvar,
}

#[derive(Clone, Debug)]
pub struct Screen {
    inner: Arc<SharedScreen>,
    region: Option<Rect>,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        if cols == 0 || rows == 0 {
            return Err(Error::InvalidDimensions { cols, rows });
        }
        Ok(Self {
            inner: Arc::new(SharedScreen {
                state: RwLock::new(ScreenState {
                    primary: Buffer::new(cols, rows),
                    alternate: None,
                    version: 0,
                }),
                changed_version: Mutex::new(0),
                changed: Condvar::new(),
            }),
            region: None,
        })
    }

    pub fn dimensions(&self) -> (u16, u16) {
        let state = self.inner.state.read().expect("screen lock poisoned");
        let active = state.active();
        let rect = self.effective_rect(active);
        (rect.width, rect.height)
    }

    pub fn cell(&self, col: u16, row: u16) -> Option<Cell> {
        let state = self.inner.state.read().expect("screen lock poisoned");
        let active = state.active();
        let rect = self.effective_rect(active);
        if col >= rect.width || row >= rect.height {
            return None;
        }
        active
            .index(rect.col.saturating_add(col), rect.row.saturating_add(row))
            .map(|index| active.cells[index].clone())
    }

    pub fn lines(&self, preserve_width: bool) -> Vec<String> {
        let state = self.inner.state.read().expect("screen lock poisoned");
        let active = state.active();
        let rect = self.effective_rect(active);
        (0..rect.height)
            .map(|relative_row| {
                let mut line = String::new();
                for relative_col in 0..rect.width {
                    if let Some(index) = active.index(
                        rect.col.saturating_add(relative_col),
                        rect.row.saturating_add(relative_row),
                    ) {
                        let cell = &active.cells[index];
                        if !cell.continuation {
                            line.push_str(&cell.text);
                        }
                    }
                }
                if !preserve_width {
                    line = line.trim_end_matches(' ').to_string();
                }
                line
            })
            .collect()
    }

    pub fn text(&self) -> String {
        self.lines(false).join("\n")
    }

    pub fn fixed_text(&self) -> String {
        self.lines(true).join("\n")
    }

    pub fn region(&self, rect: Rect) -> Result<Self> {
        let state = self.inner.state.read().expect("screen lock poisoned");
        let active = state.active();
        let parent = self.effective_rect(active);
        if rect.width == 0
            || rect.height == 0
            || rect
                .col
                .checked_add(rect.width)
                .is_none_or(|end| end > parent.width)
            || rect
                .row
                .checked_add(rect.height)
                .is_none_or(|end| end > parent.height)
        {
            return Err(Error::OutOfBounds {
                col: rect.col,
                row: rect.row,
                cols: parent.width,
                rows: parent.height,
            });
        }
        Ok(Self {
            inner: Arc::clone(&self.inner),
            region: Some(Rect::new(
                parent.col.checked_add(rect.col).ok_or(Error::OutOfBounds {
                    col: rect.col,
                    row: rect.row,
                    cols: parent.width,
                    rows: parent.height,
                })?,
                parent.row.checked_add(rect.row).ok_or(Error::OutOfBounds {
                    col: rect.col,
                    row: rect.row,
                    cols: parent.width,
                    rows: parent.height,
                })?,
                rect.width,
                rect.height,
            )),
        })
    }

    pub fn version(&self) -> u64 {
        self.inner
            .state
            .read()
            .expect("screen lock poisoned")
            .version
    }

    pub fn wait_for_change(&self, after: u64, timeout: Duration) -> bool {
        if self.version() > after {
            return true;
        }
        let deadline = Instant::now() + timeout;
        let mut observed = self
            .inner
            .changed_version
            .lock()
            .expect("change lock poisoned");
        while *observed <= after {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, result) = self
                .inner
                .changed
                .wait_timeout(observed, remaining)
                .expect("change lock poisoned");
            observed = guard;
            if result.timed_out() {
                return *observed > after;
            }
        }
        true
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 {
            return Err(Error::InvalidDimensions { cols, rows });
        }
        let mut state = self.inner.state.write().expect("screen lock poisoned");
        if state.primary.cols == cols && state.primary.rows == rows {
            return Ok(());
        }
        state.primary.resize(cols, rows);
        if let Some(alternate) = state.alternate.as_mut() {
            alternate.resize(cols, rows);
        }
        self.mark_changed(&mut state);
        Ok(())
    }

    fn mutate(&self, operation: impl FnOnce(&mut ScreenState) -> bool) {
        let mut state = self.inner.state.write().expect("screen lock poisoned");
        if operation(&mut state) {
            self.mark_changed(&mut state);
        }
    }

    pub(crate) fn notify(&self) {
        let mut state = self.inner.state.write().expect("screen lock poisoned");
        self.mark_changed(&mut state);
    }

    fn mark_changed(&self, state: &mut ScreenState) {
        state.version = state.version.wrapping_add(1);
        *self
            .inner
            .changed_version
            .lock()
            .expect("change lock poisoned") = state.version;
        self.inner.changed.notify_all();
    }

    fn effective_rect(&self, active: &Buffer) -> Rect {
        let Some(region) = self.region else {
            return Rect::new(0, 0, active.cols, active.rows);
        };
        let col = region.col.min(active.cols);
        let row = region.row.min(active.rows);
        Rect::new(
            col,
            row,
            region.width.min(active.cols.saturating_sub(col)),
            region.height.min(active.rows.saturating_sub(row)),
        )
    }
}

pub struct Terminal {
    parser: vte::Parser,
    screen: Screen,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        Ok(Self {
            parser: vte::Parser::new(),
            screen: Screen::new(cols, rows)?,
        })
    }
    pub fn screen(&self) -> Screen {
        self.screen.clone()
    }
    pub fn advance(&mut self, bytes: &[u8]) {
        let screen = self.screen.clone();
        let mut performer = TerminalPerformer { screen };
        self.parser.advance(&mut performer, bytes);
    }
}

struct TerminalPerformer {
    screen: Screen,
}

impl TerminalPerformer {
    fn param(params: &Params, index: usize, default: u16) -> u16 {
        params
            .iter()
            .nth(index)
            .and_then(|p| p.first())
            .copied()
            .filter(|value| *value != 0)
            .unwrap_or(default)
    }

    fn set_sgr(buffer: &mut Buffer, params: &Params) {
        let values: Vec<u16> = params.iter().filter_map(|p| p.first().copied()).collect();
        let values = if values.is_empty() { vec![0] } else { values };
        let mut index = 0;
        while index < values.len() {
            match values[index] {
                0 => buffer.style = Style::default(),
                1 => buffer.style.bold = true,
                3 => buffer.style.italic = true,
                4 => buffer.style.underline = true,
                7 => buffer.style.inverse = true,
                22 => buffer.style.bold = false,
                23 => buffer.style.italic = false,
                24 => buffer.style.underline = false,
                27 => buffer.style.inverse = false,
                30..=37 => buffer.style.foreground = Color::Indexed((values[index] - 30) as u8),
                39 => buffer.style.foreground = Color::Default,
                40..=47 => buffer.style.background = Color::Indexed((values[index] - 40) as u8),
                49 => buffer.style.background = Color::Default,
                90..=97 => buffer.style.foreground = Color::Indexed((values[index] - 90 + 8) as u8),
                100..=107 => {
                    buffer.style.background = Color::Indexed((values[index] - 100 + 8) as u8)
                }
                38 | 48 if values.get(index + 1) == Some(&5) => {
                    if let Some(color) = values.get(index + 2) {
                        if values[index] == 38 {
                            buffer.style.foreground = Color::Indexed(*color as u8);
                        } else {
                            buffer.style.background = Color::Indexed(*color as u8);
                        }
                        index += 2;
                    }
                }
                38 | 48 if values.get(index + 1) == Some(&2) => {
                    if let (Some(r), Some(g), Some(b)) = (
                        values.get(index + 2),
                        values.get(index + 3),
                        values.get(index + 4),
                    ) {
                        let color = Color::Rgb(*r as u8, *g as u8, *b as u8);
                        if values[index] == 38 {
                            buffer.style.foreground = color;
                        } else {
                            buffer.style.background = color;
                        }
                        index += 4;
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }
}

impl Perform for TerminalPerformer {
    fn print(&mut self, c: char) {
        self.screen.mutate(|state| state.active_mut().print(c));
    }
    fn execute(&mut self, byte: u8) {
        self.screen.mutate(|state| {
            let buffer = state.active_mut();
            match byte {
                b'\n' | 0x0b | 0x0c => {
                    buffer.newline();
                    true
                }
                b'\r' => {
                    let changed = buffer.cursor_col != 0 || buffer.pending_wrap;
                    buffer.cursor_col = 0;
                    buffer.pending_wrap = false;
                    changed
                }
                0x08 => {
                    let before = (buffer.cursor_col, buffer.pending_wrap);
                    buffer.cursor_col = buffer.cursor_col.saturating_sub(1);
                    buffer.pending_wrap = false;
                    before != (buffer.cursor_col, buffer.pending_wrap)
                }
                b'\t' => {
                    let before = (buffer.cursor_col, buffer.pending_wrap);
                    buffer.cursor_col =
                        ((buffer.cursor_col / 8 + 1) * 8).min(buffer.cols.saturating_sub(1));
                    buffer.pending_wrap = false;
                    before != (buffer.cursor_col, buffer.pending_wrap)
                }
                _ => false,
            }
        });
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let private = intermediates == [b'?'];
        self.screen.mutate(|state| {
            if private && matches!(action, 'h' | 'l') && Self::param(params, 0, 0) == 1049 {
                return if action == 'h' {
                    let primary = &state.primary;
                    state.alternate = Some(Buffer::new(primary.cols, primary.rows));
                    true
                } else {
                    state.alternate.take().is_some()
                };
            }
            let buffer = state.active_mut();
            let before_cursor = (buffer.cursor_col, buffer.cursor_row, buffer.pending_wrap);
            let before_style = buffer.style;
            let supported = matches!(
                action,
                'A' | 'B'
                    | 'C'
                    | 'D'
                    | 'E'
                    | 'F'
                    | 'G'
                    | '`'
                    | 'H'
                    | 'f'
                    | 'J'
                    | 'K'
                    | 'm'
                    | 's'
                    | 'u'
            );
            if !supported {
                return false;
            }
            if action != 'm' && action != 's' {
                buffer.pending_wrap = false;
            }
            let cells_changed = match action {
                'A' => {
                    buffer.cursor_row = buffer.cursor_row.saturating_sub(Self::param(params, 0, 1));
                    false
                }
                'B' => {
                    buffer.cursor_row = buffer
                        .cursor_row
                        .saturating_add(Self::param(params, 0, 1))
                        .min(buffer.rows.saturating_sub(1));
                    false
                }
                'C' => {
                    buffer.cursor_col = buffer
                        .cursor_col
                        .saturating_add(Self::param(params, 0, 1))
                        .min(buffer.cols.saturating_sub(1));
                    false
                }
                'D' => {
                    buffer.cursor_col = buffer.cursor_col.saturating_sub(Self::param(params, 0, 1));
                    false
                }
                'E' => {
                    buffer.cursor_row = buffer
                        .cursor_row
                        .saturating_add(Self::param(params, 0, 1))
                        .min(buffer.rows.saturating_sub(1));
                    buffer.cursor_col = 0;
                    false
                }
                'F' => {
                    buffer.cursor_row = buffer.cursor_row.saturating_sub(Self::param(params, 0, 1));
                    buffer.cursor_col = 0;
                    false
                }
                'G' | '`' => {
                    buffer.cursor_col = Self::param(params, 0, 1)
                        .saturating_sub(1)
                        .min(buffer.cols.saturating_sub(1));
                    false
                }
                'H' | 'f' => {
                    buffer.cursor_row = Self::param(params, 0, 1)
                        .saturating_sub(1)
                        .min(buffer.rows.saturating_sub(1));
                    buffer.cursor_col = Self::param(params, 1, 1)
                        .saturating_sub(1)
                        .min(buffer.cols.saturating_sub(1));
                    false
                }
                'J' => {
                    let mode = Self::param(params, 0, 0);
                    if mode == 2 || mode == 3 {
                        buffer.erase(0, buffer.cells.len())
                    } else if mode == 0 {
                        if let Some(start) = buffer.index(buffer.cursor_col, buffer.cursor_row) {
                            buffer.erase(start, buffer.cells.len())
                        } else {
                            false
                        }
                    } else if mode == 1 {
                        if let Some(end) = buffer.index(buffer.cursor_col, buffer.cursor_row) {
                            buffer.erase(0, end + 1)
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                'K' => {
                    let mode = Self::param(params, 0, 0);
                    let row_start = buffer.index(0, buffer.cursor_row).unwrap();
                    let cursor = buffer.index(buffer.cursor_col, buffer.cursor_row).unwrap();
                    let row_end = row_start + buffer.cols as usize;
                    match mode {
                        1 => buffer.erase(row_start, cursor + 1),
                        2 => buffer.erase(row_start, row_end),
                        _ => buffer.erase(cursor, row_end),
                    }
                }
                'm' => {
                    Self::set_sgr(buffer, params);
                    false
                }
                's' => {
                    buffer.saved_cursor = (buffer.cursor_col, buffer.cursor_row);
                    false
                }
                'u' => {
                    (buffer.cursor_col, buffer.cursor_row) = buffer.saved_cursor;
                    false
                }
                _ => false,
            };
            cells_changed
                || before_cursor != (buffer.cursor_col, buffer.cursor_row, buffer.pending_wrap)
                || before_style != buffer.style
        });
    }
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        self.screen.mutate(|state| {
            let buffer = state.active_mut();
            match byte {
                b'7' => {
                    buffer.saved_cursor = (buffer.cursor_col, buffer.cursor_row);
                    false
                }
                b'8' => {
                    let before = (buffer.cursor_col, buffer.cursor_row);
                    (buffer.cursor_col, buffer.cursor_row) = buffer.saved_cursor;
                    before != (buffer.cursor_col, buffer.cursor_row)
                }
                b'D' | b'E' => {
                    buffer.newline();
                    true
                }
                b'c' => {
                    let cols = buffer.cols;
                    let rows = buffer.rows;
                    let changed = *buffer != Buffer::new(cols, rows);
                    *buffer = Buffer::new(cols, rows);
                    changed
                }
                _ => false,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_screen_and_bounds() {
        let screen = Screen::new(3, 2).unwrap();
        assert_eq!(screen.fixed_text(), "   \n   ");
        assert_eq!(screen.cell(0, 0).unwrap(), Cell::default());
        assert!(screen.cell(3, 0).is_none());
    }

    #[test]
    fn chunked_and_combined_input_match() {
        let mut combined = Terminal::new(10, 2).unwrap();
        combined.advance("héllo\n世界".as_bytes());
        let mut chunked = Terminal::new(10, 2).unwrap();
        for byte in "héllo\n世界".as_bytes() {
            chunked.advance(&[*byte]);
        }
        assert_eq!(
            combined.screen().fixed_text(),
            chunked.screen().fixed_text()
        );
    }

    #[test]
    fn cursor_erase_style_and_alternate_screen() {
        let mut terminal = Terminal::new(8, 2).unwrap();
        terminal.advance(b"primary\x1b[?1049h\x1b[31;1mred\x1b[0m");
        assert_eq!(
            terminal.screen().cell(0, 0).unwrap().style.foreground,
            Color::Indexed(1)
        );
        assert!(terminal.screen().cell(0, 0).unwrap().style.bold);
        terminal.advance(b"\x1b[2Jx\x1b[?1049l");
        assert!(terminal.screen().text().starts_with("primary"));
    }

    #[test]
    fn wide_and_combining_characters_have_stable_cells() {
        let mut terminal = Terminal::new(8, 1).unwrap();
        terminal.advance("界e\u{301}x".as_bytes());
        let screen = terminal.screen();
        assert_eq!(screen.cell(0, 0).unwrap().text, "界");
        assert!(screen.cell(1, 0).unwrap().continuation);
        assert_eq!(screen.cell(2, 0).unwrap().text, "e\u{301}");
        assert_eq!(screen.cell(3, 0).unwrap().text, "x");
    }

    #[test]
    fn overwriting_wide_characters_clears_both_cells() {
        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.advance("界".as_bytes());
        terminal.advance(b"\rx");
        assert_eq!(terminal.screen().fixed_text(), "x   ");

        terminal.advance(b"\r");
        terminal.advance("界".as_bytes());
        terminal.advance(b"\x1b[1Dy");
        assert_eq!(terminal.screen().fixed_text(), " y  ");
    }

    #[test]
    fn regions_clip_output() {
        let mut terminal = Terminal::new(5, 2).unwrap();
        terminal.advance(b"abcde12345");
        assert_eq!(
            terminal
                .screen()
                .region(Rect::new(1, 0, 3, 2))
                .unwrap()
                .fixed_text(),
            "bcd\n234"
        );
    }

    #[test]
    fn regions_remain_safe_and_clamp_after_resize() {
        let screen = Screen::new(5, 3).unwrap();
        let region = screen.region(Rect::new(2, 1, 3, 2)).unwrap();
        screen.resize(3, 2).unwrap();
        assert_eq!(region.dimensions(), (1, 1));
        assert_eq!(region.fixed_text(), " ");
        screen.resize(1, 1).unwrap();
        assert_eq!(region.dimensions(), (0, 0));
        assert_eq!(region.fixed_text(), "");
        assert!(region.cell(0, 0).is_none());
    }

    #[test]
    fn erase_expands_across_wide_character_boundaries() {
        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.advance("界x".as_bytes());
        terminal.advance(b"\r\x1b[1C\x1b[K");
        assert_eq!(terminal.screen().fixed_text(), "    ");

        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.advance("x界".as_bytes());
        terminal.advance(b"\r\x1b[1C\x1b[1K");
        assert_eq!(terminal.screen().fixed_text(), "    ");
    }

    #[test]
    fn resize_clears_a_wide_character_cut_by_the_new_edge() {
        let mut terminal = Terminal::new(4, 1).unwrap();
        terminal.advance("x界".as_bytes());
        terminal.screen().resize(2, 1).unwrap();
        assert_eq!(terminal.screen().fixed_text(), "x ");
        assert_eq!(terminal.screen().cell(1, 0).unwrap(), Cell::default());
    }

    #[test]
    fn overflowing_regions_are_rejected() {
        let screen = Screen::new(5, 2).unwrap();
        assert!(matches!(
            screen.region(Rect::new(u16::MAX, 0, 2, 1)),
            Err(Error::OutOfBounds { .. })
        ));
    }
}
