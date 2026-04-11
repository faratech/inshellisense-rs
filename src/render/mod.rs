//! Suggestion renderers.
//!
//! Two modes:
//! - `ghost` (default, PSReadLine-style): grey inline text after the
//!   cursor showing the top suggestion's tail. Accepted via right-arrow.
//! - `popup` (upstream inshellisense-style): a small box below the prompt
//!   showing up to `max_suggestions` ranked candidates with their
//!   descriptions. Active candidate highlighted; accept via right-arrow
//!   (same binding as ghost mode for now — full up/down navigation is
//!   deferred to a follow-up since it requires deeper PTY key
//!   interception).
//!
//! The `Renderer` struct below is an enum-dispatch wrapper that picks
//! one of the two backends at construction time based on config. This
//! keeps `src/pty.rs` agnostic of which UI is active.

pub mod ghost;
pub mod popup;

use crate::config::UiMode;
use crate::spec::model::Suggestion;
use std::io;

pub use popup::Direction;

/// Pick the top-suggestion tail for ghost rendering and popup accept.
/// Assumes the Vec is sorted (priority DESC, type precedence, name len ASC).
pub fn pick_top(suggestions: &[Suggestion], partial: &str) -> Option<String> {
    let top = suggestions.first()?;
    if top.name.len() <= partial.len() || !top.name.starts_with(partial) {
        return None;
    }
    Some(top.name[partial.len()..].to_string())
}

pub enum Renderer {
    Ghost(ghost::GhostRenderer<std::io::Stdout>),
    Popup(popup::PopupRenderer<std::io::Stdout>),
    Hybrid {
        ghost: ghost::GhostRenderer<std::io::Stdout>,
        popup: popup::PopupRenderer<std::io::Stdout>,
    },
}

impl Renderer {
    pub fn new(_out: std::io::Stdout, mode: UiMode, max_suggestions: u8) -> Self {
        match mode {
            UiMode::Ghost => Renderer::Ghost(ghost::GhostRenderer::new(std::io::stdout())),
            UiMode::Popup => Renderer::Popup(popup::PopupRenderer::new(
                std::io::stdout(),
                max_suggestions,
            )),
            UiMode::Hybrid => Renderer::Hybrid {
                ghost: ghost::GhostRenderer::new(std::io::stdout()),
                popup: popup::PopupRenderer::new(std::io::stdout(), max_suggestions),
            },
        }
    }

    /// Redraw from the current suggestion state. `tail` is the accept
    /// target for both modes; `all` is the ranked list for popup mode.
    pub fn draw(
        &mut self,
        tail: Option<&str>,
        all: &[Suggestion],
    ) -> io::Result<()> {
        match self {
            Renderer::Ghost(g) => g.draw(tail),
            Renderer::Popup(p) => p.draw(tail, all),
            Renderer::Hybrid { ghost, popup } => {
                ghost.draw(tail)?;
                popup.draw(tail, all)
            }
        }
    }

    /// Popup-interactive draw with cursor + direction. In Ghost mode,
    /// falls back to a regular ghost draw (popup state is irrelevant).
    /// In Hybrid mode, draws the ghost tail *and* the popup so both are
    /// visible at once.
    ///
    /// `cursor_col` and `term_cols` drive upstream-style cursor-aware
    /// padding so the popup's left edge aligns under the partial token
    /// the user is typing.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_popup_interactive(
        &mut self,
        tail: Option<&str>,
        all: &[Suggestion],
        cursor: usize,
        direction: Direction,
        cursor_col: u16,
        term_cols: u16,
    ) -> io::Result<()> {
        match self {
            Renderer::Ghost(g) => g.draw(tail),
            Renderer::Popup(p) => p.draw_full(all, cursor, direction, cursor_col, term_cols),
            Renderer::Hybrid { ghost, popup } => {
                ghost.draw(tail)?;
                popup.draw_full(all, cursor, direction, cursor_col, term_cols)
            }
        }
    }

    pub fn clear(&mut self) -> io::Result<()> {
        match self {
            Renderer::Ghost(g) => g.clear(),
            Renderer::Popup(p) => p.clear(),
            Renderer::Hybrid { ghost, popup } => {
                ghost.clear()?;
                popup.clear()
            }
        }
    }
}
