//! Waku-derived Markdown parsing and rendering.
//!
//! Vendored and adapted from Waku's `src/md` module. Both ShellRS and Waku are
//! licensed GPL-3.0-only. ShellRS adaptations cover GPUI Kit imports, semantic
//! theme tokens, dynamic font families, and image resolution against the AI
//! panel's working directory.

pub mod highlight;
pub mod mend;
pub mod parser;
pub mod render;
pub mod selection;
pub mod theme;
pub mod veil;
