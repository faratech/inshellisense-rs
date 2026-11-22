//! Popup suggestion renderer — 1:1 port of upstream inshellisense's
//! `suggestionManager.render()` + `ui-root._render()` (see
//! upstream inshellisense's `src/ui/suggestionManager.ts` and `ui-root.ts`).
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
/// Below this the popup cannot be drawn without wrapping; skip it entirely
/// rather than smearing partial rows the clear path cannot erase.
pub const MIN_POPUP_WIDTH: usize = 12;
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

pub use crate::config::BoxBorderStyle;

impl BoxBorderStyle {
    pub const fn top_left(self) -> &'static str {
        match self {
            Self::Square => "┌",
            Self::Rounded => "╭",
        }
    }
    pub const fn top_right(self) -> &'static str {
        match self {
            Self::Square => "┐",
            Self::Rounded => "╮",
        }
    }
    pub const fn bottom_left(self) -> &'static str {
        match self {
            Self::Square => "└",
            Self::Rounded => "╰",
        }
    }
    pub const fn bottom_right(self) -> &'static str {
        match self {
            Self::Square => "┘",
            Self::Rounded => "╯",
        }
    }
}

pub fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    // Byte-length alone lets a 6-byte non-ASCII value through, and the
    // slicing below then panics on a char boundary (#107).
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        if r < 8 {
            16
        } else if r > 248 {
            231
        } else {
            (((r as u16 - 8) * 24) / 240) as u8 + 232
        }
    } else {
        let qr = (r as u16 * 5 + 127) / 255;
        let qg = (g as u16 * 5 + 127) / 255;
        let qb = (b as u16 * 5 + 127) / 255;
        (16 + 36 * qr + 6 * qg + qb) as u8
    }
}

pub fn compute_active_bg(hex: &str) -> String {
    let (r, g, b) = parse_hex_color(hex).unwrap_or((125, 86, 244));
    if is_truecolor() {
        format!("\x1b[48;2;{r};{g};{b}m")
    } else if (r, g, b) == (125, 86, 244) {
        "\x1b[48;5;105m".to_string()
    } else {
        let idx = rgb_to_ansi256(r, g, b);
        format!("\x1b[48;5;{idx}m")
    }
}

/// Upstream active-row background default.
#[cfg(test)]
fn active_bg_on() -> &'static str {
    use std::sync::LazyLock;
    static BG: LazyLock<&'static str> = LazyLock::new(|| {
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
    icons: IconSet,
    border_style: BoxBorderStyle,
    active_bg: String,
    description_width: usize,
    description_height: usize,
    truncate_descriptions: bool,
    last_drawn_rows: u16,
    last_direction: Direction,
    last_gap_rows: u16,
    last_signature: Option<u64>,
}

impl PopupRenderer {
    pub fn new(max_suggestions: u8, icons: IconSet) -> Self {
        Self::with_options(max_suggestions, icons, BoxBorderStyle::default(), None)
    }

    pub fn with_options(
        max_suggestions: u8,
        icons: IconSet,
        border_style: BoxBorderStyle,
        active_bg_hex: Option<&str>,
    ) -> Self {
        let active_bg = compute_active_bg(active_bg_hex.unwrap_or("#7D56F4"));
        Self {
            max_suggestions: max_suggestions.max(1),
            icons,
            border_style,
            active_bg,
            description_width: DESCRIPTION_WIDTH,
            description_height: DESCRIPTION_HEIGHT,
            truncate_descriptions: true,
            last_drawn_rows: 0,
            last_direction: Direction::Below,
            last_gap_rows: 0,
            last_signature: None,
        }
    }

    pub fn with_config(cfg: &crate::config::Config, icons: IconSet) -> Self {
        let mut popup = Self::with_options(
            cfg.max_suggestions,
            icons,
            cfg.box_border_style,
            Some(&cfg.active_suggestion_background_color),
        );
        popup.description_width = cfg.description_width.max(3);
        popup.description_height = cfg.description_height.max(1);
        popup.truncate_descriptions = cfg.truncate_descriptions;
        popup
    }

    pub fn desired_rows(cfg: &crate::config::Config, cols: u16) -> usize {
        let suggestions = cfg.max_suggestions.max(1) as usize;
        if !cfg.truncate_descriptions {
            usize::MAX
        } else if (cols as usize) < SUGGESTION_WIDTH.saturating_add(cfg.description_width) {
            (suggestions + 2 * BORDER_WIDTH).saturating_add(cfg.description_height)
        } else {
            suggestions
                .max(cfg.description_height)
                .saturating_add(BORDER_WIDTH)
        }
    }

    /// Legacy non-interactive entry point — renders with `cursor=0` and
    /// `direction=Below` centered at column 0.
    pub fn draw(
        &mut self,
        out: &mut impl Write,
        _tail: Option<&str>,
        all: &[Suggestion],
    ) -> io::Result<()> {
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
        self.draw_full_with_rows(
            out,
            all,
            cursor,
            direction,
            cursor_col,
            term_cols,
            usize::MAX,
        )
    }

    /// Fit both boxes in the screen rows available on the chosen side of the
    /// prompt. Page through fewer suggestions when necessary; drawing beyond
    /// the screen clamps cursor movement and causes clear() to erase the
    /// prompt itself (#129).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_full_with_rows(
        &mut self,
        out: &mut impl Write,
        all: &[Suggestion],
        cursor: usize,
        direction: Direction,
        cursor_col: u16,
        term_cols: u16,
        available_rows: usize,
    ) -> io::Result<()> {
        self.draw_full_with_gap(
            out,
            all,
            cursor,
            direction,
            cursor_col,
            term_cols,
            available_rows,
            0,
        )
    }

    /// Skip other rows occupied by the prompt or command between the cursor
    /// and the popup. `available_rows` excludes both that gap and the cursor.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_full_with_gap(
        &mut self,
        out: &mut impl Write,
        all: &[Suggestion],
        cursor: usize,
        direction: Direction,
        cursor_col: u16,
        term_cols: u16,
        available_rows: usize,
        gap_rows: u16,
    ) -> io::Result<()> {
        self.draw_with_argument_hint(
            out,
            all,
            None,
            cursor,
            direction,
            cursor_col,
            term_cols,
            available_rows,
            gap_rows,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_with_argument_hint(
        &mut self,
        out: &mut impl Write,
        all: &[Suggestion],
        argument_hint: Option<&str>,
        cursor: usize,
        direction: Direction,
        cursor_col: u16,
        term_cols: u16,
        available_rows: usize,
        gap_rows: u16,
    ) -> io::Result<()> {
        if (all.is_empty() && argument_hint.is_none()) || available_rows < BORDER_WIDTH + 1 {
            self.clear(out)?;
            return Ok(());
        }

        let cols = term_cols as usize;
        if cols < MIN_POPUP_WIDTH {
            self.clear(out)?;
            return Ok(());
        }

        let max = (self.max_suggestions as usize).min(available_rows - BORDER_WIDTH);
        let cursor = cursor.min(all.len().saturating_sub(1));
        let start = cursor / max * max;
        let visible = &all[start..(start + max).min(all.len())];
        let active_in_page = cursor - start;
        let description = all
            .get(cursor)
            .and_then(|s| s.description.as_deref())
            .filter(|s| !s.is_empty())
            .or(argument_hint)
            .unwrap_or_default();
        // Treat whitespace as word boundaries and make other controls visible.
        let description = description.replace(char::is_whitespace, " ");
        let active_desc = super::printable(&description);
        let active_desc = active_desc.trim();
        let sug_width = SUGGESTION_WIDTH.min(cols);
        let stacked = visible.is_empty() || cols < sug_width.saturating_add(self.description_width);
        let (base_width, width_limit, row_limit) = if visible.is_empty() {
            (self.description_width, cols, available_rows)
        } else if stacked {
            (
                sug_width.max(self.description_width),
                cols,
                available_rows.saturating_sub(visible.len() + BORDER_WIDTH),
            )
        } else {
            (self.description_width, cols - sug_width, available_rows)
        };
        let (description_width, description_height) =
            if active_desc.is_empty() || width_limit <= BORDER_WIDTH || row_limit <= BORDER_WIDTH {
                (0, 0)
            } else {
                let height_limit = row_limit - BORDER_WIDTH;
                if self.truncate_descriptions {
                    (
                        base_width.min(width_limit),
                        self.description_height.min(height_limit),
                    )
                } else {
                    expanded_description_size(
                        active_desc,
                        base_width,
                        self.description_height,
                        width_limit,
                        height_limit,
                    )
                }
            };
        if visible.is_empty() && description_height == 0 {
            return self.clear(out);
        }
        let panel_width = if visible.is_empty() {
            description_width
        } else if stacked {
            sug_width.max(description_width)
        } else {
            sug_width
        };
        let side_width = if stacked { 0 } else { description_width };
        let (padding, swap_description) =
            calculate_padding(cursor_col, term_cols, side_width, panel_width);

        let suggestion_box = if visible.is_empty() {
            Vec::new()
        } else {
            render_suggestion_box(
                visible,
                active_in_page,
                self.icons,
                panel_width,
                self.border_style,
                &self.active_bg,
            )
        };
        let description_box = render_description_box(
            active_desc,
            self.border_style,
            description_width,
            description_height,
        );

        // Signature hash: skip redraw when nothing visible has changed.
        // Uses a u64 hash instead of formatting a 300-byte String to
        // avoid per-draw allocations. Every input that decides the emitted
        // bytes must be hashed: the swap decision and the box widths change
        // the layout even when the visible names, active row, description
        // and padding all stay the same (e.g. a cursor jump past the
        // padding limit flips the description box to the other side).
        let layout = Layout {
            padding,
            swap_description,
            sug_width,
            term_cols,
            description_height,
            description_width,
            stacked,
            gap_rows,
        };
        let sig = signature_hash(
            visible,
            active_in_page,
            direction,
            active_desc,
            &layout,
            self.icons,
            self.border_style,
            &self.active_bg,
        );
        if Some(sig) == self.last_signature && self.last_direction == direction {
            return Ok(());
        }
        let rows: Vec<_> = if stacked {
            suggestion_box
                .into_iter()
                .chain(description_box)
                .map(|row| (padding, row))
                .collect()
        } else {
            let height = suggestion_box.len().max(description_box.len());
            let row_index = |row: usize, box_height: usize| match direction {
                Direction::Above => row.checked_sub(height - box_height),
                Direction::Below => Some(row),
            };
            (0..height)
                .map(|row| {
                    let suggestion = row_index(row, suggestion_box.len())
                        .and_then(|index| suggestion_box.get(index));
                    let description = row_index(row, description_box.len())
                        .and_then(|index| description_box.get(index));
                    let row_padding = calc_row_padding(
                        padding,
                        swap_description,
                        suggestion.is_some(),
                        description.is_some(),
                        sug_width,
                        description_width,
                    );
                    let suggestion = suggestion.map_or("", String::as_str);
                    let description = description.map_or("", String::as_str);
                    let data = if swap_description {
                        format!("{description}{suggestion}")
                    } else {
                        format!("{suggestion}{description}")
                    };
                    (row_padding, data)
                })
                .collect()
        };
        self.clear(out)?;

        // Upstream uses SCO save/restore (`\x1b[s` / `\x1b[u`) rather
        // than DECSC/DECRC (`\x1b7` / `\x1b8`). Some terminals only
        // implement one correctly, and chalk/ansi-escapes settled on
        // SCO — we match for byte-for-byte parity.
        out.write_all(ansi::CURSOR_HIDE.as_bytes())?;
        out.write_all(b"\x1b[s")?;

        match direction {
            Direction::Below => {
                if gap_rows > 0 {
                    write!(out, "\x1b[{}E", usize::from(gap_rows) + 1)?;
                } else {
                    out.write_all(b"\x1b[E")?; // CNL with no count
                }
            }
            Direction::Above => {
                if gap_rows > 0 {
                    write!(out, "\x1b[{}F", rows.len() + usize::from(gap_rows))?;
                } else {
                    // Preserve the upstream byte pattern for adjacent popups.
                    for _ in 0..rows.len() {
                        out.write_all(b"\x1b[F")?;
                    }
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
        self.last_gap_rows = gap_rows;
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
                if self.last_gap_rows > 0 {
                    write!(out, "\x1b[{}E", usize::from(self.last_gap_rows) + 1)?;
                } else {
                    out.write_all(b"\x1b[E")?;
                }
            }
            Direction::Above => {
                if self.last_gap_rows > 0 {
                    write!(
                        out,
                        "\x1b[{}F",
                        usize::from(rows) + usize::from(self.last_gap_rows)
                    )?;
                } else {
                    for _ in 0..rows {
                        out.write_all(b"\x1b[F")?;
                    }
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

    pub fn is_visible(&self) -> bool {
        self.last_drawn_rows > 0
    }
}

// ---------- upstream layout helpers ----------

/// Port of `_calculatePadding` from suggestionManager.ts.
fn calculate_padding(
    cursor_col: u16,
    term_cols: u16,
    description_width: usize,
    sug_width: usize,
) -> (usize, bool) {
    let cols = term_cols.max(1) as usize;
    let wrapped_padding = (cursor_col as usize) % cols;
    let max_padding = if description_width > 0 {
        cols.saturating_sub(sug_width + description_width)
    } else {
        cols.saturating_sub(sug_width)
    };
    let swap_description = wrapped_padding > max_padding && description_width > 0;
    let swapped_padding = if swap_description {
        wrapped_padding.saturating_sub(description_width)
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
    sug_width: usize,
    description_width: usize,
) -> usize {
    if swap_description {
        if !has_description {
            padding + description_width
        } else {
            padding
        }
    } else if !has_suggestion {
        padding + sug_width
    } else {
        padding
    }
}

#[inline]
fn repeat_bar(inner: usize) -> std::borrow::Cow<'static, str> {
    const BARS: &str = "────────────────────────────────────────────────────────────────────────────────────────────────────";
    let bytes = inner * 3;
    if bytes <= BARS.len() {
        std::borrow::Cow::Borrowed(&BARS[..bytes])
    } else {
        std::borrow::Cow::Owned("─".repeat(inner))
    }
}

/// Port of `_renderSuggestions` → `renderBox(...)`.
///
/// Byte format matches upstream exactly: each middle row is
/// `\x1b[0m│<bg_on><38-cell padded text><bg_off>\x1b[0m│`, so the border
/// bars are outside the highlighted region and chalk-style `\x1b[0m`
/// resets bracket every color change.
fn render_suggestion_box(
    visible: &[Suggestion],
    active_in_page: usize,
    icons: IconSet,
    width: usize,
    border_style: BoxBorderStyle,
    active_bg: &str,
) -> Vec<String> {
    let inner = width.saturating_sub(BORDER_WIDTH); // cells between the borders
    let mut out = Vec::with_capacity(visible.len() + 2);
    let bar = repeat_bar(inner);
    // Top border
    out.push(format!(
        "\x1b[0m{}{}{}",
        border_style.top_left(),
        bar,
        border_style.top_right()
    ));
    for (idx, s) in visible.iter().enumerate() {
        // Names come from filenames and generator output; sanitize before
        // the width math, which counts control characters as zero width.
        let text = format!("{} {}", icon_for(s, icons), super::printable(&s.name));
        let padded = truncate_or_pad_wc(&text, inner);
        let body = if idx == active_in_page {
            format!("{}{}{}\x1b[0m", active_bg, padded, ACTIVE_BG_OFF)
        } else {
            padded
        };
        out.push(format!("\x1b[0m│{}│", body));
    }
    // Bottom border
    out.push(format!(
        "\x1b[0m{}{}{}",
        border_style.bottom_left(),
        bar,
        border_style.bottom_right()
    ));
    out
}

/// Port of `_renderDescription` → `renderBox(truncateMultilineText(...))`.
fn render_description_box(
    description: &str,
    border_style: BoxBorderStyle,
    width: usize,
    max_lines: usize,
) -> Vec<String> {
    if description.is_empty() || max_lines == 0 {
        return Vec::new();
    }
    let inner = width - BORDER_WIDTH; // 28 cells
    let lines = wrap_multiline(description, inner, max_lines);
    let mut out = Vec::with_capacity(lines.len() + 2);
    let bar = repeat_bar(inner);
    out.push(format!(
        "\x1b[0m{}{}{}",
        border_style.top_left(),
        bar,
        border_style.top_right()
    ));
    for line in &lines {
        out.push(format!("\x1b[0m│{line}│"));
    }
    out.push(format!(
        "\x1b[0m{}{}{}",
        border_style.bottom_left(),
        bar,
        border_style.bottom_right()
    ));
    out
}

/// The layout inputs that decide how a popup's rows are emitted, alongside
/// the visible names and the active row.
#[derive(Debug, Clone, Copy)]
struct Layout {
    padding: usize,
    swap_description: bool,
    sug_width: usize,
    term_cols: u16,
    description_height: usize,
    description_width: usize,
    stacked: bool,
    gap_rows: u16,
}

#[allow(clippy::too_many_arguments)]
fn signature_hash(
    visible: &[Suggestion],
    active: usize,
    direction: Direction,
    desc: &str,
    layout: &Layout,
    icons: IconSet,
    border_style: BoxBorderStyle,
    active_bg: &str,
) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in visible {
        s.name.hash(&mut h);
        // Every field that decides the emitted row bytes must count
        // (#109): the icon comes from type/icon, and a reclassified
        // entry (File -> Folder) previously kept the stale icon.
        s.display_name.hash(&mut h);
        s.icon.hash(&mut h);
        std::mem::discriminant(&s.suggestion_type).hash(&mut h);
        s.description.hash(&mut h);
    }
    active.hash(&mut h);
    (direction == Direction::Above).hash(&mut h);
    desc.hash(&mut h);
    layout.padding.hash(&mut h);
    layout.swap_description.hash(&mut h);
    layout.sug_width.hash(&mut h);
    layout.term_cols.hash(&mut h);
    layout.description_height.hash(&mut h);
    layout.description_width.hash(&mut h);
    layout.stacked.hash(&mut h);
    layout.gap_rows.hash(&mut h);
    std::mem::discriminant(&icons).hash(&mut h);
    std::mem::discriminant(&border_style).hash(&mut h);
    active_bg.hash(&mut h);
    h.finish()
}

/// Which glyph table `icon_for` draws from. Selected by the `use_nerd_font`
/// config key: terminals without a Nerd Font render those glyphs as tofu, so
/// emoji stay the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconSet {
    #[default]
    Emoji,
    /// Nerd Fonts' Font Awesome range (U+F000–U+F2FF).
    NerdFont,
}

impl IconSet {
    pub fn from_config(use_nerd_font: bool) -> Self {
        if use_nerd_font {
            IconSet::NerdFont
        } else {
            IconSet::Emoji
        }
    }
}

/// Upstream icon table (from upstream's `src/runtime/suggestion.ts`).
pub fn icon_for(s: &Suggestion, icons: IconSet) -> &'static str {
    // Upstream first checks whether the spec's `icon` is already a
    // non-ASCII Unicode glyph (i.e. an actual emoji) and passes it
    // through. We don't store that in the extracted specs, so we fall
    // back to the suggestion type mapping.
    if let Some(icon) = s.icon.as_deref()
        && let Some(mapped) = icon_from_fig_uri(icon, icons)
    {
        return mapped;
    }
    icon_for_type(s.suggestion_type, icons)
}

fn icon_for_type(kind: SuggestionType, icons: IconSet) -> &'static str {
    match (kind, icons) {
        (SuggestionType::File, IconSet::Emoji) => "📄",
        (SuggestionType::Folder, IconSet::Emoji) => "📁",
        (SuggestionType::Subcommand, IconSet::Emoji) => "📦",
        (SuggestionType::Option, IconSet::Emoji) => "🔗",
        (SuggestionType::Arg, IconSet::Emoji) => "💲",
        (SuggestionType::Mixin, IconSet::Emoji) => "🏝",
        (SuggestionType::Shortcut, IconSet::Emoji) => "🔥",
        (SuggestionType::Special, IconSet::Emoji) => "⭐",
        // nf-fa-* glyphs; all single-width.
        (SuggestionType::File, IconSet::NerdFont) => "\u{f15b}",
        (SuggestionType::Folder, IconSet::NerdFont) => "\u{f07b}",
        (SuggestionType::Subcommand, IconSet::NerdFont) => "\u{f1b2}",
        (SuggestionType::Option, IconSet::NerdFont) => "\u{f0c1}",
        (SuggestionType::Arg, IconSet::NerdFont) => "\u{f155}",
        (SuggestionType::Mixin, IconSet::NerdFont) => "\u{f0ac}",
        (SuggestionType::Shortcut, IconSet::NerdFont) => "\u{f0e7}",
        (SuggestionType::Special, IconSet::NerdFont) => "\u{f005}",
    }
}

fn icon_from_fig_uri(icon: &str, icons: IconSet) -> Option<&'static str> {
    let kind = match icon {
        "fig://icon?type=folder" => SuggestionType::Folder,
        "fig://icon?type=file" => SuggestionType::File,
        "fig://icon?type=option" => SuggestionType::Option,
        "fig://icon?type=command" => SuggestionType::Subcommand,
        "fig://icon?type=string" => SuggestionType::Arg,
        _ => return None,
    };
    Some(icon_for_type(kind, icons))
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
    if current > width {
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

/// Return up to one extra row so the caller can detect clipped text.
fn wrapped_rows(text: &str, width: usize, limit: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut line = String::new();
    let mut cells = 0;
    for word in text.split_whitespace() {
        let word_cells = UnicodeWidthStr::width(word);
        if !line.is_empty() && cells + 1 + word_cells > width {
            rows.push(std::mem::take(&mut line));
            if rows.len() == limit {
                return rows;
            }
            cells = 0;
        }
        if !line.is_empty() {
            line.push(' ');
            cells += 1;
        }
        if word_cells <= width {
            line.push_str(word);
            cells += word_cells;
            continue;
        }
        for ch in word.chars() {
            let char_cells = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let (ch, char_cells) = if char_cells > width {
                ('…', 1)
            } else {
                (ch, char_cells)
            };
            if cells + char_cells > width {
                rows.push(std::mem::take(&mut line));
                if rows.len() == limit {
                    return rows;
                }
                cells = 0;
            }
            line.push(ch);
            cells += char_cells;
        }
    }
    if !line.is_empty() {
        rows.push(line);
    }
    rows
}

fn expanded_description_size(
    text: &str,
    base_width: usize,
    base_height: usize,
    width_limit: usize,
    height_limit: usize,
) -> (usize, usize) {
    let mut width = base_width.min(width_limit);
    let mut height = base_height.min(height_limit);
    let usable_width = UnicodeWidthStr::width(text).saturating_add(BORDER_WIDTH);
    let width_limit = width_limit.min(usable_width.max(width));
    loop {
        let rows = wrapped_rows(text, width - BORDER_WIDTH, height_limit.saturating_add(1));
        if rows.len() <= height {
            return (width, height);
        }
        if width == width_limit {
            return (width, rows.len().min(height_limit));
        }
        width += 1;
        height = base_height
            .saturating_mul(width)
            .div_ceil(base_width)
            .min(height_limit);
    }
}

fn wrap_multiline(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut rows = wrapped_rows(text, width, max_lines.saturating_add(1));
    if rows.len() > max_lines {
        rows.truncate(max_lines);
        if let Some(last) = rows.last_mut() {
            let mut end = last.len();
            while UnicodeWidthStr::width(&last[..end]) >= width && end > 0 {
                end = last[..end].char_indices().next_back().unwrap().0;
            }
            last.truncate(end);
            last.push('…');
        }
    }
    rows.iter().map(|row| pad_right_wc(row, width)).collect()
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

    /// #107: a 6-byte non-ASCII color must fall back, not panic.
    #[test]
    fn non_ascii_color_falls_back() {
        assert_eq!(parse_hex_color("#a€bc"), None);
        assert_eq!(parse_hex_color("#€abcde"), None);
        assert_eq!(parse_hex_color("#7D56F4"), Some((125, 86, 244)));
        assert!(!compute_active_bg("#a€bc").is_empty());
    }

    #[test]
    fn wrap_short_fits_one_line() {
        let w = wrap_multiline("hello world", 28, 5);
        assert_eq!(w.len(), 1);
        assert_eq!(UnicodeWidthStr::width(w[0].as_str()), 28);
    }

    #[test]
    fn wrap_long_caps_at_max_lines() {
        let text =
            "one two three four five six seven eight nine ten eleven twelve thirteen fourteen";
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
        let sugs = vec![mk(
            "checkout",
            "Switch branches",
            SuggestionType::Subcommand,
        )];
        let box_ = render_suggestion_box(
            &sugs,
            0,
            IconSet::default(),
            SUGGESTION_WIDTH,
            BoxBorderStyle::Square,
            active_bg_on(),
        );
        assert!(box_[0].contains("┌"));
        assert!(box_[0].ends_with('┐'));
        assert!(box_.last().unwrap().contains("└"));
        assert!(box_.last().unwrap().ends_with('┘'));

        let rounded_box = render_suggestion_box(
            &sugs,
            0,
            IconSet::default(),
            SUGGESTION_WIDTH,
            BoxBorderStyle::Rounded,
            active_bg_on(),
        );
        assert!(rounded_box[0].contains("╭"));
        assert!(rounded_box[0].ends_with('╮'));
        assert!(rounded_box.last().unwrap().contains("╰"));
        assert!(rounded_box.last().unwrap().ends_with('╯'));
    }

    #[test]
    fn description_box_has_borders() {
        let box_ = render_description_box(
            "Switch branches or restore working tree files",
            BoxBorderStyle::Square,
            DESCRIPTION_WIDTH,
            DESCRIPTION_HEIGHT,
        );
        assert!(!box_.is_empty());
        assert!(box_[0].contains("┌"));
        assert!(box_.last().unwrap().contains("└"));

        let rounded = render_description_box(
            "Switch branches or restore working tree files",
            BoxBorderStyle::Rounded,
            DESCRIPTION_WIDTH,
            DESCRIPTION_HEIGHT,
        );
        assert!(!rounded.is_empty());
        assert!(rounded[0].contains("╭"));
        assert!(rounded.last().unwrap().contains("╰"));
    }

    #[test]
    fn short_terminal_popup_preserves_prompt_and_pages_every_suggestion() {
        let suggestions: Vec<_> = (0..5)
            .map(|i| {
                mk(
                    &format!("choice_{i}"),
                    &"description ".repeat(30),
                    SuggestionType::Arg,
                )
            })
            .collect();
        for direction in [Direction::Above, Direction::Below] {
            for cursor in 0..suggestions.len() {
                let mut screen = vt100::Parser::new(4, 80, 0);
                let prompt = match direction {
                    Direction::Above => b"\x1b[4;1H> git".as_slice(),
                    Direction::Below => b"> git".as_slice(),
                };
                screen.process(prompt);
                let before = screen.screen().contents();
                let position = screen.screen().cursor_position();
                let mut renderer = PopupRenderer::new(5, IconSet::Emoji);
                let mut bytes = Vec::new();
                renderer
                    .draw_full_with_rows(&mut bytes, &suggestions, cursor, direction, 5, 80, 3)
                    .unwrap();
                assert_eq!(renderer.last_drawn_rows, 3);
                let painted = String::from_utf8_lossy(&bytes);
                assert!(painted.contains(&format!("choice_{cursor}")));
                assert_eq!(painted.matches("choice_").count(), 1);
                // vt100 implements DEC save/restore, but not the SCO aliases
                // used by the popup. Translate only those equivalent controls
                // so the screen assertion exercises the actual paint/clear.
                let vt_bytes = |bytes: &[u8]| {
                    String::from_utf8_lossy(bytes)
                        .replace("\x1b[s", "\x1b7")
                        .replace("\x1b[u", "\x1b8")
                };
                screen.process(vt_bytes(&bytes).as_bytes());
                assert_eq!(screen.screen().cursor_position(), position);
                assert!(screen.screen().contents().contains("> git"));
                bytes.clear();
                renderer.clear(&mut bytes).unwrap();
                screen.process(vt_bytes(&bytes).as_bytes());
                assert_eq!(screen.screen().contents(), before);
                assert_eq!(screen.screen().cursor_position(), position);
            }
        }
    }

    #[test]
    fn popup_skips_multiline_prompt_and_both_halves_of_wrapped_command() {
        use crate::{ansi::IsEvent, term::TermTracker};
        let suggestions = vec![mk("status", "Repository status", SuggestionType::Arg)];
        for direction in [Direction::Above, Direction::Below] {
            let mut tracker = TermTracker::new(12, 20);
            let mut screen = vt100::Parser::new(12, 20, 0);
            let paint = b"\r\n\r\n\r\n\r\nfirst prompt line\r\n> abcdefghijklmnopqrstuvwxyz0123456789abcdef\x1b[A\r\x1b[5C";
            tracker.feed_ordered(
                paint,
                &[(8, IsEvent::PromptStart), (29, IsEvent::PromptEnd)],
            );
            screen.process(paint);
            let protected = screen.screen().clone();
            let before = screen.screen().contents();
            let position = screen.screen().cursor_position();
            let (first, last) = tracker.popup_region();
            assert_eq!((first, last), (4, 7));
            assert_eq!(position.0, 6);
            let (available, gap) = match direction {
                Direction::Above => (first, position.0 - first),
                Direction::Below => (12 - last - 1, last - position.0),
            };
            let mut renderer = PopupRenderer::new(5, IconSet::Emoji);
            let mut bytes = Vec::new();
            renderer
                .draw_full_with_gap(
                    &mut bytes,
                    &suggestions,
                    0,
                    direction,
                    position.1,
                    20,
                    available as usize,
                    gap,
                )
                .unwrap();
            let vt_bytes = |bytes: &[u8]| {
                String::from_utf8_lossy(bytes)
                    .replace("\x1b[s", "\x1b7")
                    .replace("\x1b[u", "\x1b8")
            };
            screen.process(vt_bytes(&bytes).as_bytes());
            // Every protected cell survives while the popup is visible too.
            for row in first..=last {
                for col in 0..20 {
                    assert_eq!(screen.screen().cell(row, col), protected.cell(row, col));
                }
            }
            bytes.clear();
            renderer.clear(&mut bytes).unwrap();
            screen.process(vt_bytes(&bytes).as_bytes());
            assert_eq!(screen.screen().contents(), before);
            assert_eq!(screen.screen().cursor_position(), position);
        }
    }

    #[test]
    fn changed_gap_invalidates_popup_redraw_cache() {
        let suggestions = vec![mk("status", "Repository status", SuggestionType::Arg)];
        let mut renderer = PopupRenderer::new(5, IconSet::Emoji);
        let mut bytes = Vec::new();
        renderer
            .draw_full_with_gap(&mut bytes, &suggestions, 0, Direction::Above, 5, 80, 4, 0)
            .unwrap();
        bytes.clear();
        renderer
            .draw_full_with_gap(&mut bytes, &suggestions, 0, Direction::Above, 5, 80, 4, 1)
            .unwrap();
        assert!(!bytes.is_empty());
        bytes.clear();
        renderer
            .draw_full_with_gap(&mut bytes, &suggestions, 0, Direction::Above, 5, 80, 4, 1)
            .unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn short_popup_emits_less_output_and_hides_without_room_for_one_row() {
        let suggestions: Vec<_> = (0..5)
            .map(|i| {
                mk(
                    &format!("choice_{i}"),
                    &"description ".repeat(30),
                    SuggestionType::Arg,
                )
            })
            .collect();
        let mut renderer = PopupRenderer::new(5, IconSet::Emoji);
        let mut full = Vec::new();
        renderer
            .draw_full(&mut full, &suggestions, 0, Direction::Above, 5, 80)
            .unwrap();
        let mut compact = Vec::new();
        let mut renderer = PopupRenderer::new(5, IconSet::Emoji);
        renderer
            .draw_full_with_rows(&mut compact, &suggestions, 0, Direction::Above, 5, 80, 3)
            .unwrap();
        assert!(
            compact.len() * 3 < full.len() * 2,
            "compact={} full={}",
            compact.len(),
            full.len()
        );
        let mut clear = Vec::new();
        renderer
            .draw_full_with_rows(&mut clear, &suggestions, 0, Direction::Above, 5, 80, 2)
            .unwrap();
        assert!(!renderer.is_visible());
        assert!(!clear.is_empty(), "previous popup should be cleared");
        clear.clear();
        renderer
            .draw_full_with_rows(&mut clear, &suggestions, 0, Direction::Above, 5, 80, 0)
            .unwrap();
        assert!(clear.is_empty());
    }

    #[test]
    fn increased_height_redraws_a_previously_truncated_description() {
        let suggestions = vec![mk("one", &"description ".repeat(30), SuggestionType::Arg)];
        let mut renderer = PopupRenderer::new(5, IconSet::Emoji);
        let mut out = Vec::new();
        renderer
            .draw_full_with_rows(&mut out, &suggestions, 0, Direction::Below, 0, 80, 3)
            .unwrap();
        assert_eq!(renderer.last_drawn_rows, 3);
        out.clear();
        renderer
            .draw_full_with_rows(&mut out, &suggestions, 0, Direction::Below, 0, 80, 7)
            .unwrap();
        assert_eq!(renderer.last_drawn_rows, 7);
        assert!(!out.is_empty());
    }

    /// #109: same name, different type/icon must force a redraw.
    #[test]
    fn type_change_forces_redraw() {
        let mut r = PopupRenderer::new(5, IconSet::Emoji);
        let mut out = Vec::new();
        r.draw_full(
            &mut out,
            &[mk("thing", "", SuggestionType::File)],
            0,
            Direction::Below,
            0,
            80,
        )
        .unwrap();
        assert!(!out.is_empty());
        let mut out2 = Vec::new();
        r.draw_full(
            &mut out2,
            &[mk("thing", "", SuggestionType::Folder)],
            0,
            Direction::Below,
            0,
            80,
        )
        .unwrap();
        assert!(
            !out2.is_empty(),
            "Folder redraw was suppressed; stale File icon remains"
        );
    }

    /// #110: Above boxes share the bottom row, not the top.
    #[test]
    fn above_boxes_share_bottom_row() {
        let mut r = PopupRenderer::new(5, IconSet::Emoji);
        let sugs = vec![mk(
            "one",
            "line1\nline2\nline3\nline4\nline5",
            SuggestionType::Arg,
        )];
        let mut out = Vec::new();
        r.draw_full(&mut out, &sugs, 0, Direction::Above, 0, 120)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        // Rows split on CNL; the suggestion box bottom border must be in
        // the last row chunk, flush with the description bottom border.
        let last = text.rsplit("\x1b[E").next().unwrap();
        assert!(
            last.matches('└').count() >= 2,
            "bottom borders not shared: {text:?}"
        );
    }

    #[test]
    fn draw_full_emits_active_bg() {
        let mut buf: Vec<u8> = Vec::new();
        let mut r = PopupRenderer::new(5, IconSet::default());
        let sugs = vec![
            mk("checkout", "Switch branches", SuggestionType::Subcommand),
            mk(
                "cherry-pick",
                "Apply the changes",
                SuggestionType::Subcommand,
            ),
        ];
        r.draw_full(&mut buf, &sugs, 0, Direction::Below, 10, 120)
            .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("\x1b[48;2;125;86;244m") || s.contains("\x1b[48;5;105m"),
            "no active bg found: {s:?}"
        );
        assert!(s.contains("┌"));
        assert!(s.contains("└"));
        assert!(s.contains("📦"));
    }

    /// Names and descriptions can come from filenames, `package.json` and
    /// `cargo metadata`. Their control characters must never reach the
    /// terminal as escape sequences (#85).
    #[test]
    fn names_and_descriptions_never_emit_control_characters() {
        let sugs = vec![
            mk(
                "ev\x1b]0;pwned\x07il",
                "run \x1b]52;c;aGk=\x07 now\nthen",
                SuggestionType::Arg,
            ),
            mk("x\u{9b}2Jy", "", SuggestionType::File),
        ];
        let mut r = PopupRenderer::new(5, IconSet::default());
        let mut out = Vec::new();
        r.draw_full(&mut out, &sugs, 0, Direction::Below, 0, 120)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("\x1b]"),
            "OSC reached the terminal: {text:?}"
        );
        assert!(!text.contains('\x07'), "BEL reached the terminal: {text:?}");
        assert!(!text.contains('\u{9b}'), "C1 CSI reached the terminal");
        assert!(text.contains("ev?]0;pwned?il"), "{text:?}");
        assert!(text.contains("x?2Jy"), "{text:?}");
        // The description's newline still separates words.
        assert!(text.contains("?]52;c;aGk=?"), "{text:?}");
        assert!(text.contains("now then"), "{text:?}");
        // Every visible row still fits the terminal.
        for line in visible_rows(&text) {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 120, "{line:?}");
        }
    }

    #[test]
    fn draw_full_honors_rounded_borders_and_custom_color() {
        let mut buf: Vec<u8> = Vec::new();
        let mut r = PopupRenderer::with_options(
            5,
            IconSet::default(),
            BoxBorderStyle::Rounded,
            Some("#2E7D32"),
        );
        let sugs = vec![mk(
            "checkout",
            "Switch branches",
            SuggestionType::Subcommand,
        )];
        r.draw_full(&mut buf, &sugs, 0, Direction::Below, 10, 120)
            .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("╭"));
        assert!(s.contains("╰"));
        // #2E7D32 is (46, 125, 50) -> truecolor \x1b[48;2;46;125;50m or 256-color cube
        assert!(
            s.contains("\x1b[48;2;46;125;50m") || s.contains("\x1b[48;5;"),
            "custom active bg not found: {s:?}"
        );
    }

    #[test]
    fn calculate_padding_no_swap_near_left_edge() {
        let (pad, swap) = calculate_padding(5, 120, DESCRIPTION_WIDTH, SUGGESTION_WIDTH);
        assert_eq!(pad, 5);
        assert!(!swap);
    }

    #[test]
    fn calculate_padding_swaps_near_right_edge() {
        let (_pad, swap) = calculate_padding(80, 120, DESCRIPTION_WIDTH, SUGGESTION_WIDTH);
        assert!(swap);
    }

    /// Every emitted row must fit the terminal. A wrapped row costs two
    /// physical lines while `last_drawn_rows` counts one, so `clear` erased
    /// the wrong lines and left the popup on screen.
    #[test]
    fn rows_never_exceed_the_terminal_width() {
        let sugs: Vec<Suggestion> = (0..3)
            .map(|i| {
                mk(
                    &format!("subcommand-{i}"),
                    "A fairly long description that wraps",
                    SuggestionType::Subcommand,
                )
            })
            .collect();
        for cols in [12u16, 20, 39, 40, 55, 69, 70, 100] {
            let mut r = PopupRenderer::new(5, IconSet::default());
            let mut out = Vec::new();
            r.draw_full(&mut out, &sugs, 0, Direction::Below, 0, cols)
                .unwrap();
            let text = String::from_utf8(out).unwrap();
            for line in visible_rows(&text) {
                let width = UnicodeWidthStr::width(line.as_str());
                assert!(
                    width <= cols as usize,
                    "row of {width} cells in a {cols}-col terminal: {line:?}"
                );
            }
        }
    }

    /// Terminals too narrow for even a minimal box draw nothing at all.
    #[test]
    fn very_narrow_terminal_draws_no_popup() {
        let sugs = vec![mk(
            "checkout",
            "Switch branches",
            SuggestionType::Subcommand,
        )];
        let mut r = PopupRenderer::new(5, IconSet::default());
        let mut out = Vec::new();
        r.draw_full(&mut out, &sugs, 0, Direction::Below, 0, 8)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('┌'), "popup drawn in an 8-column terminal");
    }

    #[test]
    fn description_stacks_on_narrow_terminals_with_room() {
        let suggestions = [mk(
            "checkout",
            "Switch branches",
            SuggestionType::Subcommand,
        )];
        let mut popup = PopupRenderer::new(5, IconSet::default());
        let mut out = Vec::new();
        popup
            .draw_full_with_rows(&mut out, &suggestions, 0, Direction::Below, 0, 50, 6)
            .unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("Switch branches"));
        assert_eq!(popup.last_drawn_rows, 6);
        out.clear();
        popup
            .draw_full_with_rows(&mut out, &suggestions, 0, Direction::Above, 0, 50, 5)
            .unwrap();
        assert!(!String::from_utf8_lossy(&out).contains("Switch branches"));
        assert_eq!(popup.last_drawn_rows, 3);
        out.clear();
        popup
            .draw_full_with_rows(&mut out, &suggestions, 0, Direction::Above, 0, 100, 5)
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("Switch branches"));
        assert_eq!(popup.last_drawn_rows, 3);
        popup.clear(&mut out).unwrap();
        assert!(!popup.is_visible());
    }

    #[test]
    fn description_expands_and_clips_by_display_width() {
        let description = "漢字 漢字 漢字";
        let mut cfg = crate::config::Config {
            description_width: 6,
            description_height: 1,
            ..Default::default()
        };
        let suggestions = [mk("checkout", description, SuggestionType::Subcommand)];
        let mut out = Vec::new();
        PopupRenderer::with_config(&cfg, IconSet::default())
            .draw_full_with_rows(&mut out, &suggestions, 0, Direction::Below, 0, 60, 6)
            .unwrap();
        assert!(String::from_utf8_lossy(&out).contains('…'));
        cfg.truncate_descriptions = false;
        out.clear();
        PopupRenderer::with_config(&cfg, IconSet::default())
            .draw_full_with_rows(&mut out, &suggestions, 0, Direction::Below, 0, 60, 6)
            .unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(!text.contains('…'));
        for row in visible_rows(&text) {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 60);
        }
        assert_eq!(wrap_multiline("漢字", 1, 1), ["…"]);
    }

    /// Strip escapes and split on the CNL the renderer emits between rows.
    fn visible_rows(text: &str) -> Vec<String> {
        let mut rows = vec![String::new()];
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                rows.last_mut().unwrap().push(ch);
                continue;
            }
            // CSI ... final-byte
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        match c {
                            // CNL / CPL start a new row.
                            'E' | 'F' => rows.push(String::new()),
                            // CUF by N pads the row.
                            'C' => {
                                let n: usize = params.parse().unwrap_or(1);
                                rows.last_mut().unwrap().push_str(&" ".repeat(n));
                            }
                            _ => {}
                        }
                        break;
                    }
                    params.push(c);
                }
            }
        }
        rows.into_iter().filter(|r| !r.trim().is_empty()).collect()
    }

    /// A layout-relevant change must force a redraw even when the previously
    /// hashed inputs coincide. At 80 columns with a description, drawing at
    /// cursor column 0 and then at column 11 flips `swap_description` (the
    /// description box belongs on the LEFT of the second draw) while the
    /// padding saturates to 0 in both cases — the old signature hashed only
    /// the padding, so the redraw was skipped and the boxes stayed swapped.
    #[test]
    fn swap_flip_forces_a_redraw() {
        let sugs = vec![mk(
            "checkout",
            "Switch branches or restore working tree files",
            SuggestionType::Subcommand,
        )];
        let mut r = PopupRenderer::new(5, IconSet::default());
        let mut out: Vec<u8> = Vec::new();
        r.draw_full(&mut out, &sugs, 0, Direction::Below, 0, 80)
            .unwrap();
        assert!(!out.is_empty());
        let after_first = out.len();
        // Same suggestions, same active row, same direction, same padding:
        // only the swap decision changed.
        r.draw_full(&mut out, &sugs, 0, Direction::Below, 11, 80)
            .unwrap();
        assert!(out.len() > after_first, "swapped layout was not redrawn");
        // The redrawn rows must place the description box on the left.
        let text = String::from_utf8_lossy(&out[after_first..]).to_string();
        let rows = visible_rows(&text);
        let first_data_row = rows.iter().find(|r| r.contains("checkout")).unwrap();
        assert!(
            first_data_row.find("checkout").unwrap()
                > first_data_row.find("Switch branches").unwrap(),
            "description box was not drawn to the left of the suggestion box"
        );
    }

    /// `term_cols` also decides the emitted layout (box widths, wrap points),
    /// so a resize must not be swallowed by the signature check either.
    #[test]
    fn terminal_resize_forces_a_redraw() {
        let sugs = vec![mk(
            "checkout",
            "Switch branches",
            SuggestionType::Subcommand,
        )];
        let mut r = PopupRenderer::new(5, IconSet::default());
        let mut out: Vec<u8> = Vec::new();
        r.draw_full(&mut out, &sugs, 0, Direction::Below, 10, 100)
            .unwrap();
        let after_first = out.len();
        r.draw_full(&mut out, &sugs, 0, Direction::Below, 10, 120)
            .unwrap();
        assert!(
            out.len() > after_first,
            "a terminal resize was treated as an unchanged popup"
        );
    }
}
