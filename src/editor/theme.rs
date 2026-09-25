use masonry::peniko::Color;

use crate::doc::BlockKind;

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

// Notion-ish light theme.
pub(super) const BACKGROUND: Color = Color::from_rgb8(0xff, 0xff, 0xff);
pub(super) const TEXT: Color = Color::from_rgb8(0x37, 0x35, 0x2f);
pub(super) const PLACEHOLDER: Color = Color::from_rgb8(0xb8, 0xb7, 0xb4);
pub(super) const BULLET: Color = Color::from_rgb8(0x37, 0x35, 0x2f);
pub(super) const GUTTER_ICON_COLOR: Color = Color::from_rgb8(0xb8, 0xb7, 0xb4);
pub(super) const CARET: Color = Color::from_rgb8(0x37, 0x35, 0x2f);
pub(super) const SELECTION: Color = Color::from_rgba8(0x23, 0x83, 0xe2, 0x40);
pub(super) const MENU_BG: Color = Color::from_rgb8(0xff, 0xff, 0xff);
pub(super) const MENU_BORDER: Color = Color::from_rgb8(0xe9, 0xe9, 0xe7);
pub(super) const MENU_SELECTED: Color = Color::from_rgb8(0xf1, 0xf1, 0xef);
pub(super) const MENU_SHADOW: Color = Color::from_rgba8(0x0f, 0x0f, 0x0f, 0x12);

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
