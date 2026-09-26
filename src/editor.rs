use std::collections::HashMap;
use std::time::{Duration, Instant};

use masonry::accesskit::{Node, Role};
use masonry::core::{
    AccessCtx, AccessEvent, BoxConstraints, BrushIndex, ChildrenIds, EventCtx, Ime, LayoutCtx,
    PaintCtx, PointerEvent, PropertiesMut, PropertiesRef, RegisterCtx, TextEvent, Update,
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
use crate::doc::{BlockId, BlockKind, Doc, Mark, TextRun};

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

/// How long the layout width must be stable before a deferred resize reflow
/// runs. Long enough to ride out the width updates of a live resize, short
/// enough that the reflow lands promptly once the drag stops.
const REFLOW_DELAY: Duration = Duration::from_millis(160);

/// The vertical slice of the document worth painting, in content coordinates.
/// Outside a scrolling viewport the slice is unbounded, so everything is drawn.
#[derive(Clone, Copy)]
pub(super) struct Visible {
    top: f64,
    bottom: f64,
}

impl Visible {
    /// Whether a block spanning `[top, top + height)` overlaps the slice.
    pub(super) fn contains(&self, top: f64, height: f64) -> bool {
        top + height >= self.top && top <= self.bottom
    }

    /// The slice clamped to a widget of the given height.
    pub(super) fn clamped_to(&self, height: f64) -> (f64, f64) {
        (
            self.top.max(0.0).min(height),
            self.bottom.max(0.0).min(height),
        )
    }
}

/// Signals the editor raises to the application.
#[derive(Debug)]
pub enum EditorAction {
    /// The document changed; anything derived from it may need refreshing.
    Edited,
}

/// A custom, GPU-rendered block editor.
///
/// The widget owns the [`Doc`] (a page over the shared workspace),
/// caches a Parley [`Layout`] per block, and paints through Vello. Shaping,
/// font fallback and glyph rasterization are delegated to Parley/Vello; the
/// document model, layout cache, cursor/selection, and editing semantics are ours.
pub struct Editor {
    doc: Box<dyn Doc>,
    layouts: Vec<BlockLayout>,
    /// Maps each block's stable id to its current slot in `layouts`. Rebuilt
    /// with `layouts`, so selection and geometry address blocks by id rather
    /// than by a position that a remote edit can shift out from under them.
    block_index: HashMap<BlockId, usize>,
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
    /// Height of the scrolling viewport, captured from the parent's constraints
    /// during layout. Used to cull blocks that are scrolled out of view.
    viewport_height: f64,
    /// Pending width change from a live resize, applied once the drag settles.
    reflow: Reflow,
}

impl Editor {
    pub fn new(doc: Box<dyn Doc>, blink: BlinkTimer) -> Self {
        let mut editor = Self {
            doc,
            layouts: Vec::new(),
            block_index: HashMap::new(),
            layout_width: 0.0,
            page_left: MIN_CONTENT_LEFT,
            content_height: 0.0,
            layouts_dirty: true,
            selection: SelectionState::default(),
            preedit: String::new(),
            hover: None,
            menu: None,
            caret_visible: true,
            focused: false,
            last_activity: Instant::now(),
            blink,
            viewport_height: 0.0,
            reflow: Reflow::new(),
        };
        editor.reset_for_new_document();
        editor
    }

    /// The caret moved (or the user interacted): show it immediately and restart
    /// the blink timer so it stays solid for a full interval.
    fn activity(&mut self) {
        self.caret_visible = true;
        self.last_activity = Instant::now();
        self.blink.reset();
    }

    /// Swap in a different document, discarding all cached state so it is laid
    /// out and painted from scratch on the next pass.
    pub fn load_document(&mut self, doc: Box<dyn Doc>) {
        self.doc = doc;
        self.reset_for_new_document();
    }

    /// A remote merge touched this page: relayout on the next pass and show the
    /// caret. The records were already applied to the workspace by the caller.
    pub fn refresh(&mut self) {
        self.layouts_dirty = true;
        self.activity();
    }

    /// The sidebar title derived straight from the document, bypassing the
    /// layout cache. Used after a remote edit, before the relayout has run.
    pub fn derive_document_title(&self) -> String {
        crate::title::derive_title(&*self.doc)
    }

    /// Discard all cached layout and interaction state so the current document
    /// is re-shaped and painted from scratch.
    fn reset_for_new_document(&mut self) {
        self.layouts.clear();
        self.block_index.clear();
        self.layout_width = 0.0;
        self.content_height = 0.0;
        self.layouts_dirty = true;
        self.hover = None;
        self.menu = None;
        self.preedit.clear();
        self.selection = SelectionState::default();
        self.reflow.reset();
        self.activity();
    }

    /// If a width change was deferred during a live resize and the width has
    /// since been stable, flag a reflow. Returns `true` when a layout pass should
    /// be requested to apply it.
    fn flush_deferred_reflow(&mut self) -> bool {
        if self.reflow.ready(self.layout_width) {
            self.reflow.request();
            return true;
        }
        false
    }

    /// The vertical range of blocks worth painting, in content coordinates.
    /// `scroll` is the current scroll offset. A full viewport of overscan above
    /// and below keeps scrolling from revealing unpainted content while repaints
    /// stay bounded by the viewport instead of the whole document.
    fn visible_range(&self, scroll: f64) -> Visible {
        if self.viewport_height <= 0.0 {
            return Visible {
                top: f64::NEG_INFINITY,
                bottom: f64::INFINITY,
            };
        }
        let overscan = self.viewport_height;
        Visible {
            top: scroll - overscan,
            bottom: scroll + self.viewport_height + overscan,
        }
    }

    /// A short title for the sidebar: the first non-empty line of the document.
    pub fn title(&self) -> String {
        crate::title::first_title(self.layouts.iter().map(|block| block.text.as_str()))
    }

    /// Blink the caret. Driven by a timer thread (see `main.rs`) that wakes the app
    /// every half second; we toggle and request a single repaint. This avoids a
    /// continuous animation-frame loop, which starved event-driven repaints.
    pub fn blink_tick(this: &mut WidgetMut<'_, Self>) {
        // Apply a deferred resize reflow even when the editor isn't focused.
        if this.widget.flush_deferred_reflow() {
            this.ctx.request_layout();
        }
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

    /// The id of the block currently laid out at `index`.
    fn block(&self, index: usize) -> Option<BlockId> {
        self.layouts.get(index).map(|layout| layout.id)
    }

    /// The current slot of the block with `id`, if it is laid out.
    fn index_of(&self, id: BlockId) -> Option<usize> {
        self.block_index.get(&id).copied()
    }

    /// The cached layout of the block with `id`, if it is laid out.
    fn layout_of(&self, id: BlockId) -> Option<&BlockLayout> {
        self.index_of(id).and_then(|index| self.layouts.get(index))
    }

    /// Flush pending content changes: rebuild layouts and request a relayout.
    /// Safe to call when nothing is dirty.
    fn flush_edits(&mut self, ctx: &mut EventCtx<'_>) {
        if self.layouts_dirty {
            self.refresh_layouts(ctx);
            ctx.request_layout();
            // Let the application refresh anything derived from the document
            // (e.g. the sidebar title) as soon as content changes.
            ctx.submit_action::<EditorAction>(EditorAction::Edited);
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
    type Action = EditorAction;

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

        // Apply a deferred reflow if the width has settled since the last pass.
        self.flush_deferred_reflow();

        let (left, page_width) = page_metrics(max.width);
        self.page_left = left;

        let (fcx, lcx) = ctx.text_contexts();
        self.ensure_layouts(fcx, lcx, page_width);

        // Report the full content height so the enclosing `Portal` can scroll us.
        // The parent hands us bounded height constraints, which is the viewport.
        if bc.is_height_bounded() {
            self.viewport_height = max.height;
        }

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
        // The enclosing `Portal` scrolls us by translating our coordinate space,
        // so the scroll offset is the negation of our window-space origin.
        let scroll = -ctx.window_origin().y;
        let visible = self.visible_range(scroll);
        self.paint_background(scene, size, visible);

        let focused = ctx.is_focus_target();
        // Layout always runs before paint, and every edit requests a new layout
        // pass, so the cached layouts (and `page_left`) are current here. Rebuilding
        // them would call `doc.snapshot()` on every repaint — including caret blinks
        // and hover — which is far too expensive for content-heavy documents.
        let (font_cx, layout_cx) = ctx.text_contexts();
        self.paint_blocks(scene, font_cx, layout_cx, focused, visible);
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
    use crate::library::Library;
    use crate::model::{RecordPayload, TextRun};
    use masonry::core::{NewWidget, WidgetTag};
    use masonry::theme::default_property_set;
    use masonry_testing::TestHarness;

    const TAG: WidgetTag<Editor> = WidgetTag::new("editor");

    /// A remote merge followed by the driver's `refresh` + `request_layout` must
    /// rebuild the cached layouts so the new content is visible.
    #[test]
    fn refresh_rebuilds_layouts_after_a_remote_edit() {
        let mut library = Library::open(None).unwrap();
        let page = library.create();
        let handle = library.open_document(&page).unwrap();
        let workspace = library.workspace();
        let block = handle.snapshot().remove(0).id;
        handle.set_text(block, "local");

        let mut harness = TestHarness::create_with_size(
            default_property_set(),
            NewWidget::new_with_tag(Editor::new(Box::new(handle), BlinkTimer::disabled()), TAG),
            Size::new(800.0, 600.0),
        );
        assert_eq!(harness.get_widget(TAG).title(), "local");

        // Simulate a remote edit: a newer version on the same block.
        let mut remote = workspace.get(&block.to_simple()).unwrap();
        remote.version = workspace.next_clock();
        remote.payload = RecordPayload::Block {
            kind: BlockKind::Paragraph,
            runs: vec![TextRun {
                text: "REMOTE".to_string(),
                bold: false,
                italic: false,
            }],
        };
        workspace.merge(vec![remote]);

        harness.edit_widget(TAG, |mut editor| {
            editor.widget.refresh();
            editor.ctx.request_layout();
        });

        assert_eq!(
            harness.get_widget(TAG).title(),
            "REMOTE",
            "the editor did not rebuild its layouts after refresh"
        );
    }
}
