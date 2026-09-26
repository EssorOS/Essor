use crate::doc::BlockKind;

/// The shared palette, re-exported under the names this widget has always used
/// so the editor submodules reach it through `use super::*`.
pub(super) use crate::theme::{
    HAIRLINE as MENU_BORDER, INK as BULLET, INK as CARET, INK as TEXT, MENU_SELECTED, PLACEHOLDER,
    PLACEHOLDER as GUTTER_ICON_COLOR, SELECTION, SHADOW as MENU_SHADOW, SURFACE as BACKGROUND,
    SURFACE as MENU_BG,
};

pub(super) const PAGE_MAX_WIDTH: f64 = 900.0;
pub(super) const PAGE_MARGIN: f64 = 48.0;
pub(super) const MIN_CONTENT_LEFT: f64 = 56.0;
pub(super) const PAGE_TOP: f64 = 72.0;
pub(super) const PAGE_BOTTOM: f64 = 96.0;
pub(super) const GUTTER_ICON: f64 = 14.0;
pub(super) const BULLET_INDENT: f64 = 22.0;
pub(super) const BULLET_RADIUS: f64 = 2.6;
pub(super) const CARET_WIDTH: f64 = 1.5;
/// Squared pointer travel (px²) before a press turns into a drag.
pub(super) const DRAG_START_DIST_SQ: f64 = 9.0;
pub(super) const MENU_WIDTH: f64 = 190.0;
pub(super) const MENU_ROW: f64 = 30.0;
pub(super) const MENU_PAD: f64 = 6.0;
pub(super) const MENU_RADIUS: f64 = 6.0;
pub(super) const MENU_FONT_SIZE: f32 = 14.0;
pub(super) const DEFAULT_FONT_SIZE: f32 = 16.0;

/// Horizontal offsets of the gutter icons from the page's left edge, and the
/// minimum x they may sit at.
pub(super) const GUTTER_PLUS_OFFSET: f64 = 30.0;
pub(super) const GUTTER_OPTIONS_OFFSET: f64 = 13.0;
pub(super) const GUTTER_MIN_CENTER: f64 = 9.0;

/// Layout of the six-dot options glyph.
pub(super) const DOT_RADIUS: f64 = 1.15;
pub(super) const DOT_COLUMNS: [f64; 2] = [-2.4, 2.4];
pub(super) const DOT_ROWS: [f64; 3] = [-4.0, 0.0, 4.0];

/// Per-kind font size, in logical pixels.
pub(super) fn font_size(kind: BlockKind) -> f32 {
    match kind {
        BlockKind::Paragraph | BlockKind::Bullet => 16.0,
        BlockKind::Heading1 => 28.0,
        BlockKind::Heading2 => 22.0,
    }
}

/// Per-kind line height (multiple of font size) and space above the block.
pub(super) fn block_metrics(kind: BlockKind) -> (f32, f64) {
    match kind {
        BlockKind::Heading1 => (1.2, 24.0),
        BlockKind::Heading2 => (1.3, 16.0),
        BlockKind::Paragraph | BlockKind::Bullet => (1.5, 2.0),
    }
}

/// Page column metrics for a widget of the given width: left inset and content
/// width. Used by both layout and paint so they always agree on the wrap width.
pub(super) fn page_metrics(width: f64) -> (f64, f32) {
    let margin = ((width - PAGE_MAX_WIDTH) * 0.5).max(PAGE_MARGIN);
    let left = margin.max(MIN_CONTENT_LEFT);
    let content = (width - left - margin).max(1.0);
    (left, content as f32)
}

pub(super) fn indent(kind: BlockKind) -> f64 {
    match kind {
        BlockKind::Bullet => BULLET_INDENT,
        _ => 0.0,
    }
}
