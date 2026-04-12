//! Popup suggestion renderer — 1:1 port of upstream inshellisense's
//! `suggestionManager.render()` + `ui-root._render()` (see
//! `/tmp/inshellisense/src/ui/suggestionManager.ts` and `ui-root.ts`).
//!
//! Visual layout: two side-by-side boxes drawn with Unicode box-drawing
//! characters (┌ ─ ┐ │ └ ┘). The left box is 40 columns wide and
//! contains up to `max_suggestions` rows (icon + name), the right box
//! is 30 columns wide and contains the active suggestion's description
//! wrapped across up to 5 lines. Both boxes share a `┌────┐` top bar
//! and `└────┘` bottom bar, so total vertical height is
//! `max(borderWidth + max_suggestions, borderWidth + descriptionHeight)
//! = 2 + 5 = 7` for the default.
//!
//! The active row is highlighted by wrapping its text (not the box
//! borders) in a chalk-style `\x1b[48;2;125;86;244m...\x1b[49m` bg
//! sequence, matching upstream's `chalk.bgHex("#7D56F4")`.
//!
//! Cursor-aware padding: each row is shifted right by `cursor_col %
//! cols` columns so the popup's left edge lands under the partial
//! token the user is typing. If the cursor is too far right for the
//! suggestion + description boxes to fit, upstream *swaps* the two
//! columns so the description ends up on the left — we do the same.

use crate::ansi;
use crate::spec::model::{Suggestion, SuggestionType};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

pub const SUGGESTION_WIDTH: usize = 40;
pub const DESCRIPTION_WIDTH: usize = 30;
pub const DESCRIPTION_HEIGHT: usize = 5;
pub const BORDER_WIDTH: usize = 2;

/// Total rows: top border + max(suggestions, desc lines) + bottom
/// border. For the 5-suggestion default that's 2 + 5 = 7 rows.
pub const fn max_lines(max_suggestions: u8) -> usize {
    let m = max_suggestions as usize;
    let inner = if m > DESCRIPTION_HEIGHT {
        m
    } else {
        DESCRIPTION_HEIGHT
    };
    BORDER_WIDTH + inner + 1 // +1 trailing newline slack matches upstream
}

/// Upstream active-row background.
///
/// Upstream's source sets `activeSuggestionBackgroundColor = "#7D56F4"`
/// via `chalk.bgHex(...)`, but chalk's `supports-color` crate downgrades
/// to the nearest 256-color index when the terminal doesn't advertise
/// `COLORTERM=truecolor`. In practice upstream's binary almost always
/// emits `\x1b[48;5;105m` (xterm-256 index 105 ≈ #8787FF) on Linux ttys.
/// For byte-for-byte parity we follow the same detection + fallback.
fn active_bg_on() -> &'static str {
    // Check once, cache. We never flip between terminals within one
    // run so a static OnceLock is fine.
    use once_cell::sync::Lazy;
    static BG: Lazy<&'static str> = Lazy::new(|| {
        if is_truecolor() {
            "\x1b[48;2;125;86;244m"
        } else {
            "\x1b[48;5;105m"
        }
    });
    *BG
}

/// Port of chalk's supports-color level-3 detection (the truecolor
/// branch). We treat `COLORTERM=truecolor` / `COLORTERM=24bit` as
/// authoritative; everything else falls back to the 256-color path.
fn is_truecolor() -> bool {
    match std::env::var("COLORTERM") {
        Ok(v) => {
            let v = v.to_lowercase();
            v == "truecolor" || v == "24bit"
        }
        Err(_) => false,
    }
}

const ACTIVE_BG_OFF: &str = "\x1b[49m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Above,
    Below,
}

pub struct PopupRenderer {
    max_suggestions: u8,
    last_drawn_rows: u16,
    last_direction: Direction,
    last_signature: Option<u64>,
}

impl PopupRenderer {
    pub fn new(max_suggestions: u8) -> Self {
        Self {
            max_suggestions: max_suggestions.max(1),
            last_drawn_rows: 0,
            last_direction: Direction::Below,
            last_signature: None,
        }
    }

    /// Legacy non-interactive entry point — renders with `cursor=0` and
    /// `direction=Below` centered at column 0.
    pub fn draw(&mut self, out: &mut impl Write, _tail: Option<&str>, all: &[Suggestion]) -> io::Result<()> {
        self.draw_full(out, all, 0, Direction::Below, 0, 80)
    }

    /// Full interactive draw with cursor X + terminal columns.
    pub fn draw_full(
        &mut self,
        out: &mut impl Write,
        all: &[Suggestion],
        cursor: usize,
        direction: Direction,
        cursor_col: u16,
        term_cols: u16,
    ) -> io::Result<()> {
        if all.is_empty() {
            self.clear(out)?;
            return Ok(());
        }

        let max = self.max_suggestions as usize;

        // Paging — upstream:
        //   page = min(floor(active/max)+1, floor(len/max)+1)
        let max_page = all.len() / max + 1;
        let page = ((cursor / max) + 1).min(max_page);
        let start = (page - 1) * max;
        let end = (start + max).min(all.len());
        let visible = &all[start..end];
        let active_in_page = cursor.saturating_sub(start);

        // Active description (for the right-hand box).
        let active = &all[cursor.min(all.len() - 1)];
        let active_desc = active.description.clone().unwrap_or_default();

        // Padding + swap decision (port of _calculatePadding).
        let (padding, swap_description) =
            calculate_padding(cursor_col, term_cols, &active_desc);

        // Render each column.
        let suggestion_box = render_suggestion_box(visible, active_in_page);
        let description_box = render_description_box(&active_desc);

        let max_rows = suggestion_box.len().max(description_box.len());

        // Build the list of (row_pad, data) pairs. For `direction="above"`
        // each column's rows align to the BOTTOM of the combined block,
        // so we index from the end.
        let mut rows: Vec<(usize, String)> = Vec::with_capacity(max_rows);
        for i in 0..max_rows {
            let (sug_row, desc_row) = if direction == Direction::Above {
                let si = i + suggestion_box.len().saturating_sub(max_rows);
                let di = i + description_box.len().saturating_sub(max_rows);
                (
                    suggestion_box.get(si).cloned(),
                    description_box.get(di).cloned(),
                )
            } else {
                (
                    suggestion_box.get(i).cloned(),
                    description_box.get(i).cloned(),
                )
            };

            let data = if swap_description {
                format!(
                    "{}{}",
                    desc_row.clone().unwrap_or_default(),
                    sug_row.clone().unwrap_or_default()
                )
            } else {
                format!(
                    "{}{}",
                    sug_row.clone().unwrap_or_default(),
                    desc_row.clone().unwrap_or_default()
                )
            };

            let row_pad = calc_row_padding(
                padding,
                swap_description,
                sug_row.is_some(),
                desc_row.is_some(),
            );
            rows.push((row_pad, data));
        }

        // Signature hash: skip redraw when nothing visible has changed.
        // Uses a u64 hash instead of formatting a 300-byte String to
        // avoid per-draw allocations.
        let sig = signature_hash(visible, active_in_page, direction, &active_desc, padding);
        if Some(sig) == self.last_signature && self.last_direction == direction {
            return Ok(());
        }
        self.clear(out)?;

        // Upstream uses SCO save/restore (`\x1b[s` / `\x1b[u`) rather
        // than DECSC/DECRC (`\x1b7` / `\x1b8`). Some terminals only
        // implement one correctly, and chalk/ansi-escapes settled on
        // SCO — we match for byte-for-byte parity.
        out.write_all(ansi::CURSOR_HIDE.as_bytes())?;
        out.write_all(b"\x1b[s")?;

        match direction {
            Direction::Below => {
                out.write_all(b"\x1b[E")?; // CNL with no count
            }
            Direction::Above => {
                // Move cursor up `rows.len()` lines using repeated CPL.
                for _ in 0..rows.len() {
                    out.write_all(b"\x1b[F")?;
                }
            }
        }

        for (i, (pad, data)) in rows.iter().enumerate() {
            // Upstream emits `\x1b[1C` (CUF by 1) repeated N times,
            // not `\x1b[{N}C`. Match that byte pattern exactly.
            for _ in 0..*pad {
                out.write_all(b"\x1b[1C")?;
            }
            out.write_all(data.as_bytes())?;
            if i + 1 < rows.len() {
                out.write_all(b"\x1b[E")?; // CNL no count
            }
        }

        out.write_all(b"\x1b[u")?; // SCO restore
        out.write_all(ansi::CURSOR_SHOW.as_bytes())?;
        out.flush()?;

        self.last_drawn_rows = rows.len() as u16;
        self.last_direction = direction;
        self.last_signature = Some(sig);
        Ok(())
    }

    pub fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.last_drawn_rows == 0 {
            self.last_signature = None;
            return Ok(());
        }
        let rows = self.last_drawn_rows;
        out.write_all(ansi::CURSOR_HIDE.as_bytes())?;
        out.write_all(b"\x1b[s")?;
        match self.last_direction {
            Direction::Below => {
                out.write_all(b"\x1b[E")?;
            }
            Direction::Above => {
                for _ in 0..rows {
                    out.write_all(b"\x1b[F")?;
                }
            }
        }
        for i in 0..rows {
            out.write_all(b"\x1b[2K")?;
            if i + 1 < rows {
                out.write_all(b"\x1b[E")?;
            }
        }
        out.write_all(b"\x1b[u")?;
        out.write_all(ansi::CURSOR_SHOW.as_bytes())?;
        out.flush()?;
        self.last_drawn_rows = 0;
        self.last_signature = None;
        Ok(())
    }
}

// ---------- upstream layout helpers ----------

/// Port of `_calculatePadding` from suggestionManager.ts.
fn calculate_padding(cursor_col: u16, term_cols: u16, description: &str) -> (usize, bool) {
    let cols = term_cols.max(1) as usize;
    let wrapped_padding = (cursor_col as usize) % cols;
    let max_padding = if !description.is_empty() {
        cols.saturating_sub(SUGGESTION_WIDTH + DESCRIPTION_WIDTH)
    } else {
        cols.saturating_sub(SUGGESTION_WIDTH)
    };
    let swap_description = wrapped_padding > max_padding && !description.is_empty();
    let swapped_padding = if swap_description {
        wrapped_padding.saturating_sub(DESCRIPTION_WIDTH)
    } else {
        wrapped_padding
    };
    let padding = wrapped_padding.min(swapped_padding).min(max_padding);
    (padding, swap_description)
}

/// Port of `_calculateRowPadding`.
fn calc_row_padding(
    padding: usize,
    swap_description: bool,
    has_suggestion: bool,
    has_description: bool,
) -> usize {
    if swap_description {
        if !has_description {
            padding + DESCRIPTION_WIDTH
        } else {
            padding
        }
    } else if !has_suggestion {
        padding + SUGGESTION_WIDTH
    } else {
        padding
    }
}

/// Port of `_renderSuggestions` → `renderBox(...)`.
///
/// Byte format matches upstream exactly: each middle row is
/// `\x1b[0m│<bg_on><38-cell padded text><bg_off>\x1b[0m│`, so the border
/// bars are outside the highlighted region and chalk-style `\x1b[0m`
/// resets bracket every color change.
fn render_suggestion_box(visible: &[Suggestion], active_in_page: usize) -> Vec<String> {
    let width = SUGGESTION_WIDTH;
    let inner = width - BORDER_WIDTH; // 38 cells between the two borders
    let mut out = Vec::with_capacity(visible.len() + 2);
    // Top border
    out.push(format!("\x1b[0m┌{}┐", "─".repeat(inner)));
    for (idx, s) in visible.iter().enumerate() {
        let text = format!("{} {}", icon_for(s), s.name);
        let padded = truncate_or_pad_wc(&text, inner);
        let body = if idx == active_in_page {
            format!(
                "{}{}{}\x1b[0m",
                active_bg_on(),
                padded,
                ACTIVE_BG_OFF
            )
        } else {
            padded
        };
        out.push(format!("\x1b[0m│{}│", body));
    }
    // Bottom border
    out.push(format!("\x1b[0m└{}┘", "─".repeat(inner)));
    out
}

/// Port of `_renderDescription` → `renderBox(truncateMultilineText(...))`.
fn render_description_box(description: &str) -> Vec<String> {
    if description.is_empty() {
        return Vec::new();
    }
    let width = DESCRIPTION_WIDTH;
    let inner = width - BORDER_WIDTH; // 28 cells
    let lines = wrap_multiline(description, inner, DESCRIPTION_HEIGHT);
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(format!("\x1b[0m┌{}┐", "─".repeat(inner)));
    for line in &lines {
        let padded = pad_right_wc(line, inner);
        out.push(format!("\x1b[0m│{}│", padded));
    }
    out.push(format!("\x1b[0m└{}┘", "─".repeat(inner)));
    out
}

fn signature_hash(
    visible: &[Suggestion],
    active: usize,
    direction: Direction,
    desc: &str,
    padding: usize,
) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in visible {
        s.name.hash(&mut h);
    }
    active.hash(&mut h);
    (direction == Direction::Above).hash(&mut h);
    desc.hash(&mut h);
    padding.hash(&mut h);
    h.finish()
}

/// Upstream icon table (from /tmp/inshellisense/src/runtime/suggestion.ts).
pub fn icon_for(s: &Suggestion) -> &'static str {
    // Upstream first checks whether the spec's `icon` is already a
    // non-ASCII Unicode glyph (i.e. an actual emoji) and passes it
    // through. We don't store that in the extracted specs, so we fall
    // back to the suggestion type mapping.
    if let Some(icon) = s.icon.as_deref() {
        if let Some(mapped) = icon_from_fig_uri(icon) {
            return mapped;
        }
    }
    match s.suggestion_type {
        SuggestionType::File => "📄",
        SuggestionType::Folder => "📁",
        SuggestionType::Subcommand => "📦",
        SuggestionType::Option => "🔗",
        SuggestionType::Arg => "💲",
        SuggestionType::Mixin => "🏝",
        SuggestionType::Shortcut => "🔥",
        SuggestionType::Special => "⭐",
    }
}

fn icon_from_fig_uri(icon: &str) -> Option<&'static str> {
    match icon {
        "fig://icon?type=folder" => Some("📁"),
        "fig://icon?type=file" => Some("📄"),
        "fig://icon?type=option" => Some("🔗"),
        "fig://icon?type=command" => Some("📦"),
        "fig://icon?type=string" => Some("💲"),
        _ => None,
    }
}

/// Truncate text to `width` display cells, padding with spaces if
/// short. Wide-character aware.
fn truncate_or_pad_wc(s: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(s);
    if current <= width {
        let mut out = s.to_string();
        for _ in 0..(width - current) {
            out.push(' ');
        }
        return out;
    }
    // Walk chars until we exceed (width - 1) cells, then append '…'.
    let mut acc = String::new();
    let mut acc_w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc_w + cw > width.saturating_sub(1) {
            break;
        }
        acc.push(ch);
        acc_w += cw;
    }
    acc.push('…');
    acc_w += 1;
    for _ in 0..(width.saturating_sub(acc_w)) {
        acc.push(' ');
    }
    acc
}

fn pad_right_wc(s: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(s);
    if current >= width {
        let mut acc = String::new();
        let mut acc_w = 0;
        for ch in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if acc_w + cw > width {
                break;
            }
            acc.push(ch);
            acc_w += cw;
        }
        while acc_w < width {
            acc.push(' ');
            acc_w += 1;
        }
        return acc;
    }
    let mut out = s.to_string();
    for _ in 0..(width - current) {
        out.push(' ');
    }
    out
}

/// Greedy word-wrap into at most `max_lines` of width `width` (display
/// cells). Port of `truncateMultilineText`.
#[allow(unused_assignments)]
fn wrap_multiline(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in text.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        if word_w > width {
            if !current.is_empty() {
                lines.push(current.clone());
                current.clear();
                current_w = 0;
                if lines.len() >= max_lines {
                    break;
                }
            }
            let mut remaining = word.to_string();
            loop {
                let rw = UnicodeWidthStr::width(remaining.as_str());
                if rw <= width {
                    current = remaining;
                    current_w = rw;
                    break;
                }
                let mut chunk = String::new();
                let mut chunk_w = 0;
                let mut tail = String::new();
                let mut past = false;
                for ch in remaining.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if past {
                        tail.push(ch);
                    } else if chunk_w + cw > width {
                        past = true;
                        tail.push(ch);
                    } else {
                        chunk.push(ch);
                        chunk_w += cw;
                    }
                }
                lines.push(chunk);
                remaining = tail;
                if lines.len() >= max_lines {
                    break;
                }
            }
            continue;
        }
        let need = if current.is_empty() { word_w } else { current_w + 1 + word_w };
        if need > width {
            lines.push(current.clone());
            current.clear();
            current_w = 0;
            if lines.len() >= max_lines {
                break;
            }
            current.push_str(word);
            current_w = word_w;
        } else {
            if !current.is_empty() {
                current.push(' ');
                current_w += 1;
            }
            current.push_str(word);
            current_w += word_w;
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            // Replace last char with '…'.
            let mut acc = String::new();
            let mut acc_w = 0;
            let chars: Vec<char> = last.chars().collect();
            for ch in chars.iter().take(chars.len().saturating_sub(1)) {
                let cw = unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
                if acc_w + cw > width.saturating_sub(1) {
                    break;
                }
                acc.push(*ch);
                acc_w += cw;
            }
            acc.push('…');
            *last = acc;
        }
    }
    // Pad lines to full width.
    for line in lines.iter_mut() {
        let w = UnicodeWidthStr::width(line.as_str());
        if w < width {
            for _ in 0..(width - w) {
                line.push(' ');
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(name: &str, desc: &str, ty: SuggestionType) -> Suggestion {
        Suggestion {
            name: name.into(),
            description: if desc.is_empty() {
                None
            } else {
                Some(desc.into())
            },
            suggestion_type: ty,
            ..Default::default()
        }
    }

    #[test]
    fn wrap_short_fits_one_line() {
        let w = wrap_multiline("hello world", 28, 5);
        assert_eq!(w.len(), 1);
        assert_eq!(UnicodeWidthStr::width(w[0].as_str()), 28);
    }

    #[test]
    fn wrap_long_caps_at_max_lines() {
        let text = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen";
        let w = wrap_multiline(text, 10, 3);
        assert!(w.len() <= 3);
    }

    #[test]
    fn truncate_or_pad_exact_width() {
        let out = truncate_or_pad_wc("checkout", 38);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 38);
    }

    #[test]
    fn suggestion_box_top_bottom_match_width() {
        let sugs = vec![mk("checkout", "Switch branches", SuggestionType::Subcommand)];
        let box_ = render_suggestion_box(&sugs, 0);
        assert!(box_[0].contains("┌"));
        assert!(box_[0].ends_with('┐'));
        assert!(box_.last().unwrap().contains("└"));
        assert!(box_.last().unwrap().ends_with('┘'));
    }

    #[test]
    fn description_box_has_borders() {
        let box_ = render_description_box("Switch branches or restore working tree files");
        assert!(!box_.is_empty());
        assert!(box_[0].contains("┌"));
        assert!(box_.last().unwrap().contains("└"));
    }

    #[test]
    fn draw_full_emits_active_bg() {
        let mut buf: Vec<u8> = Vec::new();
        let mut r = PopupRenderer::new(5);
        let sugs = vec![
            mk("checkout", "Switch branches", SuggestionType::Subcommand),
            mk("cherry-pick", "Apply the changes", SuggestionType::Subcommand),
        ];
        r.draw_full(&mut buf, &sugs, 0, Direction::Below, 10, 120).unwrap();
        let s = String::from_utf8_lossy(&buf);
        // Active bg is either truecolor (48;2;125;86;244) or 256-color
        // indexed (48;5;105) depending on COLORTERM.
        assert!(
            s.contains("\x1b[48;2;125;86;244m") || s.contains("\x1b[48;5;105m"),
            "no active bg found: {s:?}"
        );
        assert!(s.contains("┌"));
        assert!(s.contains("└"));
        assert!(s.contains("📦"));
    }

    #[test]
    fn calculate_padding_no_swap_near_left_edge() {
        let (pad, swap) = calculate_padding(5, 120, "some desc");
        assert_eq!(pad, 5);
        assert!(!swap);
    }

    #[test]
    fn calculate_padding_swaps_near_right_edge() {
        let (_pad, swap) = calculate_padding(80, 120, "some desc");
        assert!(swap);
    }
}
