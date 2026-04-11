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
use std::io::{self, Write};

/// Pick the top-suggestion tail for ghost rendering and popup accept.
/// Assumes the Vec is sorted (priority DESC, type precedence, name len ASC).
pub fn pick_top(suggestions: &[Suggestion], partial: &str) -> Option<String> {
    let top = suggestions.first()?;
    if top.name.len() <= partial.len() || !top.name.starts_with(partial) {
        return None;
    }
    Some(top.name[partial.len()..].to_string())
}

pub enum Renderer<W: Write> {
    Ghost(ghost::GhostRenderer<W>),
    Popup(popup::PopupRenderer<W>),
}

impl<W: Write> Renderer<W> {
    pub fn new(out: W, mode: UiMode, max_suggestions: u8) -> Self {
        match mode {
            UiMode::Ghost => Renderer::Ghost(ghost::GhostRenderer::new(out)),
            UiMode::Popup => Renderer::Popup(popup::PopupRenderer::new(out, max_suggestions)),
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
        }
    }

    pub fn clear(&mut self) -> io::Result<()> {
        match self {
            Renderer::Ghost(g) => g.clear(),
            Renderer::Popup(p) => p.clear(),
        }
    }
}
