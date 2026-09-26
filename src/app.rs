//! The application controller.
//!
//! [`Driver`] reacts to widget actions and sync signals: it keeps the sidebar,
//! the editor and the [`Library`] in step, and swaps the editor's document when
//! the active page changes. Window construction lives in `main`.

use masonry::core::{ErasedAction, FromDynWidget, Widget, WidgetId, WidgetMut, WidgetTag};
use masonry::kurbo::Point;
use masonry::widgets::Portal;
use masonry_winit::app::{AppDriver, DriverCtx, WindowId};

use crate::doc::RemoteUpdate;
use crate::editor::{Editor, EditorAction};
use crate::library::Library;
use crate::platform;
use crate::session::{Signal, SyncSession};
use crate::sidebar::{Sidebar, SidebarAction, SidebarEntry};
use crate::{EDITOR_TAG, PORTAL_TAG, SIDEBAR_TAG};

/// Action sent by the blink timer thread to toggle the caret.
#[derive(Debug)]
pub(crate) struct BlinkTick;

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

pub(crate) struct Driver {
    window_id: WindowId,
    resizable_configured: bool,
    library: Library,
    /// Shared-list and active-page sync, or an inert session when offline.
    session: SyncSession,
}

impl Driver {
    pub(crate) fn new(window_id: WindowId, library: Library, session: SyncSession) -> Self {
        Self {
            window_id,
            resizable_configured: false,
            library,
            session,
        }
    }

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
        let active = self.library.active_index();
        self.with_sidebar(ctx, |mut sidebar| {
            Sidebar::set_entries(&mut sidebar, entries, active);
        });
    }

    /// Load the active library document into the editor.
    fn load_active(&mut self, ctx: &mut DriverCtx<'_, '_>) {
        let Some(id) = self.library.active_id() else {
            return;
        };
        let Some(doc) = self.library.open_document(&id) else {
            return;
        };
        self.session.connect_document(doc.handle(), id);
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

    /// Persist the catalog and refresh the sidebar and focus after a change.
    fn after_library_change(&mut self, ctx: &mut DriverCtx<'_, '_>) {
        self.library.save();
        self.sync_sidebar(ctx);
        self.focus_editor(ctx);
    }

    /// Keep the sidebar title in sync with the active document as it is edited,
    /// reading the cached layout.
    fn sync_title(&mut self, ctx: &mut DriverCtx<'_, '_>) {
        self.sync_title_with(ctx, Editor::title);
    }

    /// Like [`Self::sync_title`], but derives the title straight from the CRDT.
    /// Needed after a remote edit, before the relayout it schedules has run.
    fn sync_title_from_document(&mut self, ctx: &mut DriverCtx<'_, '_>) {
        self.sync_title_with(ctx, Editor::derive_document_title);
    }

    fn sync_title_with(&mut self, ctx: &mut DriverCtx<'_, '_>, read: impl Fn(&Editor) -> String) {
        let title = ctx
            .render_root(self.window_id)
            .get_widget_with_tag(EDITOR_TAG)
            .map(|editor| read(&editor));
        if let Some(title) = title {
            self.apply_active_title(ctx, title);
        }
    }

    /// Cache `title` for the active document, publishing it to the shared list
    /// and sidebar if it changed.
    fn apply_active_title(&mut self, ctx: &mut DriverCtx<'_, '_>, title: String) {
        let Some(id) = self.library.active_id() else {
            return;
        };
        let stale = self
            .library
            .entries()
            .iter()
            .find(|meta| meta.id == id)
            .map(|meta| meta.title.as_str())
            != Some(title.as_str());
        if !stale {
            return;
        }
        self.library.set_title(&id, &title);
        self.library.save();
        self.with_sidebar(ctx, |mut sidebar| {
            Sidebar::set_title(&mut sidebar, &id, title);
        });
    }

    /// Pull the shared catalog in, mirroring it to page files, and react to any
    /// change: a page may have appeared, vanished, or been renamed remotely.
    fn refresh_library(&mut self, ctx: &mut DriverCtx<'_, '_>) {
        let before = self.library.active_id();
        self.library.sync_files();
        // A peer could have removed the last page; never leave the library
        // empty, since the sidebar and editor both assume one page.
        if self.library.entries().is_empty() {
            self.library.create();
        }
        if self.library.active_id().is_none()
            && let Some(first) = self.library.entries().first()
        {
            self.library.set_active_id(&first.id);
        }
        self.library.save();
        self.sync_sidebar(ctx);
        if self.library.active_id() != before {
            self.load_active(ctx);
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
            self.resizable_configured = platform::configure_live_resize();
        }

        if action.is::<BlinkTick>() {
            self.with_editor(ctx, |mut editor| Editor::blink_tick(&mut editor));
            return;
        }

        if action.is::<EditorAction>() {
            self.sync_title(ctx);
            return;
        }

        if let Some(signal) = action.downcast_ref::<Signal>() {
            match signal {
                Signal::RemoteUpdate { room, update } => {
                    // Ignore edits for a page we have since navigated away from.
                    if self.library.active_id().as_deref() == Some(room.as_str()) {
                        let mut changed = false;
                        self.with_editor(ctx, |mut editor| {
                            changed = editor.widget.apply_remote_update(RemoteUpdate::new(update));
                            if changed {
                                editor.ctx.request_layout();
                            }
                        });
                        // An unchanged update is the server echoing our own edit
                        // back; the title already reflects it.
                        if changed {
                            self.sync_title_from_document(ctx);
                        }
                    }
                }
                Signal::LibraryChanged => self.refresh_library(ctx),
            }
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
            SidebarAction::Select(id) => {
                self.sync_title(ctx);
                self.library.set_active_id(id);
                self.load_active(ctx);
                self.after_library_change(ctx);
            }
            SidebarAction::Delete(id) => {
                self.sync_title(ctx);
                let was_active = self.library.active_id().as_deref() == Some(id.as_str());
                if self.library.delete(id) {
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
        self.library.save();
        self.session.disconnect();
        ctx.exit();
    }
}
