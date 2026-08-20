//! Read the user's in-progress draft out of an agent pane.
//!
//! Event delivery has to put a message into an input box the user may be
//! typing in. To do that without losing their work OMAR captures the draft,
//! clears the box, delivers, then pastes the draft back.
//!
//! The capture is a screen scrape, so it is only ever as good as what the
//! backend renders. Two rules keep that honest:
//!
//! 1. **Anchor on the caret, not on a glyph.** `#{cursor_y}` is the row the
//!    user is typing on, whatever the backend draws around it. Scanning
//!    bottom-up for a prompt glyph instead finds autocomplete menus, which
//!    every TUI here renders *below* the input box. Backends that hide the
//!    caret (`#{cursor_flag}` is 0 — cursor-agent) fall back to the glyph,
//!    bounded by the input box's own borders.
//! 2. **Say "I don't know" out loud.** Anything unparseable, and anything the
//!    backend has already replaced with a summary token (`[Pasted text #1 +30
//!    lines]`), is [`PaneInput::Unknown`]. The caller must not clear a pane it
//!    cannot restore, so `Unknown` defers delivery instead.

/// Where the terminal caret is, when the backend leaves it visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Caret {
    /// Row within the visible pane, 0-indexed.
    pub row: usize,
    /// Column within that row, 0-indexed, counted in terminal cells.
    pub col: usize,
}

/// What the input box holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneInput {
    /// Nothing worth preserving — empty, or only the backend's placeholder.
    Empty,
    /// A draft the user typed. Newlines are preserved.
    Draft(String),
    /// Unreadable, or holding content the screen cannot reconstruct. Callers
    /// must leave the pane alone.
    Unknown(&'static str),
}

/// How a backend draws its input box.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    /// The prompt marker on the first row of the draft.
    pub glyph: &'static str,
    /// Left box border, for backends that draw one instead of a bare glyph.
    pub border: Option<char>,
}

impl Shape {
    pub(crate) fn for_backend(backend: &str) -> Option<Shape> {
        let shape = match backend {
            "claude" => Shape {
                glyph: "❯",
                border: None,
            },
            "codex" => Shape {
                glyph: "›",
                border: None,
            },
            // The live TUI uses U+003E, not the `*` an earlier version of this
            // code assumed.
            "agy" => Shape {
                glyph: ">",
                border: None,
            },
            "cursor" => Shape {
                glyph: "→",
                border: None,
            },
            // opencode draws a left-only box border and no prompt glyph.
            "opencode" => Shape {
                glyph: "",
                border: Some('┃'),
            },
            _ => return None,
        };
        Some(shape)
    }
}

/// Tokens a backend renders in place of content it is no longer showing.
/// The real text is not on screen in any form, so it cannot be restored.
const UNRECOVERABLE: &[&str] = &["[Pasted text #", "[Pasted Content ", "[Pasted ~"];

/// Rendered when a draft is taller than the input box and has scrolled inside
/// it — the rows above are not on screen.
const SCROLLED_MARKER: &str = " more lines";

pub(crate) fn extract(
    shape: Shape,
    visible_ansi: &str,
    caret: Option<Caret>,
    pane_width: Option<usize>,
) -> PaneInput {
    let rows: Vec<Row> = visible_ansi.lines().map(Row::parse).collect();
    if rows.is_empty() {
        return PaneInput::Unknown("empty capture");
    }

    let Some(start) = find_start_row(&shape, &rows, caret) else {
        return PaneInput::Unknown("no input row found");
    };

    // Text begins one space past the glyph (or past the border). Continuation
    // rows line up under it, which is what lets us find them without a
    // per-backend indent constant.
    let text_col = rows[start].text_col(&shape);
    let end = find_end_row(&shape, &rows, start, text_col);

    let mut lines = Vec::new();
    for (offset, row) in rows[start..=end].iter().enumerate() {
        let cut = text_col;
        let Some(text) = row.after_col(cut) else {
            return PaneInput::Unknown("input row shorter than its indent");
        };

        // Check before any styling is stripped: some backends render the
        // collapsed-paste token dimmed, which would otherwise erase the very
        // evidence that the draft cannot be restored.
        if UNRECOVERABLE.iter().any(|token| text.contains(token)) {
            return PaneInput::Unknown("backend collapsed the draft to a summary token");
        }
        // Also before stripping: the rows this stands for are off screen, and
        // on some backends the marker itself is dimmed.
        if text.trim_start().starts_with('↑') && text.contains(SCROLLED_MARKER) {
            return PaneInput::Unknown("draft has scrolled inside the input box");
        }

        // Ghost completions and inline argument hints trail the typed text,
        // dimmed. Drop them: the caret pins where typing stopped when the
        // backend leaves it visible, and the styling alone is enough when it
        // does not.
        let text = match caret {
            // tmux reports the caret in terminal cells. Those only line up
            // with character offsets while the row is ASCII — a CJK character
            // is two cells wide, a combining mark none — so anything else
            // falls back to reading the styling, which needs no column.
            Some(caret) if caret.row == start + offset && row.visible.is_ascii() => {
                strip_ghost_after(row, &text, cut, caret.col)
            }
            _ => strip_trailing_dim(row, cut, &text),
        };
        // Input boxes pad every row out to their own width; that padding is
        // not part of what the user typed.
        lines.push(text.trim_end().to_string());
    }

    // A row filled to the right edge is a soft wrap of one logical line, not a
    // new one; joining those with a newline would inject a break the user never
    // typed. Backends that re-flow at word boundaries (opencode) cannot be told
    // apart this way, so they keep the row split.
    // A bordered composer is narrower than the pane. Measure it from the box
    // itself — the rule closing it runs the full width — rather than from the
    // draft, whose longest line says nothing about where text would wrap.
    let width = if shape.border.is_some() {
        rows[end..]
            .iter()
            .find(|row| row.is_horizontal_border())
            .map(|row| row.visible.trim_end().chars().count())
    } else {
        pane_width
    };
    let draft = join_wrapped(&lines, &rows[start..=end], text_col, width);
    let trimmed = draft.trim();

    if UNRECOVERABLE.iter().any(|token| trimmed.contains(token)) {
        return PaneInput::Unknown("backend collapsed the draft to a summary token");
    }
    if trimmed.is_empty() {
        return PaneInput::Empty;
    }
    // A wholly dimmed box is the backend's placeholder, not a draft.
    if rows[start..=end].iter().all(|row| row.is_all_dim(text_col)) {
        return PaneInput::Empty;
    }

    PaneInput::Draft(trim_trailing_blank_lines(&draft))
}

/// Join rows, dropping the newline where one row simply ran off the edge.
fn join_wrapped(
    lines: &[String],
    rows: &[Row],
    text_col: usize,
    pane_width: Option<usize>,
) -> String {
    // Every backend here re-flows at word boundaries, so a wrapped row stops
    // short of the edge by up to one word. Treat a row that reaches within
    // `WRAP_SLACK` of the composer's width as a continuation — and require it
    // to be substantially long, so ordinary short lines are never merged.
    const WRAP_SLACK: usize = 24;

    let Some(width) = pane_width else {
        return lines.join("\n");
    };
    let available = width.saturating_sub(text_col);
    // Proportional, not absolute: on a narrow composer a 54-character line is
    // an ordinary line, while on a wide one it is nowhere near the edge.
    let nearly_full = available * 4 / 5;

    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            // Measure the printed text, not the row: backends that paint a
            // background pad every row out to the full width.
            let previous = rows[index - 1]
                .visible
                .trim_end()
                .chars()
                .count()
                .saturating_sub(text_col);
            let wrapped =
                previous >= nearly_full && previous >= available.saturating_sub(WRAP_SLACK);
            // Word wrapping ate the space that joined the two halves.
            out.push(if wrapped { ' ' } else { '\n' });
        }
        out.push_str(line);
    }
    out
}

/// The row the draft starts on.
///
/// With a visible caret this walks up from the caret row, so menus rendered
/// below the box cannot be mistaken for input. Without one it falls back to
/// the glyph, bounded by the box borders so the same menus stay out.
fn find_start_row(shape: &Shape, rows: &[Row], caret: Option<Caret>) -> Option<usize> {
    if let Some(caret) = caret {
        if caret.row < rows.len() {
            // A left border repeats on every row of the box, so it marks
            // membership, not the first row. The draft is the run of bordered
            // rows that actually carry text around the caret; the box pads
            // itself with empty bordered rows above and below.
            if shape.border.is_some() {
                // The caret can rest on one of the box's blank padding rows —
                // after a partial clear, for instance — so walk up through the
                // box to the draft rather than giving up on the spot.
                let mut row = caret.row;
                while !rows[row].holds_draft(shape) {
                    if row == 0 || !rows[row].starts_input(shape) {
                        return None;
                    }
                    row -= 1;
                }
                while row > 0 && rows[row - 1].holds_draft(shape) {
                    row -= 1;
                }
                return Some(row);
            }

            let mut row = caret.row;
            loop {
                if rows[row].starts_input(shape) {
                    return Some(row);
                }
                if row == 0 || !rows[row].is_continuation() {
                    break;
                }
                row -= 1;
            }
        }
    }

    // Caret hidden. Autocomplete menus sit below the input box and repeat its
    // glyph, so look inside the box first: the region between the last two
    // border rows. Backends that draw no border around the composer fall back
    // to the lowest glyph row.
    if let Some(bottom) = rows.iter().rposition(|row| row.is_horizontal_border()) {
        let top = rows[..bottom]
            .iter()
            .rposition(|row| row.is_horizontal_border())
            .map(|index| index + 1)
            .unwrap_or(0);
        if let Some(offset) = rows[top..bottom]
            .iter()
            .position(|row| row.starts_input(shape))
        {
            return Some(top + offset);
        }
    }
    rows.iter().rposition(|row| row.starts_input(shape))
}

/// The last row of the draft: the caret row, or the last continuation row.
fn find_end_row(shape: &Shape, rows: &[Row], start: usize, text_col: usize) -> usize {
    if shape.border.is_some() {
        let mut end = start;
        while end + 1 < rows.len() && rows[end + 1].holds_draft(shape) {
            end += 1;
        }
        return end;
    }
    // Deliberately structural, never the caret row. The caret is wherever the
    // user is typing, which may be the middle of the draft — ending there
    // would drop every line below it, and the box is cleared regardless, so
    // those lines would be gone.
    let mut end = start;
    for (offset, row) in rows.iter().enumerate().skip(start + 1) {
        if row.is_continuation_at(text_col) {
            end = offset;
        } else {
            break;
        }
    }
    end
}

/// Drop dimmed text sitting past the caret — a ghost completion or an inline
/// argument hint. Text past the caret that is *not* dimmed is real: the user
/// moved the caret left inside their own draft.
fn strip_ghost_after(row: &Row, text: &str, cut: usize, caret_col: usize) -> String {
    if caret_col <= cut {
        // Caret sits at the very start of the box; everything shown is ghost.
        return if row.is_all_dim(cut) {
            String::new()
        } else {
            text.to_string()
        };
    }
    let keep = caret_col - cut;
    if row.dim_from(caret_col) {
        text.chars().take(keep).collect()
    } else {
        text.to_string()
    }
}

/// Drop a dimmed run at the end of a row.
///
/// Used when the caret is not available. A dimmed tail is either the whole box
/// (a placeholder) or a completion appended to what the user typed; neither is
/// the user's text.
fn strip_trailing_dim(row: &Row, cut: usize, text: &str) -> String {
    let chars: Vec<char> = row.visible.chars().collect();
    let mut end = chars.len();
    while end > cut && is_blank(chars[end - 1]) {
        end -= 1;
    }
    if end <= cut {
        return text.to_string();
    }
    let mut start = end;
    while start > cut && row.dim.get(start - 1).copied().unwrap_or(false) {
        start -= 1;
    }
    if start == end {
        return text.to_string();
    }
    chars[cut..start].iter().collect()
}

/// Backends pad the prompt with a non-breaking space, which is not
/// `char::is_whitespace` for our purposes but must be treated as padding.
fn is_blank(ch: char) -> bool {
    ch == ' ' || ch == '\u{a0}' || ch.is_whitespace()
}

fn trim_trailing_blank_lines(draft: &str) -> String {
    let mut lines: Vec<&str> = draft.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// One captured row, split into what is printed and how it is styled.
struct Row {
    /// The row with ANSI escapes removed.
    visible: String,
    /// Whether the cell at each visible column is dimmed.
    dim: Vec<bool>,
}

impl Row {
    fn parse(raw: &str) -> Row {
        let mut visible = String::with_capacity(raw.len());
        let mut dim_flags = Vec::new();
        let mut dimmed = false;
        let mut reverse = false;
        let mut chars = raw.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                // CSI: ESC [ params final. Only SGR (`m`) changes styling.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    let mut params = String::new();
                    for code in chars.by_ref() {
                        if ('@'..='~').contains(&code) {
                            if code == 'm' {
                                update_dim_state(&params, &mut dimmed, &mut reverse);
                            }
                            break;
                        }
                        params.push(code);
                    }
                } else {
                    // OSC and the rest: skip to the terminator.
                    for code in chars.by_ref() {
                        if code == '\x07' || code == '\\' {
                            break;
                        }
                    }
                }
                continue;
            }
            visible.push(ch);
            dim_flags.push(dimmed || reverse);
        }

        Row {
            visible,
            dim: dim_flags,
        }
    }

    /// Does this row open the input box?
    fn starts_input(&self, shape: &Shape) -> bool {
        let trimmed = self.visible.trim_start();
        match shape.border {
            Some(border) => trimmed.starts_with(border),
            None => !shape.glyph.is_empty() && trimmed.starts_with(shape.glyph),
        }
    }

    /// Column where the draft's text begins on this row.
    fn text_col(&self, shape: &Shape) -> usize {
        let chars: Vec<char> = self.visible.chars().collect();
        let leading = chars.iter().take_while(|c| is_blank(**c)).count();
        let marker = match shape.border {
            Some(border) => border.to_string(),
            None => shape.glyph.to_string(),
        };
        let after_marker = leading + marker.chars().count();
        // One space after the marker for most backends, two for opencode —
        // and Claude pads with a non-breaking space.
        let spaces = chars
            .iter()
            .skip(after_marker)
            .take_while(|c| is_blank(**c))
            .count();
        after_marker + spaces
    }

    /// Rows under the first one are blank up to the text column.
    fn is_continuation(&self) -> bool {
        let mut chars = self.visible.chars();
        match chars.next() {
            None => true,
            Some(first) => is_blank(first),
        }
    }

    /// A continuation row is padded to exactly the text column and then starts
    /// printing. Status bars and chrome also begin with spaces, but they
    /// indent to their own column, so requiring the text to start *at*
    /// `text_col` keeps them out of the draft.
    fn is_continuation_at(&self, text_col: usize) -> bool {
        if self.is_horizontal_border() {
            return false;
        }
        let chars: Vec<char> = self.visible.chars().collect();
        if chars.iter().all(|c| is_blank(*c)) {
            // A blank row ends the draft. Chrome below the box is separated
            // from it by one, and several backends indent their status line to
            // exactly the draft's column — without this they read as draft.
            return false;
        }
        if chars.len() <= text_col {
            return false;
        }
        chars[..text_col].iter().all(|c| is_blank(*c)) && !is_blank(chars[text_col])
    }

    /// Is this a bordered row of the input box that actually carries text?
    /// Used for backends whose box repeats a left border on every row.
    fn holds_draft(&self, shape: &Shape) -> bool {
        if !self.starts_input(shape) {
            return false;
        }
        let col = self.text_col(shape);
        self.visible.chars().skip(col).any(|c| !is_blank(c))
    }

    fn is_horizontal_border(&self) -> bool {
        let trimmed = self.visible.trim();
        if trimmed.is_empty() {
            return false;
        }
        let borders = "─━═└┘┌┐├┤┬┴┼╔╗╚╝╠╣╦╩╬▀▁▂▃▄▅▆▇█▔╴╵╶╷╸╹╺╻";
        let border_count = trimmed.chars().filter(|c| borders.contains(*c)).count();
        let printed = trimmed.chars().filter(|c| !c.is_whitespace()).count();
        printed > 0 && border_count > printed / 2
    }

    fn after_col(&self, col: usize) -> Option<String> {
        let count = self.visible.chars().count();
        if count < col {
            // A blank row inside the draft is shorter than the indent.
            return if self.visible.trim().is_empty() {
                Some(String::new())
            } else {
                None
            };
        }
        Some(self.visible.chars().skip(col).collect::<String>())
    }

    /// Is every printed cell from `col` onward dimmed?
    fn is_all_dim(&self, col: usize) -> bool {
        let mut saw_text = false;
        for (index, ch) in self.visible.chars().enumerate().skip(col) {
            if ch.is_whitespace() {
                continue;
            }
            saw_text = true;
            if !self.dim.get(index).copied().unwrap_or(false) {
                return false;
            }
        }
        saw_text
    }

    /// Is the first printed cell at or after `col` dimmed?
    fn dim_from(&self, col: usize) -> bool {
        for (index, ch) in self.visible.chars().enumerate().skip(col) {
            if ch.is_whitespace() {
                continue;
            }
            return self.dim.get(index).copied().unwrap_or(false);
        }
        false
    }
}

/// Track the SGR codes that mark text as "not what the user typed".
///
/// Backends signal ghost text with faint (2), reverse video (7), or a grey
/// 256-colour foreground — Claude's inline argument hint uses `38;5;246`,
/// codex's placeholder uses `2`.
fn update_dim_state(params: &str, dimmed: &mut bool, reverse: &mut bool) {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            // An unparseable parameter must be ignored, not read as 0 — that
            // is "reset all attributes", which would clear the dim state and
            // let ghost text be captured as if the user had typed it.
            .filter_map(|part| part.parse::<u16>().ok())
            .collect()
    };

    let mut index = 0;
    while index < codes.len() {
        match codes[index] {
            0 => {
                *dimmed = false;
                *reverse = false;
            }
            2 | 90 => *dimmed = true,
            22 | 39 => *dimmed = false,
            7 => *reverse = true,
            27 => *reverse = false,
            // Extended colour: `38;5;n` / `48;5;n`, or `38;2;r;g;b` /
            // `48;2;r;g;b`. Their arguments must be consumed — a truecolour
            // background like `48;2;21;21;21` carries a literal `2`, which
            // reads as "faint" if the parameters are scanned flatly.
            38 | 48 => {
                let is_foreground = codes[index] == 38;
                match codes.get(index + 1) {
                    Some(5) => {
                        if is_foreground {
                            if let Some(color) = codes.get(index + 2) {
                                *dimmed = *color == 8 || (232..=255).contains(color);
                            }
                        }
                        index += 2;
                    }
                    Some(2) => {
                        if is_foreground {
                            // opencode paints its placeholder in a truecolour
                            // grey. Treat a low-contrast grey as ghost text;
                            // a bright one (white body text) is not.
                            let rgb = (
                                codes.get(index + 2).copied().unwrap_or(255),
                                codes.get(index + 3).copied().unwrap_or(255),
                                codes.get(index + 4).copied().unwrap_or(255),
                            );
                            let high = rgb.0.max(rgb.1).max(rgb.2);
                            let low = rgb.0.min(rgb.1).min(rgb.2);
                            *dimmed = high < 160 && high - low <= 24;
                        }
                        index += 4;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    //! Replays real `tmux capture-pane -e -S 0` dumps taken from live agent
    //! panes at 200x50, with the caret each pane reported at the time.
    //!
    //! Every case here is a bug this module was written to fix: multi-line
    //! drafts truncated to one line, autocomplete menus and placeholders
    //! captured as if typed, prompt glyphs that never matched, and pastes the
    //! backend had already collapsed beyond recovery.
    use super::*;

    #[derive(Debug)]
    enum Expect {
        Empty,
        Draft(&'static str),
        Unknown,
    }

    #[test]
    fn real_pane_captures_extract_what_the_user_typed() {
        let cases: &[(&str, &str, &str, Option<Caret>, Expect)] = &[
        ("claude", "after_cu", include_str!("../../tests/fixtures/pane_input/claude/after_cu.ansi.txt"), Some(Caret { row: 47, col: 2 }), Expect::Draft("line one\nline two")),
        // Claude renders its inline argument hint in grey after the caret; only
            // the 13 characters actually typed may come back.
            ("claude", "ghost", include_str!("../../tests/fixtures/pane_input/claude/ghost.ansi.txt"), Some(Caret { row: 47, col: 15 }), Expect::Draft("/code-review")),
        ("claude", "idle", include_str!("../../tests/fixtures/pane_input/claude/idle.ansi.txt"), Some(Caret { row: 47, col: 2 }), Expect::Empty),
        ("claude", "menu", include_str!("../../tests/fixtures/pane_input/claude/menu.ansi.txt"), Some(Caret { row: 47, col: 3 }), Expect::Draft("/")),
        ("claude", "multi3", include_str!("../../tests/fixtures/pane_input/claude/multi3.ansi.txt"), Some(Caret { row: 47, col: 12 }), Expect::Draft("line one\nline two\nline three")),
        ("claude", "pastetoken", include_str!("../../tests/fixtures/pane_input/claude/pastetoken.ansi.txt"), Some(Caret { row: 47, col: 28 }), Expect::Unknown),
        ("claude", "single", include_str!("../../tests/fixtures/pane_input/claude/single.ansi.txt"), Some(Caret { row: 47, col: 20 }), Expect::Draft("fix the parser bug")),
        ("claude", "wrapped", include_str!("../../tests/fixtures/pane_input/claude/wrapped.ansi.txt"), Some(Caret { row: 47, col: 30 }), Expect::Draft("refactor the tokenizer so that it streams input instead of buffering the entire file in memory and also update the docs and the tests to match the new streaming behaviour without breaking the existing public api surface.")),
        ("codex", "after_cu", include_str!("../../tests/fixtures/pane_input/codex/after_cu.ansi.txt"), Some(Caret { row: 21, col: 2 }), Expect::Draft("line one\nline two")),
        ("codex", "ghost", include_str!("../../tests/fixtures/pane_input/codex/ghost.ansi.txt"), Some(Caret { row: 19, col: 2 }), Expect::Empty),
        ("codex", "idle", include_str!("../../tests/fixtures/pane_input/codex/idle.ansi.txt"), Some(Caret { row: 19, col: 2 }), Expect::Empty),
        ("codex", "menu", include_str!("../../tests/fixtures/pane_input/codex/menu.ansi.txt"), Some(Caret { row: 19, col: 3 }), Expect::Draft("/")),
        ("codex", "multi3", include_str!("../../tests/fixtures/pane_input/codex/multi3.ansi.txt"), Some(Caret { row: 21, col: 12 }), Expect::Draft("line one\nline two\nline three")),
        ("codex", "pastetoken", include_str!("../../tests/fixtures/pane_input/codex/pastetoken.ansi.txt"), Some(Caret { row: 19, col: 29 }), Expect::Unknown),
        ("codex", "single", include_str!("../../tests/fixtures/pane_input/codex/single.ansi.txt"), Some(Caret { row: 19, col: 20 }), Expect::Draft("fix the parser bug")),
        ("codex", "wrapped", include_str!("../../tests/fixtures/pane_input/codex/wrapped.ansi.txt"), Some(Caret { row: 20, col: 30 }), Expect::Draft("refactor the tokenizer so that it streams input instead of buffering the entire file in memory and also update the docs and the tests to match the new streaming behaviour without breaking the existing public api surface.")),
        ("opencode", "after_cu", include_str!("../../tests/fixtures/pane_input/opencode/after_cu.ansi.txt"), Some(Caret { row: 26, col: 66 }), Expect::Draft("line one\nline two")),
        ("opencode", "ghost", include_str!("../../tests/fixtures/pane_input/opencode/ghost.ansi.txt"), Some(Caret { row: 25, col: 66 }), Expect::Empty),
        ("opencode", "idle", include_str!("../../tests/fixtures/pane_input/opencode/idle.ansi.txt"), Some(Caret { row: 25, col: 66 }), Expect::Empty),
        ("opencode", "menu", include_str!("../../tests/fixtures/pane_input/opencode/menu.ansi.txt"), Some(Caret { row: 25, col: 67 }), Expect::Draft("/")),
        ("opencode", "multi3", include_str!("../../tests/fixtures/pane_input/opencode/multi3.ansi.txt"), Some(Caret { row: 26, col: 76 }), Expect::Draft("line one\nline two\nline three")),
        ("opencode", "pastetoken", include_str!("../../tests/fixtures/pane_input/opencode/pastetoken.ansi.txt"), Some(Caret { row: 25, col: 85 }), Expect::Unknown),
        ("opencode", "single", include_str!("../../tests/fixtures/pane_input/opencode/single.ansi.txt"), Some(Caret { row: 25, col: 84 }), Expect::Draft("fix the parser bug")),
        ("opencode", "wrapped", include_str!("../../tests/fixtures/pane_input/opencode/wrapped.ansi.txt"), Some(Caret { row: 27, col: 85 }), Expect::Draft("refactor the tokenizer so that it streams input instead of buffering the entire file in memory and also update the docs and the tests to match the new streaming behaviour without breaking the existing public api surface.")),
        ("cursor", "after_cu", include_str!("../../tests/fixtures/pane_input/cursor/after_cu.ansi.txt"), None, Expect::Draft("line one\nline two")),
        ("cursor", "ghost", include_str!("../../tests/fixtures/pane_input/cursor/ghost.ansi.txt"), None, Expect::Empty),
        ("cursor", "idle", include_str!("../../tests/fixtures/pane_input/cursor/idle.ansi.txt"), None, Expect::Empty),
        ("cursor", "menu", include_str!("../../tests/fixtures/pane_input/cursor/menu.ansi.txt"), None, Expect::Draft("/")),
        ("cursor", "multi3", include_str!("../../tests/fixtures/pane_input/cursor/multi3.ansi.txt"), None, Expect::Draft("line one\nline two\nline three")),
        ("cursor", "pastetoken", include_str!("../../tests/fixtures/pane_input/cursor/pastetoken.ansi.txt"), None, Expect::Unknown),
        ("cursor", "single", include_str!("../../tests/fixtures/pane_input/cursor/single.ansi.txt"), None, Expect::Draft("fix the parser bug")),
        ("cursor", "wrapped", include_str!("../../tests/fixtures/pane_input/cursor/wrapped.ansi.txt"), None, Expect::Draft("refactor the tokenizer so that it streams input instead of buffering the entire file in memory and also update the docs and the tests to match the new streaming behaviour without breaking the existing public api surface.")),
        ("agy", "after_cu", include_str!("../../tests/fixtures/pane_input/agy/after_cu.ansi.txt"), Some(Caret { row: 14, col: 2 }), Expect::Draft("line one\nline two")),
        ("agy", "idle", include_str!("../../tests/fixtures/pane_input/agy/idle.ansi.txt"), Some(Caret { row: 12, col: 2 }), Expect::Empty),
        ("agy", "menu", include_str!("../../tests/fixtures/pane_input/agy/menu.ansi.txt"), Some(Caret { row: 12, col: 3 }), Expect::Draft("/")),
        ("agy", "multi3", include_str!("../../tests/fixtures/pane_input/agy/multi3.ansi.txt"), Some(Caret { row: 14, col: 12 }), Expect::Draft("line one\nline two\nline three")),
        ("agy", "pastetoken", include_str!("../../tests/fixtures/pane_input/agy/pastetoken.ansi.txt"), Some(Caret { row: 12, col: 28 }), Expect::Unknown),
        ("agy", "single", include_str!("../../tests/fixtures/pane_input/agy/single.ansi.txt"), Some(Caret { row: 12, col: 20 }), Expect::Draft("fix the parser bug")),
        ("agy", "wrapped", include_str!("../../tests/fixtures/pane_input/agy/wrapped.ansi.txt"), Some(Caret { row: 13, col: 30 }), Expect::Draft("refactor the tokenizer so that it streams input instead of buffering the entire file in memory and also update the docs and the tests to match the new streaming behaviour without breaking the existing public api surface.")),
        ];

        let mut failures = Vec::new();
        for (backend, case, capture, caret, want) in cases {
            let shape = Shape::for_backend(backend).expect("known backend");
            let got = extract(shape, capture, *caret, Some(200));
            let ok = match (want, &got) {
                (Expect::Empty, PaneInput::Empty) => true,
                (Expect::Unknown, PaneInput::Unknown(_)) => true,
                (Expect::Draft(want), PaneInput::Draft(got)) => want == got,
                _ => false,
            };
            if !ok {
                failures.push(format!("{backend}/{case}: wanted {want:?}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn a_caret_parked_mid_draft_still_captures_the_lines_below_it() {
        // The caret says where the user is typing, not where the draft ends.
        // Ending at the caret row would drop everything under it — and the box
        // is cleared either way, so those lines would be destroyed.
        for (backend, capture, rows) in [
            (
                "claude",
                include_str!("../../tests/fixtures/pane_input/claude/multi3.ansi.txt"),
                [45usize, 46, 47],
            ),
            (
                "codex",
                include_str!("../../tests/fixtures/pane_input/codex/multi3.ansi.txt"),
                [19, 20, 21],
            ),
        ] {
            let shape = Shape::for_backend(backend).unwrap();
            for row in rows {
                assert_eq!(
                    extract(shape, capture, Some(Caret { row, col: 2 }), Some(200)),
                    PaneInput::Draft("line one\nline two\nline three".to_string()),
                    "{backend}: caret on row {row} must not truncate the draft"
                );
            }
        }
    }

    #[test]
    fn chrome_indented_like_the_draft_is_not_swallowed() {
        // codex indents its status line to the same column the draft starts
        // at; only the blank row between them tells the two apart.
        let shape = Shape::for_backend("codex").unwrap();
        let capture = include_str!("../../tests/fixtures/pane_input/codex/single.ansi.txt");
        let got = extract(shape, capture, Some(Caret { row: 19, col: 20 }), Some(200));
        assert_eq!(got, PaneInput::Draft("fix the parser bug".to_string()));
        if let PaneInput::Draft(draft) = got {
            assert!(!draft.contains('·'), "status line leaked in: {draft:?}");
        }
    }

    #[test]
    fn two_ordinary_lines_of_similar_length_are_not_read_as_one_wrapped_line() {
        // The test for "this row ran off the edge" has to be proportional to
        // the composer's width, or on a narrow pane every longish line merges
        // with the next and the user's line breaks are lost on restore.
        let line = "a".repeat(54);
        let esc = "\u{1b}[39m";
        let capture = format!("{esc}❯ {line}\n  {line}\n");
        let shape = Shape::for_backend("claude").unwrap();
        assert_eq!(
            extract(shape, &capture, None, Some(80)),
            PaneInput::Draft(format!("{line}\n{line}")),
            "80-column pane: two 54-char lines are two lines"
        );
    }

    #[test]
    fn a_draft_that_scrolled_inside_the_box_is_never_restored_from_what_shows() {
        // Only the tail is on screen; treating it as the whole draft and then
        // clearing the box would throw the rest away.
        let shape = Shape::for_backend("agy").unwrap();
        for capture in [
            "> \u{1b}[2m↑ 91 more lines\u{1b}[0m\n  the visible tail\n",
            "> the visible head\n  ↑ 12 more lines\n",
        ] {
            assert!(
                matches!(
                    extract(shape, capture, None, Some(200)),
                    PaneInput::Unknown(_)
                ),
                "a scrolled box must not be treated as a complete draft: {capture:?}"
            );
        }
    }

    #[test]
    fn a_wide_character_row_falls_back_to_styling_rather_than_the_caret_column() {
        // tmux counts the caret in cells; CJK is two cells per character, so
        // using it as an index would cut the user's own text.
        let shape = Shape::for_backend("claude").unwrap();
        let capture = "\u{1b}[39m❯ 日本語のテキストです\n";
        assert_eq!(
            extract(shape, capture, Some(Caret { row: 0, col: 22 }), Some(200)),
            PaneInput::Draft("日本語のテキストです".to_string())
        );
    }

    #[test]
    fn a_pane_that_cannot_be_parsed_is_unknown_rather_than_empty() {
        // The distinction is load-bearing: `Empty` lets the caller clear the
        // box, `Unknown` must not.
        let shape = Shape::for_backend("claude").unwrap();
        assert!(matches!(
            extract(shape, "no input box here\njust output\n", None, Some(200)),
            PaneInput::Unknown(_)
        ));
    }

    #[test]
    fn a_truecolour_background_is_not_mistaken_for_faint_text() {
        // `48;2;21;21;21` carries a literal `2`, which reads as SGR 2 "faint"
        // if the parameters are scanned without consuming the colour's args.
        let capture = "\u{1b}[48;2;21;21;21m → fix the parser bug\u{1b}[0m";
        let shape = Shape::for_backend("cursor").unwrap();
        assert_eq!(
            extract(shape, capture, None, Some(200)),
            PaneInput::Draft("fix the parser bug".to_string())
        );
    }
}
