//! Suggestion renderers.
//!
//! Three modes:
//! - `ghost` (PSReadLine-style): grey inline text after the cursor.
//! - `popup` (upstream inshellisense-style): boxed popup below/above.
//! - `hybrid` (default): both at once — ghost for the top suggestion,
//!   popup for the alternatives.
//!
//! All renderer state is owned by `Renderer`; actual I/O is done via
//! a `&mut impl Write` parameter passed into each draw/clear call.
//! This means the caller (pty.rs) controls stdout locking and there's
//! no risk of interleaved ANSI sequences from multiple Stdout handles.

pub mod ghost;
pub mod popup;

use crate::config::UiMode;
use crate::spec::model::Suggestion;
use std::io;

pub use popup::Direction;

pub fn pick_top(suggestions: &[Suggestion], partial: &str) -> Option<String> {
    let top = suggestions.first()?;
    if top.name.len() <= partial.len() || !top.name.starts_with(partial) {
        return None;
    }
    Some(top.name[partial.len()..].to_string())
}

pub enum Renderer {
    Ghost(ghost::GhostRenderer),
    Popup(popup::PopupRenderer),
    Hybrid {
        ghost: ghost::GhostRenderer,
        popup: popup::PopupRenderer,
    },
}

impl Renderer {
    pub fn new(mode: UiMode, max_suggestions: u8, icons: popup::IconSet) -> Self {
        match mode {
            UiMode::Ghost => Renderer::Ghost(ghost::GhostRenderer::new()),
            UiMode::Popup => Renderer::Popup(popup::PopupRenderer::new(max_suggestions, icons)),
            UiMode::Hybrid => Renderer::Hybrid {
                ghost: ghost::GhostRenderer::new(),
                popup: popup::PopupRenderer::new(max_suggestions, icons),
            },
        }
    }

    pub fn with_config(cfg: &crate::config::Config, icons: popup::IconSet) -> Self {
        match cfg.ui {
            UiMode::Ghost => Renderer::Ghost(ghost::GhostRenderer::new()),
            UiMode::Popup => Renderer::Popup(popup::PopupRenderer::with_options(
                cfg.max_suggestions,
                icons,
                cfg.box_border_style,
                Some(&cfg.active_suggestion_background_color),
            )),
            UiMode::Hybrid => Renderer::Hybrid {
                ghost: ghost::GhostRenderer::new(),
                popup: popup::PopupRenderer::with_options(
                    cfg.max_suggestions,
                    icons,
                    cfg.box_border_style,
                    Some(&cfg.active_suggestion_background_color),
                ),
            },
        }
    }

    /// `ghost_cells` is the number of columns left on the cursor's row; the
    /// ghost tail is truncated to fit so it never wraps.
    pub fn draw(
        &mut self,
        out: &mut impl io::Write,
        tail: Option<&str>,
        all: &[Suggestion],
        ghost_cells: usize,
    ) -> io::Result<()> {
        match self {
            Renderer::Ghost(g) => g.draw(out, tail, ghost_cells),
            Renderer::Popup(p) => p.draw(out, tail, all),
            Renderer::Hybrid { ghost, popup } => {
                ghost.draw(out, tail, ghost_cells)?;
                popup.draw(out, tail, all)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_popup_interactive(
        &mut self,
        out: &mut impl io::Write,
        tail: Option<&str>,
        all: &[Suggestion],
        cursor: usize,
        direction: Direction,
        cursor_col: u16,
        term_cols: u16,
    ) -> io::Result<()> {
        let ghost_cells = term_cols.saturating_sub(cursor_col) as usize;
        match self {
            Renderer::Ghost(g) => g.draw(out, tail, ghost_cells),
            Renderer::Popup(p) => p.draw_full(out, all, cursor, direction, cursor_col, term_cols),
            Renderer::Hybrid { ghost, popup } => {
                ghost.draw(out, tail, ghost_cells)?;
                popup.draw_full(out, all, cursor, direction, cursor_col, term_cols)
            }
        }
    }

    pub fn clear(&mut self, out: &mut impl io::Write) -> io::Result<()> {
        match self {
            Renderer::Ghost(g) => g.clear(out),
            Renderer::Popup(p) => p.clear(out),
            Renderer::Hybrid { ghost, popup } => {
                ghost.clear(out)?;
                popup.clear(out)
            }
        }
    }

    pub fn ghost_visible(&self) -> bool {
        match self {
            Renderer::Ghost(g) => g.is_visible(),
            Renderer::Popup(_) => false,
            Renderer::Hybrid { ghost, .. } => ghost.is_visible(),
        }
    }

    pub fn clear_ghost(&mut self, out: &mut impl io::Write) -> io::Result<()> {
        match self {
            Renderer::Ghost(g) => g.clear(out),
            Renderer::Popup(_) => Ok(()),
            Renderer::Hybrid { ghost, .. } => ghost.clear(out),
        }
    }
}
