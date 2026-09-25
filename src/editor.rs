use std::time::Instant;

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AccessEvent, BoxConstraints, BrushIndex, ChildrenIds, EventCtx, Ime, LayoutCtx,
    NoAction, PaintCtx, PointerEvent, PropertiesMut, PropertiesRef, RegisterCtx, TextEvent, Update,
    UpdateCtx, Widget, WidgetMut,
};
use masonry::kurbo::{Affine, Circle, Point, Rect, RoundedRect, Size, Stroke};
use masonry::parley::{
    Affinity, Cursor, FontContext, Layout, LayoutContext, RangedBuilder, Selection,
    style::{
        FontFamily, FontStack, FontStyle, FontWeight, GenericFamily, LineHeight, StyleProperty,
    },
};
use masonry::peniko::{Brush, Color, Fill};
use masonry::ui_events::keyboard::{Key, KeyState, KeyboardEvent, NamedKey};
use masonry::ui_events::pointer::PointerButton;
use masonry::vello::Scene;
use masonry::{TextAlign, TextAlignOptions};

use crate::blink::{BLINK_INTERVAL, BlinkTimer};
use crate::doc::{Block, BlockKind, Doc, Mark, TextRun};

mod cursor;
mod edit;
mod layout;
mod menu;
mod paint;
mod selection;
mod text;
mod theme;
use self::edit::*;
use self::layout::*;
use self::menu::*;
use self::selection::*;
use self::text::*;
use self::theme::*;

/// A custom, GPU-rendered block editor.
///
/// The widget owns the [`Doc`] (a `yrs`-backed CRDT behind the trait boundary),
/// caches a Parley [`Layout`] per block, and paints through Vello. Shaping,
/// font fallback and glyph rasterization are delegated to Parley/Vello; the
/// document model, layout cache, cursor/selection, and editing semantics are ours.
pub struct Editor {
    doc: Box<dyn Doc>,
    blocks: Vec<Block>,
    layouts: Vec<BlockLayout>,
    layout_width: f32,
    page_left: f64,
    content_height: f64,
    layouts_dirty: bool,
    selection: SelectionState,
    preedit: String,
    hover: Option<usize>,
    menu: Option<Menu>,
    caret_visible: bool,
    focused: bool,
    last_activity: Instant,
    blink: BlinkTimer,
}

impl Editor {
    pub fn new(doc: Box<dyn Doc>, blink: BlinkTimer) -> Self {
        Self {
            doc,
            blocks: Vec::new(),
            layouts: Vec::new(),
            layout_width: 0.0,
            page_left: MIN_CONTENT_LEFT,
            content_height: 0.0,
            layouts_dirty: true,
            selection: SelectionState {
                anchor: Position::new(0, 0),
                focus: Position::new(0, 0),
                preferred_x: None,
                dragging: false,
                drag_started: false,
                down_point: None,
                drag_span: None,
            },
            preedit: String::new(),
            hover: None,
            menu: None,
            caret_visible: true,
            focused: false,
            last_activity: Instant::now(),
            blink,
        }
    }

    /// Persist to disk; a no-op when there are no unsaved changes.
    fn autosave(&self) {
        if let Err(error) = self.doc.persist() {
            tracing::warn!(?error, "failed to persist document");
        }
    }

    /// The caret moved (or the user interacted): show it immediately and restart
    /// the blink timer so it stays solid for a full interval.
    fn activity(&mut self) {
        self.caret_visible = true;
        self.last_activity = Instant::now();
        self.blink.reset();
    }

    /// Blink the caret. Driven by a timer thread (see `main.rs`) that wakes the app
    /// every half second; we toggle and request a single repaint. This avoids a
    /// continuous animation-frame loop, which starved event-driven repaints.
    pub fn blink_tick(this: &mut WidgetMut<'_, Self>) {
        if !this.widget.focused {
            return;
        }
        // Ignore a tick that races with fresh activity.
        if this.widget.last_activity.elapsed() < BLINK_INTERVAL {
            if !this.widget.caret_visible {
                this.widget.caret_visible = true;
                this.ctx.request_paint_only();
            }
            return;
        }
        this.widget.caret_visible = !this.widget.caret_visible;
        this.ctx.request_paint_only();
    }

    #[cfg(test)]
    fn test_focus(&self) -> (usize, usize) {
        (self.selection.focus.block, self.selection.focus.offset)
    }

    fn block(&self, index: usize) -> Option<Block> {
        self.blocks.get(index).cloned()
    }

    /// Flush pending content changes: rebuild layouts, request a relayout, and
    /// autosave. Safe to call when nothing is dirty.
    fn flush_edits(&mut self, ctx: &mut EventCtx<'_>) {
        if self.layouts_dirty {
            self.refresh_layouts(ctx);
            ctx.request_layout();
            self.autosave();
        }
    }

    /// Shared post-event handling: flush edits and keep the caret visible.
    fn after_edit(&mut self, ctx: &mut EventCtx<'_>) {
        self.flush_edits(ctx);
        self.activity();
        self.scroll_caret_into_view(ctx);
    }
}

impl Widget for Editor {
    type Action = NoAction;

    fn accepts_focus(&self) -> bool {
        true
    }

    fn accepts_text_input(&self) -> bool {
        true
    }

    fn on_pointer_event(
        &mut self,
        ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        event: &PointerEvent,
    ) {
        match event {
            PointerEvent::Down(button) => {
                let point = ctx.local_position(button.state.position);
                self.pointer_down(
                    ctx,
                    point,
                    button.state.count,
                    button.state.modifiers.shift(),
                );
            }
            PointerEvent::Move(update) => {
                let point = ctx.local_position(update.current.position);
                let primary = update.current.buttons.contains(PointerButton::Primary);
                self.pointer_move(ctx, point, primary);
            }
            PointerEvent::Leave(_) => self.pointer_leave(ctx),
            PointerEvent::Up(_) if self.selection.dragging => self.pointer_up(ctx),
            _ => {}
        }
    }

    fn on_text_event(
        &mut self,
        ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        event: &TextEvent,
    ) {
        let TextEvent::Keyboard(key) = event else {
            if self.handle_ime_or_clipboard(event) {
                self.after_edit(ctx);
                self.update_slash();
                ctx.set_handled();
                ctx.request_render();
            }
            return;
        };

        if key.state != KeyState::Down || key.is_composing {
            return;
        }

        if self.handle_menu_key(ctx, key) {
            return;
        }

        let handled = self.handle_key(ctx, key);
        if handled == KeyHandled::No {
            return;
        }
        self.after_edit(ctx);
        if handled == KeyHandled::Edited {
            self.update_slash();
        }
        ctx.set_handled();
        ctx.request_render();
    }

    fn on_access_event(
        &mut self,
        _ctx: &mut EventCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        _event: &AccessEvent,
    ) {
    }

    fn update(&mut self, ctx: &mut UpdateCtx<'_>, _props: &mut PropertiesMut<'_>, event: &Update) {
        if let Update::FocusChanged(focused) = event {
            self.focused = *focused;
            self.activity();
            ctx.request_render();
        }
    }

    fn register_children(&mut self, _ctx: &mut RegisterCtx<'_>) {}

    fn layout(
        &mut self,
        ctx: &mut LayoutCtx<'_>,
        _props: &mut PropertiesMut<'_>,
        bc: &BoxConstraints,
    ) -> Size {
        let max = bc.max();
        if ctx.fonts_changed() {
            // Fonts changed but runs/kind did not, so the per-block comparison
            // can't detect it: drop the cache to force a full reshape.
            self.layouts.clear();
            self.layouts_dirty = true;
        }

        let (left, page_width) = page_metrics(max.width);
        self.page_left = left;

        let (fcx, lcx) = ctx.text_contexts();
        self.ensure_layouts(fcx, lcx, page_width);

        let content = self.content_height.max(PAGE_TOP + PAGE_BOTTOM);
        let height = if bc.is_height_bounded() {
            content.max(max.height)
        } else {
            content
        };

        if ctx.is_focus_target()
            && let Some(rect) = self.focus_rect()
        {
            ctx.set_ime_area(rect);
        }

        Size::new(max.width, height)
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_>, _props: &PropertiesRef<'_>, scene: &mut Scene) {
        let size = ctx.size();
        self.paint_background(scene, size);

        let focused = ctx.is_focus_target();
        // Derive metrics from the current size (not the last layout pass) so a
        // resize can never present a frame wrapped to a stale width.
        let (left, width) = page_metrics(size.width);
        self.page_left = left;
        let (font_cx, layout_cx) = ctx.text_contexts();
        self.ensure_layouts(font_cx, layout_cx, width);

        self.paint_blocks(scene, font_cx, layout_cx, focused);
        self.paint_preedit(scene, font_cx, layout_cx, focused);
        self.paint_gutter(scene);
        self.paint_menu(scene, font_cx, layout_cx);
    }

    fn accessibility_role(&self) -> Role {
        Role::MultilineTextInput
    }

    fn accessibility(
        &mut self,
        _ctx: &mut AccessCtx<'_>,
        _props: &PropertiesRef<'_>,
        node: &mut Node,
    ) {
        let text = self
            .layouts
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        node.set_label(text);
    }

    fn children_ids(&self) -> ChildrenIds {
        ChildrenIds::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_places_caret() {
        use masonry::core::{NewWidget, WidgetTag};
        use masonry::theme::default_property_set;
        use masonry::ui_events::pointer::PointerButton;
        use masonry_testing::TestHarness;

        const EDITOR_TAG: WidgetTag<Editor> = WidgetTag::new("editor");

        let doc = crate::doc::YrsDocument::new();
        let block = doc.snapshot().remove(0).block;
        doc.set_text(&block, "hello world");

        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new_with_tag(
                Editor::new(Box::new(doc), BlinkTimer::disabled()),
                EDITOR_TAG,
            ),
            Size::new(800.0, 600.0),
        );

        harness.mouse_move((300.0, 80.0));
        harness.mouse_button_press(PointerButton::Primary);
        harness.mouse_button_release(PointerButton::Primary);

        let focus = harness.get_widget(EDITOR_TAG).test_focus();
        assert!(focus.1 > 0, "caret did not move on click: {focus:?}");
    }
}
