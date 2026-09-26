mod app;
mod blink;
mod doc;
mod editor;
mod fs;
mod library;
mod net;
mod platform;
mod session;
mod sidebar;
mod theme;

#[cfg(test)]
mod test_support;

use std::path::PathBuf;
use std::sync::Arc;

use masonry::core::{NewWidget, WidgetId, WidgetTag};
use masonry::properties::types::CrossAxisAlignment;
use masonry::theme::default_property_set;
use masonry::widgets::{Flex, Portal};
use masonry_winit::app::{EventLoop, MasonryUserEvent, NewWindow, WindowId, run_with};
use masonry_winit::winit::window::Window;

use crate::app::{BlinkTick, Driver};
use crate::blink::BlinkTimer;
use crate::editor::Editor;
use crate::library::Library;
use crate::session::{Notify, SyncSession};
use crate::sidebar::{Sidebar, SidebarEntry};

pub(crate) const EDITOR_TAG: WidgetTag<Editor> = WidgetTag::new("editor");
pub(crate) const SIDEBAR_TAG: WidgetTag<Sidebar> = WidgetTag::new("sidebar");
pub(crate) const PORTAL_TAG: WidgetTag<Portal<Editor>> = WidgetTag::new("editor-portal");

/// Where documents are persisted between sessions.
fn data_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("essor"))
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
    let sync_proxy = proxy.clone();
    let window_id = WindowId::next();

    // Timer thread: sleeps `BLINK_INTERVAL`; if the editor reports activity in the
    // meantime (caret moved/typing), the wait restarts so the caret stays solid.
    let blink = BlinkTimer::spawn(move || {
        let event = MasonryUserEvent::Action(window_id, Box::new(BlinkTick), WidgetId::next());
        proxy.send_event(event).is_ok()
    });

    // Optional real-time sync. Without `ESSOR_SYNC_URL` the app runs offline.
    let sync_url = std::env::var("ESSOR_SYNC_URL")
        .ok()
        .filter(|url| !url.is_empty());

    let mut library = Library::open(data_dir());

    let notify: Notify = {
        let proxy = sync_proxy.clone();
        Arc::new(move |signal| {
            let action: masonry::core::ErasedAction = Box::new(signal);
            let event = MasonryUserEvent::Action(window_id, action, WidgetId::next());
            let _ = proxy.send_event(event);
        })
    };
    let catalog = library.catalog_handle();
    let catalog_dirty = library.catalog_dirty();
    let mut session = SyncSession::start(sync_url, notify, catalog, catalog_dirty, &mut library);
    // Mirror the catalog to page files and persist it, then open the active page.
    library.sync_files();
    library.save();

    let active_id = library
        .active_id()
        .expect("library is seeded with at least one page");
    let entries: Vec<SidebarEntry> = library.entries().iter().map(SidebarEntry::from).collect();
    let active = library.active_index();
    let document = library
        .open_document(&active_id)
        .expect("the active page has a file");
    session.connect_document(document.handle(), active_id);

    let editor = Editor::new(Box::new(document), blink);
    let sidebar = Sidebar::new(entries, active);
    let portal = Portal::new(NewWidget::new_with_tag(editor, EDITOR_TAG));

    let root = Flex::row()
        .cross_axis_alignment(CrossAxisAlignment::Fill)
        .with_child(NewWidget::new_with_tag(sidebar, SIDEBAR_TAG))
        .with_flex_child(NewWidget::new_with_tag(portal, PORTAL_TAG), 1.0);

    let window_attributes = Window::default_attributes()
        .with_title("Essor")
        .with_resizable(true)
        .with_min_inner_size(masonry::dpi::LogicalSize::new(640.0, 420.0));

    let driver = Driver::new(window_id, library, session);

    run_with(
        event_loop,
        vec![
            NewWindow::new_with_id(window_id, window_attributes, NewWidget::new(root).erased())
                .with_base_color(crate::theme::SURFACE),
        ],
        driver,
        default_property_set(),
    )
    .unwrap();
}
