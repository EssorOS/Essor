mod blink;
mod doc;
mod editor;
mod fs;
mod library;
mod sidebar;

use std::path::PathBuf;

use masonry::core::{
    ErasedAction, FromDynWidget, NewWidget, Widget, WidgetId, WidgetMut, WidgetTag,
};
use masonry::dpi::LogicalSize;
use masonry::kurbo::Point;
use masonry::properties::types::CrossAxisAlignment;
use masonry::theme::default_property_set;
use masonry::widgets::{Flex, Portal};
use masonry_winit::app::{
    AppDriver, DriverCtx, EventLoop, MasonryUserEvent, NewWindow, WindowId, run_with,
};
use masonry_winit::winit::window::Window;

use crate::blink::BlinkTimer;
use crate::editor::{Editor, EditorAction};
use crate::library::Library;
use crate::sidebar::{Sidebar, SidebarAction, SidebarEntry};

const EDITOR_TAG: WidgetTag<Editor> = WidgetTag::new("editor");
const SIDEBAR_TAG: WidgetTag<Sidebar> = WidgetTag::new("sidebar");
const PORTAL_TAG: WidgetTag<Portal<Editor>> = WidgetTag::new("editor-portal");

/// Action sent by the blink timer thread to toggle the caret.
#[derive(Debug)]
struct BlinkTick;

/// Where documents are persisted between sessions.
fn data_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("essor"))
}

/// Prevent macOS from stretching the last frame while the window is resized.
///
/// By default AppKit scales layer contents to the new bounds during a live resize,
/// which makes text momentarily appear stretched. Anchoring the contents at the
/// visual top-left makes it clip/reveal instead. Returns `true` once at least one
/// window was set up.
#[cfg(target_os = "macos")]
fn configure_live_resize() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_quartz_core::{CALayer, kCAGravityBottomLeft, kCAGravityTopLeft};

    // `contentsGravity` is resolved in the layer's own coordinate space. Winit's
    // view is flipped, so `Top` would land at the visual bottom; pick the
    // constant that actually anchors the contents at the visual top.
    let anchor_top_left = |layer: &CALayer| {
        // SAFETY: Both are framework-provided constants.
        let gravity = unsafe {
            if layer.contentsAreFlipped() {
                kCAGravityBottomLeft
            } else {
                kCAGravityTopLeft
            }
        };
        layer.setContentsGravity(gravity);
    };

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    let mut configured = false;
    for window in windows.iter() {
        if let Some(view) = window.contentView()
            && let Some(layer) = view.layer()
        {
            anchor_top_left(&layer);
            // The wgpu render layer is usually a sublayer, so pin it too.
            if let Some(sublayers) = unsafe { layer.sublayers() } {
                for sublayer in sublayers.iter() {
                    anchor_top_left(&sublayer);
                }
            }
            configured = true;
        }
    }
    configured
}

#[cfg(not(target_os = "macos"))]
fn configure_live_resize() -> bool {
    true
}

/// Hand the widget tagged `tag` to `f`, if it is in the tree. Tags are set at
/// construction, so a missing tag means the window hasn't been built yet.
fn with_widget<W>(
    ctx: &mut DriverCtx<'_, '_>,
    window_id: WindowId,
    tag: WidgetTag<W>,
    f: impl FnOnce(WidgetMut<'_, W>),
) where
    W: Widget + FromDynWidget + ?Sized,
{
    if ctx
        .render_root(window_id)
        .get_widget_with_tag(tag)
        .is_some()
    {
        ctx.render_root(window_id).edit_widget_with_tag(tag, f);
    }
}

struct Driver {
    window_id: WindowId,
    resizable_configured: bool,
    library: Library,
}

impl Driver {
    /// Mutate the editor widget, if present.
    fn with_editor(&self, ctx: &mut DriverCtx<'_, '_>, f: impl FnOnce(WidgetMut<'_, Editor>)) {
        with_widget(ctx, self.window_id, EDITOR_TAG, f);
    }

    /// Mutate the sidebar widget, if present.
    fn with_sidebar(&self, ctx: &mut DriverCtx<'_, '_>, f: impl FnOnce(WidgetMut<'_, Sidebar>)) {
        with_widget(ctx, self.window_id, SIDEBAR_TAG, f);
    }

    /// Mutate the editor's scroll container, if present.
    fn with_portal(
        &self,
        ctx: &mut DriverCtx<'_, '_>,
        f: impl FnOnce(WidgetMut<'_, Portal<Editor>>),
    ) {
        with_widget(ctx, self.window_id, PORTAL_TAG, f);
    }

    /// The editor's current sidebar title, if the editor exists.
    fn editor_title(&self, ctx: &mut DriverCtx<'_, '_>) -> Option<String> {
        ctx.render_root(self.window_id)
            .get_widget_with_tag(EDITOR_TAG)
            .map(|editor| editor.title())
    }

    /// The editor's widget id, if it exists.
    fn editor_id(&self, ctx: &mut DriverCtx<'_, '_>) -> Option<WidgetId> {
        ctx.render_root(self.window_id)
            .get_widget_with_tag(EDITOR_TAG)
            .map(|editor| editor.id())
    }

    /// Push the library's document list into the sidebar widget.
    fn sync_sidebar(&self, ctx: &mut DriverCtx<'_, '_>) {
        let entries: Vec<SidebarEntry> = self
            .library
            .entries()
            .iter()
            .map(SidebarEntry::from)
            .collect();
        let active = self.library.active();
        self.with_sidebar(ctx, |mut sidebar| {
            Sidebar::set_entries(&mut sidebar, entries, active);
        });
    }

    /// Load the active library document into the editor.
    fn load_active(&self, ctx: &mut DriverCtx<'_, '_>) {
        let doc = self.library.open_document(self.library.active());
        self.with_editor(ctx, |mut editor| {
            editor.widget.load_document(Box::new(doc));
            editor.ctx.request_layout();
        });
        // Sync the scroll container: reset it to the top and have it recompute
        // its content size/scrollbar against the freshly loaded document.
        self.with_portal(ctx, |mut portal| {
            Portal::set_viewport_pos(&mut portal, Point::ZERO);
            portal.ctx.request_layout();
        });
    }

    /// Return keyboard focus to the editor after a sidebar interaction.
    fn focus_editor(&self, ctx: &mut DriverCtx<'_, '_>) {
        let id = self.editor_id(ctx);
        ctx.render_root(self.window_id).focus_on(id);
    }

    /// Persist the index and refresh the sidebar and focus after a library change.
    fn after_library_change(&mut self, ctx: &mut DriverCtx<'_, '_>) {
        self.library.save_index();
        self.sync_sidebar(ctx);
        self.focus_editor(ctx);
    }

    /// Keep the sidebar title in sync with the active document as it is edited.
    fn sync_title(&mut self, ctx: &mut DriverCtx<'_, '_>) {
        let Some(title) = self.editor_title(ctx) else {
            return;
        };
        let active = self.library.active();
        let stale = self
            .library
            .entries()
            .get(active)
            .map(|meta| meta.title.as_str())
            != Some(title.as_str());
        if stale {
            self.library.set_title(active, title.clone());
            self.with_sidebar(ctx, |mut sidebar| {
                Sidebar::set_title(&mut sidebar, active, title);
            });
        }
    }
}

impl AppDriver for Driver {
    fn on_action(
        &mut self,
        window_id: WindowId,
        ctx: &mut DriverCtx<'_, '_>,
        _widget_id: WidgetId,
        action: ErasedAction,
    ) {
        debug_assert_eq!(window_id, self.window_id, "unknown window");

        if !self.resizable_configured {
            self.resizable_configured = configure_live_resize();
        }

        if action.is::<BlinkTick>() {
            self.with_editor(ctx, |mut editor| Editor::blink_tick(&mut editor));
            return;
        }

        if action.is::<EditorAction>() {
            self.sync_title(ctx);
            return;
        }

        let Some(sidebar_action) = action.downcast_ref::<SidebarAction>() else {
            return;
        };

        match sidebar_action {
            SidebarAction::Create => {
                self.sync_title(ctx);
                self.library.create();
                self.load_active(ctx);
                self.after_library_change(ctx);
            }
            SidebarAction::Select(index) => {
                self.sync_title(ctx);
                self.library.set_active(*index);
                self.load_active(ctx);
                self.after_library_change(ctx);
            }
            SidebarAction::Delete(index) => {
                self.sync_title(ctx);
                let was_active = self.library.active() == *index;
                if self.library.delete(*index) {
                    if was_active {
                        self.load_active(ctx);
                    }
                    self.after_library_change(ctx);
                }
            }
        }
    }

    fn on_close_requested(&mut self, window_id: WindowId, ctx: &mut DriverCtx<'_, '_>) {
        debug_assert_eq!(window_id, self.window_id, "unknown window");
        self.sync_title(ctx);
        self.library.save_index();
        ctx.exit();
    }
}

fn main() {
    // Install a quiet subscriber so Masonry doesn't set up its verbose DEBUG one.
    // If the user provides RUST_LOG, let Masonry's EnvFilter handle it instead.
    if std::env::var_os("RUST_LOG").is_none() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
            .try_init();
    }

    let event_loop = EventLoop::with_user_event().build().unwrap();
    let proxy = event_loop.create_proxy();
    let window_id = WindowId::next();

    // Timer thread: sleeps `BLINK_INTERVAL`; if the editor reports activity in the
    // meantime (caret moved/typing), the wait restarts so the caret stays solid.
    let blink = BlinkTimer::spawn(move || {
        let event = MasonryUserEvent::Action(window_id, Box::new(BlinkTick), WidgetId::next());
        proxy.send_event(event).is_ok()
    });

    let library = Library::open(data_dir());
    let active = library.active();
    let entries: Vec<SidebarEntry> = library.entries().iter().map(SidebarEntry::from).collect();
    let document = Box::new(library.open_document(active));

    let editor = Editor::new(document, blink);
    let sidebar = Sidebar::new(entries, active);
    let portal = Portal::new(NewWidget::new_with_tag(editor, EDITOR_TAG));

    let root = Flex::row()
        .cross_axis_alignment(CrossAxisAlignment::Fill)
        .with_child(NewWidget::new_with_tag(sidebar, SIDEBAR_TAG))
        .with_flex_child(NewWidget::new_with_tag(portal, PORTAL_TAG), 1.0);

    let window_attributes = Window::default_attributes()
        .with_title("Essor")
        .with_resizable(true)
        .with_min_inner_size(LogicalSize::new(640.0, 420.0));

    let driver = Driver {
        window_id,
        resizable_configured: false,
        library,
    };

    run_with(
        event_loop,
        vec![
            NewWindow::new_with_id(window_id, window_attributes, NewWidget::new(root).erased())
                .with_base_color(masonry::peniko::Color::from_rgb8(0xff, 0xff, 0xff)),
        ],
        driver,
        default_property_set(),
    )
    .unwrap();
}
