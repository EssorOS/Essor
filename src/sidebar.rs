use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, BoxConstraints, BrushIndex, ChildrenIds, EventCtx, LayoutCtx, PaintCtx,
    PointerEvent, PropertiesMut, PropertiesRef, RegisterCtx, Widget, WidgetMut,
};
use masonry::kurbo::{Affine, Point, Rect, RoundedRect, Size, Stroke};
use masonry::parley::{
    FontContext, Layout, LayoutContext,
    style::{FontFamily, FontStack, FontWeight, GenericFamily, StyleProperty},
};
use masonry::peniko::{Brush, Color, Fill};
use masonry::vello::Scene;
use masonry::{TextAlign, TextAlignOptions};

use crate::library::DocumentMeta;

pub const SIDEBAR_WIDTH: f64 = 220.0;
const PAD: f64 = 12.0;
const HEADER_H: f64 = 32.0;
const LIST_TOP: f64 = PAD + HEADER_H + 12.0;
const ROW_H: f64 = 34.0;
const ROW_GAP: f64 = 2.0;
const RADIUS: f64 = 6.0;
const DELETE_SIZE: f64 = 22.0;
/// Horizontal room kept clear around a row's title: left inset and the space
/// reserved on the right for the delete glyph.
const TITLE_LEFT: f64 = 10.0;
const TITLE_RIGHT: f64 = 30.0;
const CONFIRM_H: f64 = 78.0;
const CONFIRM_BUTTON_H: f64 = 30.0;
const FONT_SIZE: f32 = 14.0;
const TITLE_SIZE: f32 = 13.0;

const BACKGROUND: Color = Color::from_rgb8(0xf6, 0xf6, 0xf4);
const BORDER: Color = Color::from_rgb8(0xe9, 0xe9, 0xe7);
const TEXT: Color = Color::from_rgb8(0x37, 0x35, 0x2f);
const MUTED: Color = Color::from_rgb8(0x9b, 0x9a, 0x97);
const ROW_HOVER: Color = Color::from_rgb8(0xee, 0xee, 0xec);
const ROW_ACTIVE: Color = Color::from_rgb8(0xe4, 0xe4, 0xe2);
const CONFIRM_BG: Color = Color::from_rgb8(0xff, 0xff, 0xff);
const DANGER: Color = Color::from_rgb8(0xc0, 0x39, 0x2b);
const SHADOW: Color = Color::from_rgba8(0x0f, 0x0f, 0x0f, 0x14);

/// A document row rendered in the sidebar.
#[derive(Clone, Debug)]
pub struct SidebarEntry {
    pub title: String,
}

impl From<&DocumentMeta> for SidebarEntry {
    fn from(meta: &DocumentMeta) -> Self {
        Self {
            title: meta.title.clone(),
        }
    }
}

/// Actions the sidebar asks the application to perform.
#[derive(Debug)]
pub enum SidebarAction {
    Create,
    Select(usize),
    Delete(usize),
}

/// A shaped, elided title for one row. Cached across layouts and repaints so a
/// title edit only re-shapes the row that changed.
struct SidebarRow {
    /// The entry title this row was shaped from.
    source: String,
    /// Whether it was shaped with the active row's weight.
    active: bool,
    /// The elided text actually drawn.
    display: String,
    layout: Layout<BrushIndex>,
}

/// The document list down the left edge. Owns no documents itself: it renders a
/// snapshot of the [`crate::library::Library`] and emits [`SidebarAction`]s.
pub struct Sidebar {
    entries: Vec<SidebarEntry>,
    /// Shaped rows, indexed like `entries`, reused across layouts and repaints.
    rows: Vec<SidebarRow>,
    active: usize,
    hover: Option<usize>,
    new_hover: bool,
    confirm: Option<usize>,
    height: f64,
}

impl Sidebar {
    pub fn new(entries: Vec<SidebarEntry>, active: usize) -> Self {
        let mut sidebar = Self {
            entries: Vec::new(),
            rows: Vec::new(),
            active,
            hover: None,
            new_hover: false,
            confirm: None,
            height: 0.0,
        };
        sidebar.apply_entries(entries, active);
        sidebar
    }

    /// Replace the rendered list and active index, dropping any transient
    /// interaction state tied to the previous list.
    fn apply_entries(&mut self, entries: Vec<SidebarEntry>, active: usize) {
        self.entries = entries;
        self.active = active;
        self.rows.clear();
        self.hover = None;
        self.new_hover = false;
        self.confirm = None;
    }

    /// Replace the rendered list and active index.
    pub fn set_entries(this: &mut WidgetMut<'_, Self>, entries: Vec<SidebarEntry>, active: usize) {
        this.widget.apply_entries(entries, active);
        this.ctx.request_layout();
        this.ctx.request_render();
    }

    /// Update one row's title in place, re-shaping only that row on the next
    /// layout pass. Invalidation is implicit: the changed `entry.title` no longer
    /// matches the cached `row.source`, so `layout` rebuilds just this row.
    pub fn set_title(this: &mut WidgetMut<'_, Self>, index: usize, title: String) {
        let Some(entry) = this.widget.entries.get_mut(index) else {
            return;
        };
        if entry.title == title {
            return;
        }
        entry.title = title;
        this.ctx.request_layout();
        this.ctx.request_render();
    }

    /// Width available to a row's title, in pixels.
    fn title_width() -> f64 {
        (SIDEBAR_WIDTH - PAD * 2.0) - TITLE_LEFT - TITLE_RIGHT
    }

    fn new_rect(&self) -> Rect {
        Rect::from_origin_size((PAD, PAD), (SIDEBAR_WIDTH - PAD * 2.0, HEADER_H))
    }

    fn row_rect(&self, index: usize) -> Rect {
        Rect::from_origin_size(
            (PAD, LIST_TOP + index as f64 * (ROW_H + ROW_GAP)),
            (SIDEBAR_WIDTH - PAD * 2.0, ROW_H),
        )
    }

    fn delete_rect(&self, index: usize) -> Rect {
        let row = self.row_rect(index);
        Rect::from_center_size(
            (row.x1 - DELETE_SIZE * 0.5 - 4.0, row.center().y),
            (DELETE_SIZE, DELETE_SIZE),
        )
    }

    fn confirm_rect(&self, index: usize) -> Rect {
        let row = self.row_rect(index);
        let max_y = (self.height - CONFIRM_H - PAD).max(PAD);
        let y = (row.y1 + 4.0).min(max_y);
        Rect::from_origin_size((PAD, y), (SIDEBAR_WIDTH - PAD * 2.0, CONFIRM_H))
    }

    fn confirm_buttons(rect: Rect) -> (Rect, Rect) {
        let gap = 10.0;
        let width = (rect.width() - gap * 3.0) * 0.5;
        let y = rect.y1 - gap - CONFIRM_BUTTON_H;
        let cancel = Rect::from_origin_size((rect.x0 + gap, y), (width, CONFIRM_BUTTON_H));
        let delete =
            Rect::from_origin_size((rect.x0 + gap * 2.0 + width, y), (width, CONFIRM_BUTTON_H));
        (cancel, delete)
    }

    fn row_at(&self, point: Point) -> Option<usize> {
        (0..self.entries.len()).find(|&index| self.row_rect(index).contains(point))
    }

    fn handle_down(&mut self, ctx: &mut EventCtx<'_>, point: Point) {
        if let Some(index) = self.confirm {
            let rect = self.confirm_rect(index);
            let (cancel, delete) = Self::confirm_buttons(rect);
            if delete.contains(point) {
                self.confirm = None;
                ctx.submit_action::<SidebarAction>(SidebarAction::Delete(index));
                ctx.set_handled();
                ctx.request_render();
            } else if cancel.contains(point) || !rect.contains(point) {
                self.confirm = None;
                ctx.request_render();
            }
            return;
        }

        if self.new_rect().contains(point) {
            ctx.submit_action::<SidebarAction>(SidebarAction::Create);
            ctx.set_handled();
            ctx.request_render();
            return;
        }

        if let Some(index) = self.row_at(point) {
            if self.delete_rect(index).contains(point) {
                self.confirm = Some(index);
                ctx.request_render();
                return;
            }
            ctx.submit_action::<SidebarAction>(SidebarAction::Select(index));
            ctx.set_handled();
            ctx.request_render();
        }
    }
}

fn text_layout(
    fcx: &mut FontContext,
    lcx: &mut LayoutContext<BrushIndex>,
    text: &str,
    size: f32,
    weight: FontWeight,
) -> Layout<BrushIndex> {
    let mut builder = lcx.ranged_builder(fcx, text, 1.0, true);
    builder.push_default(StyleProperty::FontStack(FontStack::Single(
        FontFamily::Generic(GenericFamily::SansSerif),
    )));
    builder.push_default(StyleProperty::FontSize(size));
    builder.push_default(StyleProperty::FontWeight(weight));
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    layout.align(None, TextAlign::Start, TextAlignOptions::default());
    layout
}

/// Elide `text` with a trailing ellipsis so it fits within `max_width` pixels,
/// returning the elided string and its shaped layout.
fn fit_title(
    fcx: &mut FontContext,
    lcx: &mut LayoutContext<BrushIndex>,
    text: &str,
    size: f32,
    weight: FontWeight,
    max_width: f64,
) -> (String, Layout<BrushIndex>) {
    let exact = text_layout(fcx, lcx, text, size, weight);
    if text.is_empty() || exact.width() as f64 <= max_width {
        return (text.to_string(), exact);
    }

    let chars: Vec<char> = text.chars().collect();
    let mut fits = 0usize;
    let mut too_long = chars.len();
    while fits < too_long {
        let mid = (fits + too_long).div_ceil(2);
        let mut candidate: String = chars[..mid].iter().collect();
        candidate.push('…');
        if text_layout(fcx, lcx, &candidate, size, weight).width() as f64 <= max_width {
            fits = mid;
        } else {
            too_long = mid - 1;
        }
    }

    let mut elided: String = chars[..fits].iter().collect();
    elided.push('…');
    let layout = text_layout(fcx, lcx, &elided, size, weight);
    (elided, layout)
}

impl Widget for Sidebar {
    type Action = SidebarAction;

    fn on_pointer_event(
        &mut self,
        ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        event: &PointerEvent,
    ) {
        match event {
            PointerEvent::Down(button) => {
                let point = ctx.local_position(button.state.position);
                self.handle_down(ctx, point);
            }
            PointerEvent::Move(update) => {
                let point = ctx.local_position(update.current.position);
                let hover = self.row_at(point);
                let new_hover = self.new_rect().contains(point);
                if hover != self.hover || new_hover != self.new_hover {
                    self.hover = hover;
                    self.new_hover = new_hover;
                    ctx.request_render();
                }
            }
            PointerEvent::Leave(_) if self.hover.is_some() || self.new_hover => {
                self.hover = None;
                self.new_hover = false;
                ctx.request_render();
            }
            _ => {}
        }
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        bc: &BoxConstraints,
    ) -> Size {
        let width = SIDEBAR_WIDTH.min(bc.max().width);
        let natural = LIST_TOP + self.entries.len() as f64 * (ROW_H + ROW_GAP) + PAD;
        let height = if bc.is_height_bounded() {
            bc.max().height
        } else {
            natural
        };
        self.height = height;

        let fonts_changed = ctx.fonts_changed();
        let (fcx, lcx) = ctx.text_contexts();
        let max_width = Self::title_width();
        if fonts_changed {
            self.rows.clear();
        }
        self.rows.truncate(self.entries.len());
        for (index, entry) in self.entries.iter().enumerate() {
            let active = index == self.active;
            if let Some(row) = self.rows.get(index)
                && row.source == entry.title
                && row.active == active
            {
                continue;
            }
            let weight = if active {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            };
            let (display, layout) =
                fit_title(fcx, lcx, &entry.title, TITLE_SIZE, weight, max_width);
            let row = SidebarRow {
                source: entry.title.clone(),
                active,
                display,
                layout,
            };
            if index < self.rows.len() {
                self.rows[index] = row;
            } else {
                self.rows.push(row);
            }
        }

        Size::new(width, height)
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, scene: &mut Scene) {
        let size = ctx.size();
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            BACKGROUND,
            None,
            &size.to_rect(),
        );
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            BORDER,
            None,
            &Rect::from_origin_size((size.width - 1.0, 0.0), (1.0, size.height)),
        );

        let (fcx, lcx) = ctx.text_contexts();

        // "New page" header button.
        let new_rect = self.new_rect();
        let new_bg = if self.new_hover {
            ROW_ACTIVE
        } else {
            ROW_HOVER
        };
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            new_bg,
            None,
            &RoundedRect::from_rect(new_rect, RADIUS),
        );
        let plus = text_layout(fcx, lcx, "+", FONT_SIZE, FontWeight::BOLD);
        let plus_y = new_rect.y0 + (new_rect.height() - plus.height() as f64) * 0.5;
        masonry::core::render_text(
            scene,
            Affine::translate((new_rect.x0 + 10.0, plus_y)),
            &plus,
            &[Brush::Solid(TEXT)],
            true,
        );
        let label = text_layout(fcx, lcx, "New page", FONT_SIZE, FontWeight::NORMAL);
        let label_y = new_rect.y0 + (new_rect.height() - label.height() as f64) * 0.5;
        masonry::core::render_text(
            scene,
            Affine::translate((new_rect.x0 + 28.0, label_y)),
            &label,
            &[Brush::Solid(TEXT)],
            true,
        );

        // Document rows.
        for index in 0..self.entries.len() {
            let row = self.row_rect(index);
            let highlight = if index == self.active {
                Some(ROW_ACTIVE)
            } else if self.hover == Some(index) {
                Some(ROW_HOVER)
            } else {
                None
            };
            if let Some(color) = highlight {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    color,
                    None,
                    &RoundedRect::from_rect(row, RADIUS),
                );
            }

            if let Some(shaped) = self.rows.get(index) {
                let color = if index == self.active { TEXT } else { MUTED };
                let text_y = row.y0 + (row.height() - shaped.layout.height() as f64) * 0.5;
                masonry::core::render_text(
                    scene,
                    Affine::translate((row.x0 + TITLE_LEFT, text_y)),
                    &shaped.layout,
                    &[Brush::Solid(color)],
                    true,
                );
            }

            if self.hover == Some(index) && self.confirm != Some(index) {
                let cross = text_layout(fcx, lcx, "×", FONT_SIZE, FontWeight::NORMAL);
                let rect = self.delete_rect(index);
                let cross_y = rect.y0 + (rect.height() - cross.height() as f64) * 0.5;
                masonry::core::render_text(
                    scene,
                    Affine::translate((rect.x0 + 5.0, cross_y)),
                    &cross,
                    &[Brush::Solid(MUTED)],
                    true,
                );
            }
        }

        // Delete confirmation popover.
        if let Some(index) = self.confirm {
            let rect = self.confirm_rect(index);
            let panel = RoundedRect::from_rect(rect, RADIUS);
            let shadow = RoundedRect::from_rect(
                Rect::from_origin_size((rect.x0, rect.y0 + 2.0), rect.size()).inflate(1.0, 1.0),
                RADIUS,
            );
            scene.fill(Fill::NonZero, Affine::IDENTITY, SHADOW, None, &shadow);
            scene.fill(Fill::NonZero, Affine::IDENTITY, CONFIRM_BG, None, &panel);
            scene.stroke(&Stroke::new(1.0), Affine::IDENTITY, BORDER, None, &panel);

            let prompt = text_layout(
                fcx,
                lcx,
                "Delete this page?",
                TITLE_SIZE,
                FontWeight::NORMAL,
            );
            masonry::core::render_text(
                scene,
                Affine::translate((rect.x0 + 10.0, rect.y0 + 10.0)),
                &prompt,
                &[Brush::Solid(TEXT)],
                true,
            );

            let (cancel, delete) = Sidebar::confirm_buttons(rect);
            for (button, text, color) in [(cancel, "Cancel", TEXT), (delete, "Delete", DANGER)] {
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    ROW_HOVER,
                    None,
                    &RoundedRect::from_rect(button, 4.0),
                );
                let layout = text_layout(fcx, lcx, text, TITLE_SIZE, FontWeight::NORMAL);
                let y = button.y0 + (button.height() - layout.height() as f64) * 0.5;
                masonry::core::render_text(
                    scene,
                    Affine::translate((button.x0 + 12.0, y)),
                    &layout,
                    &[Brush::Solid(color)],
                    true,
                );
            }
        }
    }

    fn accessibility_role(&self) -> Role {
        Role::List
    }

    fn accessibility(
        &mut self,
        _ctx: &mut AccessCtx<'_>,
        _props: &PropertiesRef<'_>,
        node: &mut Node,
    ) {
        let active = self
            .rows
            .get(self.active)
            .map(|row| row.display.as_str())
            .unwrap_or("Untitled");
        node.set_label(format!("Documents, current: {active}"));
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use masonry::core::{NewWidget, WidgetTag};
    use masonry::theme::default_property_set;
    use masonry::ui_events::pointer::PointerButton;
    use masonry_testing::TestHarness;

    const SIDEBAR_TAG: WidgetTag<Sidebar> = WidgetTag::new("sidebar");

    fn harness() -> TestHarness<Sidebar> {
        let entries = vec![
            SidebarEntry {
                title: "First".into(),
            },
            SidebarEntry {
                title: "Second".into(),
            },
        ];
        TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new_with_tag(Sidebar::new(entries, 0), SIDEBAR_TAG),
            Size::new(SIDEBAR_WIDTH, 480.0),
        )
    }

    #[test]
    fn long_titles_are_elided_to_fit() {
        let entries = vec![SidebarEntry {
            title: "Longest document title ".repeat(20),
        }];
        let harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new_with_tag(Sidebar::new(entries, 0), SIDEBAR_TAG),
            Size::new(SIDEBAR_WIDTH, 480.0),
        );

        let display = &harness.get_widget(SIDEBAR_TAG).rows[0].display;
        assert!(display.ends_with('…'), "not elided: {display}");
        assert!(display.chars().count() < 40);
    }

    #[test]
    fn clicking_new_emits_create() {
        let mut harness = harness();
        harness.mouse_move((40.0, PAD + HEADER_H * 0.5));
        harness.mouse_button_press(PointerButton::Primary);
        harness.mouse_button_release(PointerButton::Primary);
        let action = harness.pop_action::<SidebarAction>();
        assert!(matches!(action, Some((SidebarAction::Create, _))));
    }

    #[test]
    fn clicking_row_emits_select() {
        let mut harness = harness();
        let row = Rect::from_origin_size((PAD, LIST_TOP), (SIDEBAR_WIDTH - PAD * 2.0, ROW_H));
        harness.mouse_move((row.x0 + 20.0, row.center().y));
        harness.mouse_button_press(PointerButton::Primary);
        harness.mouse_button_release(PointerButton::Primary);
        let action = harness.pop_action::<SidebarAction>();
        assert!(matches!(action, Some((SidebarAction::Select(0), _))));
    }

    #[test]
    fn delete_requires_confirmation() {
        let mut harness = harness();
        let row = Rect::from_origin_size((PAD, LIST_TOP), (SIDEBAR_WIDTH - PAD * 2.0, ROW_H));
        let delete = Rect::from_center_size(
            (row.x1 - DELETE_SIZE * 0.5 - 4.0, row.center().y),
            (DELETE_SIZE, DELETE_SIZE),
        );

        // Clicking the × opens the confirmation instead of deleting.
        harness.mouse_move(delete.center());
        harness.mouse_button_press(PointerButton::Primary);
        harness.mouse_button_release(PointerButton::Primary);
        assert!(harness.pop_action::<SidebarAction>().is_none());

        // Confirming emits the delete action.
        let confirm = harness.get_widget(SIDEBAR_TAG).confirm_rect(0);
        let (_, confirm_delete) = Sidebar::confirm_buttons(confirm);
        harness.mouse_move(confirm_delete.center());
        harness.mouse_button_press(PointerButton::Primary);
        harness.mouse_button_release(PointerButton::Primary);
        let action = harness.pop_action::<SidebarAction>();
        assert!(matches!(action, Some((SidebarAction::Delete(0), _))));
    }
}
