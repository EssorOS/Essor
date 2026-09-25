mod blink;
mod doc;
mod editor;

use masonry::core::{ErasedAction, NewWidget, WidgetId, WidgetTag};
use masonry::dpi::LogicalSize;
use masonry::theme::default_property_set;
use masonry::widgets::Portal;
use masonry_winit::app::{
    AppDriver, DriverCtx, EventLoop, MasonryUserEvent, NewWindow, WindowId, run_with,
};
use masonry_winit::winit::window::Window;

use crate::blink::BlinkTimer;
use crate::editor::Editor;

const EDITOR_TAG: WidgetTag<Editor> = WidgetTag::new("editor");

/// Action sent by the blink timer thread to toggle the caret.
#[derive(Debug)]
struct BlinkTick;

/// Where the document is persisted between sessions.
fn store_path() -> Option<std::path::PathBuf> {
    Some(dirs::data_dir()?.join("essor").join("essor.ydoc"))
}

/// Prevent macOS from stretching the last frame while the window is resized.
///
/// By default AppKit scales layer contents to the new bounds during a live resize,
/// which makes text momentarily appear stretched. Anchoring the contents top-left
/// makes it clip/reveal instead. Returns `true` once at least one window was set up.
#[cfg(target_os = "macos")]
fn configure_live_resize() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_quartz_core::kCAGravityTopLeft;

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
            // SAFETY: `kCAGravityTopLeft` is a framework-provided constant.
            let gravity = unsafe { kCAGravityTopLeft };
            layer.setContentsGravity(gravity);
            // The wgpu render layer is usually a sublayer, so pin it too.
            if let Some(sublayers) = unsafe { layer.sublayers() } {
                for sublayer in sublayers.iter() {
                    sublayer.setContentsGravity(gravity);
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

struct Driver {
    window_id: WindowId,
    resizable_configured: bool,
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
            let root = ctx.render_root(window_id);
            if root.get_widget_with_tag(EDITOR_TAG).is_some() {
                root.edit_widget_with_tag(EDITOR_TAG, |mut editor| {
                    Editor::blink_tick(&mut editor);
                });
            }
        }
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

    let document = Box::new(doc::YrsDocument::open(store_path()));
    let editor = Editor::new(document, blink);
    let root = Portal::new(NewWidget::new_with_tag(editor, EDITOR_TAG));

    let window_attributes = Window::default_attributes()
        .with_title("Essor")
        .with_resizable(true)
        .with_min_inner_size(LogicalSize::new(640.0, 420.0));

    let driver = Driver {
        window_id,
        resizable_configured: false,
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
