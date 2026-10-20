//! Independent native views over one live Session. Only the UI thread mutates documents.
//! A secondary shell borrows the session for its pass, then returns it even during unwinding.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use egui::{ViewportBuilder, ViewportCommand, ViewportId};
use pdfcraft_engine::{DocId, Session};
use pdfcraft_render::RenderPool;

use crate::{DocView, PdfCraftApp, RightPanel, canvas, dialogs, panels, theme, widgets};

pub(crate) struct ViewRenderer {
    pub pool: RenderPool,
    display: Arc<Vec<u8>>,
    generation: u64,
}

pub(crate) struct Window {
    viewport: ViewportId,
    pub app: Box<PdfCraftApp>,
    client: Option<crate::control::ControlClient>,
}

#[derive(Default)]
pub(crate) struct Windows {
    pub entries: Vec<Window>,
    pub secondary: bool,
    pub elsewhere: HashSet<DocId>,
    pub renderers: HashMap<DocId, ViewRenderer>,
    pub(crate) versions: HashMap<DocId, u64>,
    pub hidden: bool,
    restore_primary: Option<DocId>,
    next: u64,
}

/// A session has exactly one owner even if a view panics while it is borrowing it.
struct Lease<'a> {
    owner: &'a mut Session,
    app: &'a mut PdfCraftApp,
    recovery: Option<(&'a mut Option<crate::RecoveryStore>, &'a mut HashMap<DocId, String>)>,
    #[cfg(not(target_arch = "wasm32"))]
    watch: Option<&'a mut crate::file_watch::FileWatch>,
}

impl<'a> Lease<'a> {
    fn new(owner: &'a mut Session, app: &'a mut PdfCraftApp) -> Self {
        std::mem::swap(owner, &mut app.session);
        Self {
            owner,
            app,
            recovery: None,
            #[cfg(not(target_arch = "wasm32"))]
            watch: None,
        }
    }

    fn with_recovery(mut self, store: &'a mut Option<crate::RecoveryStore>, keys: &'a mut HashMap<DocId, String>) -> Self {
        std::mem::swap(store, &mut self.app.recovery);
        std::mem::swap(keys, &mut self.app.recovery_keys);
        self.recovery = Some((store, keys));
        self
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn with_watch(mut self, watch: &'a mut crate::file_watch::FileWatch) -> Self {
        std::mem::swap(watch, &mut self.app.watch);
        self.watch = Some(watch);
        self
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        std::mem::swap(self.owner, &mut self.app.session);
        if let Some((store, keys)) = &mut self.recovery {
            std::mem::swap(*store, &mut self.app.recovery);
            std::mem::swap(*keys, &mut self.app.recovery_keys);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(watch) = &mut self.watch {
            std::mem::swap(*watch, &mut self.app.watch);
        }
    }
}

impl PdfCraftApp {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn reattach_shared_document(&mut self, path: &str) -> Option<usize> {
        let doc = self.session.docs().iter().find(|doc| doc.path.as_deref().is_some_and(|p| crate::same_file(p, path)))?;
        let source = self.shared_windows.entries.iter().flat_map(|w| &w.app.views).find(|v| v.id == doc.id)?;
        let mut view = DocView::new(doc.id, &doc.info, self.view_defaults);
        view.zoom = source.zoom;
        view.fit = source.fit;
        view.layout = source.layout;
        view.continuous = source.continuous;
        view.rotation = source.rotation;
        view.cover = source.cover;
        view.go_to_page(source.current);
        self.views.push(view);
        self.views.len().checked_sub(1)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn with_shared_document_view<R>(&mut self, id: DocId, action: impl FnOnce(&mut Self) -> R) -> Option<R> {
        let window = self.shared_windows.entries.iter_mut().find(|w| w.app.views.iter().any(|v| v.id == id))?;
        let lease = Lease::new(&mut self.session, &mut window.app).with_recovery(&mut self.recovery, &mut self.recovery_keys);
        #[cfg(not(target_arch = "wasm32"))]
        let lease = lease.with_watch(&mut self.watch);
        Some(action(lease.app))
    }

    pub(crate) fn document_open_elsewhere(&self, id: DocId) -> bool {
        self.shared_windows.elsewhere.contains(&id) || self.shared_windows.entries.iter().any(|w| w.app.views.iter().any(|v| v.id == id))
    }

    /// Open a second editable view without reading, copying or saving the PDF.
    /// Repeats focus the existing view; the first version intentionally allows two views.
    pub fn open_document_window(&mut self, id: DocId) -> Result<(), String> {
        if cfg!(target_arch = "wasm32") {
            return Err("Document windows are available in the desktop app".into());
        }
        if self.shared_windows.secondary {
            self.shared_windows.restore_primary = Some(id);
            if let Some(ctx) = &self.ctx {
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
            }
            return Ok(());
        }
        if let Some(window) = self.shared_windows.entries.iter().find(|w| w.app.views.iter().any(|v| v.id == id)) {
            if let Some(ctx) = &self.ctx {
                ctx.send_viewport_cmd_to(window.viewport, ViewportCommand::Focus);
            }
            return Ok(());
        }
        let source = self.views.iter().find(|v| v.id == id).ok_or("The document has no open view")?;
        let doc = self.session.get(id).ok_or("The document is closed")?;
        let mut view = DocView::new(id, &doc.info, self.view_defaults);
        view.zoom = source.zoom;
        view.fit = source.fit;
        view.layout = source.layout;
        view.continuous = source.continuous;
        view.rotation = source.rotation;
        view.cover = source.cover;
        view.go_to_page(source.current);
        let mut app = Box::new(Self::new());
        app.restore(&self.persist());
        app.mode = self.mode;
        app.quick_tool = self.quick_tool;
        app.comment_prefs = self.comment_prefs.clone();
        app.right = self.right;
        app.active = Some(0);
        app.views.push(view);
        app.shared_windows.secondary = true;
        app.shared_windows.elsewhere.extend(self.views.iter().map(|v| v.id));
        app.fonts_ready = true;
        app.styled = true;
        self.shared_windows.versions.insert(id, doc.edit_generation());
        self.shared_windows.next = self.shared_windows.next.saturating_add(1);
        let viewport = ViewportId::from_hash_of(("shared-document", self.shared_windows.next));
        if let Some(view) = app.views.first_mut() {
            view.ui_namespace = viewport;
        }
        self.shared_windows.entries.push(Window { viewport, app, client: None });
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
        Ok(())
    }

    /// Adopt edits made through another view while preserving this view's navigation/drafts.
    pub(crate) fn sync_shared_document_views(&mut self) {
        if self.shared_windows.secondary
            && let Some((_, id)) = self.active_ids()
        {
            self.reading_colors = self.session.reading_colors(id).unwrap_or_default();
        }
        if !self.shared_windows.secondary && self.shared_windows.versions.is_empty() {
            return;
        }
        for view in &mut self.views {
            if !self.shared_windows.secondary && !self.shared_windows.versions.contains_key(&view.id) {
                continue;
            }
            let Some(doc) = self.session.get(view.id) else { continue };
            let generation = doc.edit_generation();
            if self.shared_windows.versions.insert(view.id, generation).is_some_and(|old| old != generation) {
                let goto = view.goto;
                view.document_changed(&doc.info);
                // A pending navigation belongs to this window, even if the other just edited.
                view.goto = goto.map(|(page, offset)| (page.min(doc.info.pages.len().saturating_sub(1)), offset));
            }
            if self.shared_windows.secondary
                && self.shared_windows.renderers.get(&view.id).is_none_or(|r| r.generation != generation || !Arc::ptr_eq(&r.display, &doc.display))
            {
                // A replacement pool has no queue yet; cached last_queue must not suppress
                // submitting this window's demand, nor admit results from the old bytes.
                view.restart_render_queue();
                self.shared_windows.renderers.insert(view.id, ViewRenderer { pool: doc.view_renderer(), display: doc.display.clone(), generation });
            }
        }
        if !self.shared_windows.secondary {
            self.shared_windows.versions.retain(|id, _| self.shared_windows.entries.iter().any(|w| w.app.views.iter().any(|v| v.id == *id)));
        }
        self.shared_windows.renderers.retain(|id, _| self.views.iter().any(|v| v.id == *id));
    }

    pub(crate) fn hide_primary_for_children(&mut self, ctx: &egui::Context) -> bool {
        if self.shared_windows.entries.is_empty() || self.shared_windows.secondary {
            return false;
        }
        // Even a rejected form edit or unfinished note must cancel the native close request.
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        let ids: Vec<_> = self.views.iter().map(|v| v.id).collect();
        if ids.into_iter().any(|id| self.shared_draft_open(id)) {
            return true;
        }
        if !self.process_pending_edits() {
            return true;
        }
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.shared_windows.hidden = true;
        true
    }

    pub(crate) fn shared_draft_open(&mut self, id: DocId) -> bool {
        let pending = self.views.iter().find(|v| v.id == id).is_some_and(|v| {
            v.comments.composer.as_ref().is_some_and(|c| !c.text.trim().is_empty())
                || v.comments.editing.is_some()
                || !v.comments.reply.trim().is_empty()
                || !v.comments.add_box.trim().is_empty()
        });
        if pending {
            self.notify_tr("Post or cancel the unfinished comment before closing this view");
        }
        pending
    }

    /// Application Quit first commits the other shells' typing and returns every last view to
    /// the primary shell, where the existing save/discard/cancel sequence can protect it.
    pub(crate) fn prepare_shared_quit(&mut self) -> bool {
        let ids: Vec<_> = self.views.iter().map(|v| v.id).collect();
        if ids.into_iter().any(|id| self.shared_draft_open(id)) {
            return false;
        }
        let mut windows = std::mem::take(&mut self.shared_windows.entries);
        for window in &mut windows {
            let lease = Lease::new(&mut self.session, &mut window.app).with_recovery(&mut self.recovery, &mut self.recovery_keys);
            #[cfg(not(target_arch = "wasm32"))]
            let lease = lease.with_watch(&mut self.watch);
            let id = lease.app.active_ids().map(|(_, id)| id);
            if !lease.app.process_pending_edits() || id.is_some_and(|id| lease.app.shared_draft_open(id) || !lease.app.commit_typing_in(id)) {
                drop(lease);
                self.shared_windows.entries = windows;
                return false;
            }
        }
        for window in windows {
            for view in window.app.views {
                if !self.views.iter().any(|v| v.id == view.id) {
                    self.views.push(view);
                }
            }
        }
        self.shared_windows.hidden = false;
        true
    }

    pub(crate) fn show_shared_windows(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Own edits already refreshed their local view. Only a later change from a sibling
        // should invalidate it on the next pass.
        for view in &self.views {
            if self.shared_windows.versions.contains_key(&view.id)
                && let Some(doc) = self.session.get(view.id)
            {
                self.shared_windows.versions.insert(view.id, doc.edit_generation());
            }
        }
        let mut windows = std::mem::take(&mut self.shared_windows.entries);
        let primary: HashSet<_> = self.views.iter().map(|v| v.id).collect();
        for window in &mut windows {
            window.app.shared_windows.elsewhere = primary.clone();
            let title = window
                .app
                .views
                .first()
                .and_then(|v| self.session.get(v.id))
                .map_or_else(|| "PdfCraft".to_string(), |doc| format!("{} — {} — PdfCraft", doc.name, tl!("Second window")));
            {
                let lease = Lease::new(&mut self.session, &mut window.app).with_recovery(&mut self.recovery, &mut self.recovery_keys);
                #[cfg(not(target_arch = "wasm32"))]
                let lease = lease.with_watch(&mut self.watch);
                ctx.show_viewport_immediate(
                    window.viewport,
                    ViewportBuilder::default().with_title(title).with_inner_size([1100.0, 850.0]).with_min_inner_size([640.0, 420.0]),
                    |ui, _class| lease.app.shared_window_ui(ui),
                );
            }
            if let Some(id) = window.app.shared_windows.restore_primary.take() {
                if !self.views.iter().any(|v| v.id == id)
                    && let Some(doc) = self.session.get(id)
                {
                    self.views.push(DocView::new(id, &doc.info, self.view_defaults));
                }
                self.active = self.views.iter().position(|v| v.id == id);
                self.shared_windows.hidden = false;
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            // Opening a PDF attachment or extracting pages uses the regular main-window tabs;
            // the secondary stays a single reading view and never hides newly opened documents.
            if window.app.views.len() > 1 {
                self.views.extend(window.app.views.split_off(1));
                self.active = self.views.len().checked_sub(1);
                window.app.active = Some(0);
                self.shared_windows.hidden = false;
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
        }
        windows.retain(|w| !w.app.views.is_empty());
        self.shared_windows.entries = windows;
        if self.shared_windows.hidden && self.shared_windows.entries.is_empty() {
            self.shared_windows.hidden = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            self.request_quit(ctx);
        }
    }

    fn shared_window_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.ctx = Some(ctx.clone());
        if let Some(mut control) = self.control.take() {
            control.tick(&ctx, self);
            self.control = Some(control);
        }
        self.sync_shared_document_views();
        self.scroll_speed.set_line_speed(&ctx);
        self.motion.begin_pass(&ctx, self.animation_effects);
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.poll_pending_opens();
            self.process_picked();
        }
        self.process_file_requests();
        let deferred = std::mem::take(&mut self.deferred_commands);
        self.shortcuts(&ctx);
        if self.process_pending_edits() {
            for (command, doc) in deferred {
                if self.active_ids().map(|(_, id)| id) == doc {
                    self.execute(command);
                }
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.request_close_tab(0);
        }
        if self.views.is_empty() {
            return;
        }
        for view in &mut self.views {
            view.begin_render_frame();
        }
        let viewport = ctx.viewport_id();
        egui::Panel::top(egui::Id::new(("shared-toolbar", viewport))).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (label, command) in
                    [("Save", "file.save"), ("Save as…", "file.save_as"), ("Undo", "edit.undo"), ("Redo", "edit.redo"), ("Find…", "edit.find")]
                {
                    let enabled = pdfcraft_engine::commands::command(command).is_some_and(|spec| self.command_enabled(spec));
                    if ui.add_enabled(enabled, egui::Button::new(tl!(label))).clicked() {
                        self.execute(command);
                    }
                }
                if ui.button(tl!("Show primary window")).clicked()
                    && let Some((_, id)) = self.active_ids()
                {
                    let _ = self.open_document_window(id);
                }
                ui.separator();
                for (label, panel) in
                    [("Comments", RightPanel::Comments), ("Bookmarks", RightPanel::Bookmarks), ("Page thumbnails", RightPanel::Pages)]
                {
                    if ui.selectable_label(self.right == Some(panel), tl!(label)).clicked() {
                        self.right = if self.right == Some(panel) { None } else { Some(panel) };
                    }
                }
            });
            ui.horizontal(|ui| {
                let view = &mut self.views[0]; // The nonempty view was checked above; this shell holds one view.
                if ui.button("◀").clicked() {
                    view.step_page(false);
                }
                let pages = self.session.get(view.id).map_or(0, |d| d.info.pages.len());
                let mut page = view.current.saturating_add(1);
                if ui.add(egui::DragValue::new(&mut page).range(1..=pages.max(1))).changed() {
                    view.go_to_page(page.saturating_sub(1));
                }
                ui.label(format!("/ {pages}"));
                if ui.button("▶").clicked() {
                    view.step_page(true);
                }
                if ui.button("−").clicked() {
                    view.zoom_step(false);
                }
                if ui.button("+").clicked() {
                    view.zoom_step(true);
                }
                if ui.button(tl!("Fit to width")).clicked() {
                    view.fit = canvas::Fit::Width;
                }
                if self.session.get(view.id).is_some_and(|d| d.dirty) {
                    ui.label(tl!("Unsaved changes"));
                }
                ui.label(tl!("Edits are shared; reading positions are independent"));
            });
        });
        if self.right.is_some() {
            egui::Panel::right(egui::Id::new(("shared-inspector", viewport)))
                .resizable(true)
                .default_size(300.0)
                .show(ui, |ui| panels::right_panel(self, ui));
        }
        let t = theme::Tokens::get(&ctx);
        if !self.views.is_empty() {
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.pasteboard)).show(ui, |ui| canvas::document_area(self, 0, ui));
        }
        self.process_pending_edits();
        dialogs::show(self, &ctx);
        widgets::toast(self, &ctx);
        self.sync_shared_document_views();
        self.finish_render_frame(&ctx);
        for view in &self.views {
            if let Some(doc) = self.session.get(view.id) {
                self.shared_windows.versions.insert(view.id, doc.edit_generation());
            }
        }
    }

    pub(crate) fn shared_windows_state(&self) -> serde_json::Value {
        serde_json::json!(
            self.shared_windows
                .entries
                .iter()
                .filter_map(|window| {
                    let view = window.app.views.first()?;
                    let doc = self.session.get(view.id)?;
                    Some(serde_json::json!({
                        "doc": view.id.0, "name": doc.name, "path": doc.path,
                        "page": view.current + 1, "pages": doc.info.pages.len(),
                        "zoom_percent": (view.zoom * 100.0).round(), "dirty": doc.dirty,
                        "annotations": doc.info.annotations.len(), "tool": format!("{:?}", window.app.quick_tool),
                    }))
                })
                .collect::<Vec<_>>()
        )
    }

    /// Route through the already opt-in parent connection; no additional listener or token.
    pub(crate) fn route_shared_window(
        &mut self,
        doc: u64,
        method: &str,
        params: serde_json::Value,
    ) -> Result<std::sync::mpsc::Receiver<crate::control::Reply>, String> {
        if method == "ui.screenshot" {
            return Err(
                "The immediate-window renderer does not support secondary screenshots; capture the primary window or use native window capture"
                    .into(),
            );
        }
        let window = self
            .shared_windows
            .entries
            .iter_mut()
            .find(|w| w.app.views.iter().any(|v| v.id == DocId(doc)))
            .ok_or("The document has no second window")?;
        if window.client.is_none() {
            let ctx = self.ctx.as_ref().ok_or("The interface has not drawn yet")?;
            let (control, client) = crate::control::attach_for(ctx, window.viewport);
            window.app.control = Some(control);
            window.client = Some(client);
        }
        window.client.as_ref().map(|client| client.send(method, params)).ok_or_else(|| "The window control is unavailable".into())
    }

    /// Opt-in UI control surface. `doc` chooses a document's second view.
    pub(crate) fn shared_window_control(&mut self, params: &serde_json::Value) -> crate::control::Reply {
        use crate::control::Host;
        let action = params.get("action").and_then(serde_json::Value::as_str).unwrap_or("state");
        if action == "state" {
            return Ok(self.shared_windows_state());
        }
        if action == "hide-primary" {
            let ctx = self.ctx.clone().ok_or("The interface has not drawn yet")?;
            if !self.hide_primary_for_children(&ctx) {
                return Err("Open a second document window before hiding the primary window".into());
            }
            return Ok(serde_json::json!({"primary_hidden":self.shared_windows.hidden}));
        }
        let id = params
            .get("doc")
            .and_then(serde_json::Value::as_u64)
            .map(DocId)
            .or_else(|| self.active_ids().map(|(_, id)| id))
            .or_else(|| self.shared_windows.entries.first().and_then(|w| w.app.views.first()).map(|v| v.id))
            .ok_or("No document is open")?;
        if action == "open" {
            self.open_document_window(id)?;
            return Ok(self.shared_windows_state());
        }
        let index =
            self.shared_windows.entries.iter().position(|w| w.app.views.iter().any(|v| v.id == id)).ok_or("The document has no second window")?;
        let window = self.shared_windows.entries.get_mut(index).ok_or("The window is closed")?;
        window.app.shared_windows.elsewhere = self.views.iter().map(|v| v.id).collect();
        let lease = Lease::new(&mut self.session, &mut window.app).with_recovery(&mut self.recovery, &mut self.recovery_keys);
        #[cfg(not(target_arch = "wasm32"))]
        let lease = lease.with_watch(&mut self.watch);
        let result = match action {
            "command" => lease.app.command(params.get("id").and_then(serde_json::Value::as_str).ok_or("command needs id")?),
            "set" => {
                let key = params.get("key").and_then(serde_json::Value::as_str).ok_or("set needs key")?;
                let value = params.get("value").ok_or("set needs value")?;
                let value = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
                lease.app.set(key, &value)
            }
            "close" => {
                lease.app.request_close_tab(0);
                Ok(serde_json::json!({"closed": lease.app.views.is_empty(), "close_prompt": lease.app.close_request.is_some()}))
            }
            _ => Err("ui.window action must be state, open, set, command, close or hide-primary".into()),
        };
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
        result
    }

    pub(crate) fn secondary_command_allowed(&self, id: &str) -> bool {
        if self.shared_windows.secondary && id.starts_with("view.theme") {
            return false;
        }
        !self.shared_windows.secondary
            || matches!(id, "file.save" | "file.save_as" | "file.close")
            || id.starts_with("edit.")
            || id.starts_with("comment.")
            || (id.starts_with("view.") && !matches!(id, "view.palette" | "view.next_tab" | "view.previous_tab" | "view.full_screen"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CloseRequest, QuickTool, SaveTarget};
    use pdfcraft_engine::{Edit, NewAnnotation, NoteIcon, Shape, Style};
    use pdfcraft_render::{RenderRequest, RequestKind};
    use serde_json::json;

    fn app() -> (PdfCraftApp, DocId) {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        let bytes = app.session.create_blank(300.0, 400.0, 3).unwrap();
        app.open_bytes("chapters.pdf", None, bytes.as_ref().clone()).unwrap();
        let id = app.views[0].id;
        (app, id)
    }

    fn note(page: usize, text: &str) -> Edit {
        Edit::AddAnnotation(NewAnnotation {
            page,
            shape: Shape::Note { at: [40.0, 60.0], icon: NoteIcon::Comment },
            style: Style::default(),
            contents: text.into(),
            author: "Reader".into(),
        })
    }

    #[test]
    fn two_views_share_live_edits_history_and_independent_navigation() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        app.open_document_window(id).unwrap();
        assert_eq!(app.shared_windows.entries.len(), 1, "repeated requests must reuse the second window");
        assert_eq!(app.session.docs().len(), 1, "one editable PDF, not a snapshot copy");
        assert!(app.apply_edit(note(0, "chapter one")));
        let window = &mut app.shared_windows.entries[0];
        {
            let lease = Lease::new(&mut app.session, &mut window.app);
            assert_eq!(lease.app.session.get(id).unwrap().info.annotations.len(), 1);
            lease.app.views[0].go_to_page(2);
            lease.app.views[0].set_zoom(1.7);
            lease.app.quick_tool = QuickTool::Comment(crate::comments::CommentTool::Ink);
            assert!(lease.app.apply_edit(note(2, "chapter three")));
        }
        let doc = app.session.get(id).unwrap();
        assert_eq!(doc.info.annotations.len(), 2);
        assert_eq!(doc.info.annotations[1].contents.as_deref(), Some("chapter three"));
        assert_eq!(app.views[0].current, 0);
        assert_ne!(app.views[0].zoom, 1.7);
        assert_eq!(app.quick_tool, QuickTool::Select);
        app.sync_shared_document_views();
        assert!(app.execute("edit.undo"));
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 1);
        {
            let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[0].app);
            assert!(lease.app.execute("edit.redo"));
            assert_eq!(lease.app.views[0].current, 2);
        }
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 2);
    }

    #[test]
    fn closing_one_view_keeps_dirty_document_and_last_view_prompts() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        assert!(app.apply_edit(note(0, "keep this")));
        app.request_close_tab(0);
        assert!(app.views.is_empty());
        assert!(app.close_request.is_none());
        assert!(app.session.get(id).unwrap().dirty);
        let window = &mut app.shared_windows.entries[0];
        window.app.shared_windows.elsewhere.clear();
        let lease = Lease::new(&mut app.session, &mut window.app);
        assert!(lease.app.apply_edit(note(2, "still editable")));
        lease.app.request_close_tab(0);
        assert_eq!(lease.app.close_request, Some(CloseRequest::Tab(id)));
        lease.app.resolve_close(&egui::Context::default(), None);
        assert_eq!(lease.app.views.len(), 1);
        assert_eq!(lease.app.session.get(id).unwrap().info.annotations.len(), 2);
        lease.app.request_close_tab(0);
        lease.app.resolve_close(&egui::Context::default(), Some(false));
        assert!(lease.app.session.get(id).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn reopening_a_secondary_only_file_reattaches_the_live_document_without_reading_disk() {
        let (mut app, id) = app();
        let bytes = app.session.get(id).unwrap().bytes.clone();
        let path = "missing-shared-textbook.pdf";
        app.session.mark_saved(id, bytes, Some(path.into())).unwrap();
        app.open_document_window(id).unwrap();
        assert!(app.apply_edit(note(0, "unsaved notes must survive reopening")));
        app.request_close_tab(0);
        app.open_path(path);
        assert_eq!(app.views.len(), 1);
        assert_eq!(app.views[0].id, id);
        assert_eq!(app.session.docs().len(), 1);
        assert!(app.session.get(id).unwrap().dirty);
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 1);
    }

    #[test]
    fn second_window_save_as_keeps_both_notes_and_one_save_target() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        assert!(app.apply_edit(note(0, "one")));
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("pdfcraft-shared-{}-{nonce}.pdf", std::process::id()));
        {
            let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[0].app);
            assert!(lease.app.apply_edit(note(2, "three")));
            lease.app.save_override = Some(path.to_string_lossy().into_owned());
            assert!(lease.app.save_active(SaveTarget::As));
        }
        let doc = app.session.get(id).unwrap();
        assert!(!doc.dirty);
        assert_eq!(doc.path.as_deref(), path.to_str());
        let mut reopened = Session::new();
        let saved = reopened.open("saved.pdf", None, Arc::new(std::fs::read(&path).unwrap()), None).unwrap();
        let info = &reopened.get(saved).unwrap().info;
        assert_eq!(info.annotations.iter().map(|a| (a.page, a.contents.as_deref())).collect::<Vec<_>>(), [(0, Some("one")), (2, Some("three"))]);
        assert!(app.execute("edit.undo"), "saving must preserve the shared undo history");
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn secondary_save_shares_recovery_cleanup_and_external_overwrite_protection() {
        let (mut app, id) = app();
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("pdfcraft-shared-save-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("chapters.pdf");
        app.save_override = Some(path.to_string_lossy().into_owned());
        assert!(app.save_active(SaveTarget::As));
        app.save_override = None;
        let store = crate::RecoveryStore::new(dir.join("recovery"));
        app.enable_recovery(store.clone());
        app.open_document_window(id).unwrap();
        assert!(app.apply_edit(note(0, "saved from the other view")));
        app.autosave_now();
        assert_eq!(store.list().len(), 1);
        assert!(app.with_shared_document_view(id, |child| child.save_active(SaveTarget::InPlace)).unwrap());
        assert!(store.list().is_empty(), "saving from either view must retire the same recovery snapshot");
        assert!(!app.watch.disk_differs(id, path.to_str().unwrap()));

        let mut external = std::fs::read(&path).unwrap();
        external.extend_from_slice(b"\n% external change\n");
        std::fs::write(&path, &external).unwrap();
        assert!(app.apply_edit(note(2, "do not overwrite the external version")));
        assert!(!app.with_shared_document_view(id, |child| child.save_active(SaveTarget::InPlace)).unwrap());
        assert_eq!(app.shared_windows.entries[0].app.dialog, Some(crate::Dialog::OverwriteChanged));
        assert_eq!(std::fs::read(&path).unwrap(), external);
        assert!(app.session.get(id).unwrap().dirty);
        drop(app);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn separate_render_consumers_do_not_cancel_or_drain_each_other() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[0].app);
        lease.app.sync_shared_document_views();
        let first = &lease.app.session.get(id).unwrap().renderer;
        let second = &lease.app.shared_windows.renderers.get(&id).unwrap().pool;
        let request = |page| RenderRequest { page, kind: RequestKind::Text, ..Default::default() };
        first.set_queue(vec![request(0)]);
        second.set_queue(vec![request(2)]);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut one = None;
        let mut two = None;
        while std::time::Instant::now() < until && (one.is_none() || two.is_none()) {
            one = one.or_else(|| first.try_recv());
            two = two.or_else(|| second.try_recv());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(one.unwrap().request.page, 0);
        assert_eq!(two.unwrap().request.page, 2);
        assert_ne!(
            lease.app.views[0].page_input_id(),
            crate::DocView::new(id, &lease.app.session.get(id).unwrap().info, Default::default()).page_input_id()
        );
    }

    #[test]
    fn unfinished_comments_survive_close_and_quit_requests() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        app.shared_windows.entries[0].app.views[0].comments.add_box = "not posted yet".into();
        assert!(!app.prepare_shared_quit());
        assert_eq!(app.shared_windows.entries.len(), 1);
        let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[0].app);
        lease.app.request_close_tab(0);
        assert_eq!(lease.app.views[0].comments.add_box, "not posted yet");
        assert_eq!(lease.app.views.len(), 1);
    }

    #[test]
    fn an_unposted_primary_note_blocks_window_close_and_the_final_quit() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        app.views[0].comments.add_box = "keep this primary draft".into();
        assert!(app.hide_primary_for_children(&egui::Context::default()));
        assert!(!app.shared_windows.hidden, "the unfinished editor must stay reachable");
        // The secondary has closed: the final application-quit path still protects the draft.
        app.shared_windows.entries.clear();
        assert!(!app.request_quit(&egui::Context::default()));
        assert_eq!(app.views[0].comments.add_box, "keep this primary draft");
        assert!(!app.allow_quit);
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.viewports.get_mut(&ViewportId::ROOT).unwrap().events.push(egui::ViewportEvent::Close);
        let output = ctx.run_ui(input, |_| app.guard_quit(&ctx));
        assert!(
            output.viewport_output.get(&ViewportId::ROOT).unwrap().commands.iter().any(|command| matches!(command, ViewportCommand::CancelClose))
        );
        output.drop_without_applying_deltas();
    }

    #[test]
    fn quit_transfers_a_document_whose_primary_tab_was_closed() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        assert!(app.apply_edit(note(0, "unsaved")));
        app.request_close_tab(0);
        assert!(app.prepare_shared_quit());
        assert!(app.shared_windows.entries.is_empty());
        assert_eq!(app.views.len(), 1);
        assert_eq!(app.first_dirty(), Some(0));
        assert!(!app.request_quit(&egui::Context::default()));
        assert_eq!(app.close_request, Some(CloseRequest::Quit));
    }

    #[test]
    fn window_control_changes_only_the_requested_reading_view() {
        let (mut app, id) = app();
        app.shared_window_control(&json!({"action":"open", "doc":id.0})).unwrap();
        app.shared_window_control(&json!({"action":"set", "doc":id.0, "key":"page", "value":3})).unwrap();
        assert_eq!(app.shared_windows_state()[0]["page"], 3);
        assert_eq!(app.views[0].current, 0);
        assert!(app.shared_window_control(&json!({"action":"command", "doc":id.0, "id":"create.blank"})).is_err());
        assert!(app.shared_window_control(&json!({"action":"open", "doc":u64::MAX})).is_err());
        assert_eq!(app.session.docs().len(), 1);
    }

    #[test]
    fn render_config_changes_refresh_second_view_without_replacing_pdf_bytes() {
        fn pixels(pool: &RenderPool) -> Vec<u8> {
            pool.set_queue(vec![RenderRequest { page: 0, scale: 1.0, tag: 1, ..Default::default() }]);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < until {
                if let Some(page) = pool.try_recv() {
                    assert!(page.error.is_none(), "{:?}", page.error);
                    return page.rgba.to_vec();
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the small form fixture did not render in time");
        }
        let mut app = PdfCraftApp::new();
        app.open_bytes("form.pdf", None, include_bytes!("../tests/data/form.pdf").to_vec()).unwrap();
        let id = app.views[0].id;
        app.open_document_window(id).unwrap();
        let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[0].app);
        lease.app.sync_shared_document_views();
        let before = pixels(&lease.app.shared_windows.renderers.get(&id).unwrap().pool);
        let display = lease.app.session.get(id).unwrap().display.clone();
        assert!(lease.app.session.set_reading_colors(id, pdfcraft_render::ReadingColors::WHITE_ON_BLACK));
        assert!(Arc::ptr_eq(&display, &lease.app.session.get(id).unwrap().display), "reading colours do not rewrite the PDF");
        lease.app.sync_shared_document_views();
        let after = pixels(&lease.app.shared_windows.renderers.get(&id).unwrap().pool);
        assert!(!before.is_empty());
        assert!(before != after, "the second window must adopt the new rendering configuration, even when PDF bytes stay the same");
    }
}
