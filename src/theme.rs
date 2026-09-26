//! The shared UI palette.
//!
//! Colors live here, not in the widgets that use them: the editor and sidebar
//! paint related surfaces (ink, hairlines, shadows) and previously declared
//! their own copies, which risked drifting apart.

use masonry::peniko::Color;

// Ink and text.
pub(crate) const INK: Color = Color::from_rgb8(0x37, 0x35, 0x2f);
pub(crate) const MUTED: Color = Color::from_rgb8(0x9b, 0x9a, 0x97);
pub(crate) const PLACEHOLDER: Color = Color::from_rgb8(0xb8, 0xb7, 0xb4);

// Surfaces.
pub(crate) const SURFACE: Color = Color::from_rgb8(0xff, 0xff, 0xff);
pub(crate) const SIDEBAR_BG: Color = Color::from_rgb8(0xf6, 0xf6, 0xf4);
pub(crate) const SIDEBAR_HOVER: Color = Color::from_rgb8(0xee, 0xee, 0xec);
pub(crate) const SIDEBAR_ACTIVE: Color = Color::from_rgb8(0xe4, 0xe4, 0xe2);
pub(crate) const MENU_SELECTED: Color = Color::from_rgb8(0xf1, 0xf1, 0xef);

// Lines and shadows.
pub(crate) const HAIRLINE: Color = Color::from_rgb8(0xe9, 0xe9, 0xe7);
pub(crate) const SHADOW: Color = Color::from_rgba8(0x0f, 0x0f, 0x0f, 0x14);

// Accents.
pub(crate) const DANGER: Color = Color::from_rgb8(0xc0, 0x39, 0x2b);
pub(crate) const SELECTION: Color = Color::from_rgba8(0x23, 0x83, 0xe2, 0x40);
