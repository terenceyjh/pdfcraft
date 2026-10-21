//! Fixed reading columns and independent native views over one live Session. Only the UI thread mutates documents.
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
    id: u64,
    embedded: bool,
    rect: Option<egui::Rect>,
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
    focused: Option<u64>,
    embedded: bool,
    input_active: bool,
    requests: Vec<(DocId, bool)>,
    pub(crate) area: Option<egui::Rect>,
    widths: Vec<f32>,
    pub(crate) primary_rect: Option<egui::Rect>,
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

/// Background views must not consume shortcuts, text or clipboard events belonging to the
/// selected document window. Keep pointer/wheel events available for egui's layer hit testing.
struct KeyboardLease {
    ctx: egui::Context,
    removed: Vec<(usize, egui::Event)>,
}

impl KeyboardLease {
    fn new(ctx: &egui::Context, enabled: bool) -> Self {
        let mut removed = Vec::new();
        if !enabled {
            ctx.input_mut(|input| {
                let events = std::mem::take(&mut input.events);
                for (index, event) in events.into_iter().enumerate() {
                    if matches!(
                        event,
                        egui::Event::Key { .. }
                            | egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Copy
                            | egui::Event::Cut
                            | egui::Event::Ime(_)
                    ) {
                        removed.push((index, event));
                    } else {
                        input.events.push(event);
                    }
                }
            });
        }
        Self { ctx: ctx.clone(), removed }
    }
}

impl Drop for KeyboardLease {
    fn drop(&mut self) {
        self.ctx.input_mut(|input| {
            for (index, event) in self.removed.drain(..) {
                input.events.insert(index.min(input.events.len()), event);
            }
        });
    }
}

const SPLIT_GAP: f32 = 6.0;
const MIN_COLUMN: f32 = 280.0;

/// Columns share one bounded row. Widths are positive weights owned by the workspace.
fn column_rects(area: egui::Rect, widths: &[f32]) -> Vec<egui::Rect> {
    let count = widths.len();
    if count == 0 || !area.is_positive() {
        return Vec::new();
    }
    let usable = (area.width() - SPLIT_GAP * count.saturating_sub(1) as f32).max(0.0);
    let total: f32 = widths.iter().map(|w| if w.is_finite() && *w > 0.0 { *w } else { 1.0 }).sum();
    let mut left = area.left();
    widths
        .iter()
        .enumerate()
        .map(|(index, weight)| {
            let weight = if weight.is_finite() && *weight > 0.0 { *weight } else { 1.0 };
            let right = if index + 1 == count { area.right() } else { left + usable * weight / total };
            let rect = egui::Rect::from_min_max(egui::pos2(left, area.top()), egui::pos2(right, area.bottom()));
            left = right + SPLIT_GAP;
            rect
        })
        .collect()
}

/// A tab-like header in the normal document flow, without a movable window frame.
fn column_header(ui: &mut egui::Ui, title: &str, selected: bool, closable: bool) -> bool {
    let t = theme::Tokens::get(ui.ctx());
    let mut close = false;
    egui::Frame::NONE.fill(if selected { t.accent_soft } else { t.chrome }).inner_margin(4.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let width = (ui.available_width() - if closable { 32.0 } else { 0.0 }).max(0.0);
            ui.add_sized([width, 24.0], egui::Label::new(egui::RichText::new(title).strong()).truncate()).on_hover_text(title);
            if closable {
                close = ui.button("×").on_hover_text(tl!("Close Tab")).clicked();
            }
        });
    });
    close
}

impl PdfCraftApp {
    pub(crate) fn has_document_panes(&self) -> bool {
        self.shared_windows.entries.iter().any(|w| w.embedded)
    }

    pub(crate) fn arrange_document_panes(&mut self) {
        self.shared_windows.widths.clear();
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }

    /// Expose existing tabs in adjacent columns once; repeated single-file requests do not
    /// accidentally add more columns. Explicit new_pane is how a file is opened repeatedly.
    pub(crate) fn read_tabs_side_by_side(&mut self) {
        let primary = self.active_ids().map(|(_, id)| id);
        let docs: Vec<_> = self.views.iter().map(|v| v.id).filter(|id| Some(*id) != primary).collect();
        for id in docs {
            if !self.shared_windows.entries.iter().any(|w| w.embedded && w.app.views.iter().any(|v| v.id == id))
                && let Err(error) = self.open_document_pane(id)
            {
                self.notify_error(error);
            }
        }
        if !self.has_document_panes()
            && let Some(id) = primary
            && let Err(error) = self.open_document_pane(id)
        {
            self.notify_error(error);
        }
        self.arrange_document_panes();
    }

    pub(crate) fn primary_keyboard_active(&self) -> bool {
        self.shared_windows.focused.is_none() || self.modal_open() || self.palette_open || self.full_screen
    }

    pub(crate) fn update_workspace_focus(&mut self, ctx: &egui::Context) {
        if self.shared_windows.focused.is_some_and(|id| !self.shared_windows.entries.iter().any(|w| w.id == id && !w.app.views.is_empty())) {
            self.shared_windows.focused = None;
        }
        if let Some(window) = self.shared_windows.entries.iter().find(|w| w.embedded && w.app.modal_open()) {
            self.shared_windows.focused = Some(window.id);
            return;
        }
        if let Some(pos) = ctx.input(|i| i.pointer.press_origin().filter(|_| i.pointer.any_pressed())) {
            // Popups/dialogs keep their owning column. All column bodies otherwise live on
            // the parent's normal layer, so focus follows their clipped content rectangles.
            if ctx.layer_id_at(pos).is_some_and(|layer| layer.order >= egui::Order::Foreground) {
                return;
            }
            self.shared_windows.focused = self
                .shared_windows
                .entries
                .iter()
                .find(|w| w.embedded && w.rect.is_some_and(|r| r.contains(pos)) && self.shared_windows.area.is_some_and(|area| area.contains(pos)))
                .map(|w| w.id);
        }
    }

    pub(crate) fn show_document_workspace(&mut self, ui: &mut egui::Ui) -> bool {
        if !self.has_document_panes() {
            self.shared_windows.area = None;
            self.shared_windows.primary_rect = None;
            return false;
        }
        let count = usize::from(self.active.is_some()) + self.shared_windows.entries.iter().filter(|w| w.embedded && !w.app.views.is_empty()).count();
        if count == 0 {
            return false;
        }
        let ctx = ui.ctx().clone();
        let available = ui.available_size();
        let width = available.x.max(count as f32 * MIN_COLUMN + count.saturating_sub(1) as f32 * SPLIT_GAP);
        if self.shared_windows.widths.len() != count {
            self.shared_windows.widths = vec![1.0; count];
        }
        egui::ScrollArea::horizontal().id_salt("document-split-scroll").auto_shrink([false, false]).show(ui, |ui| {
            let (area, _) = ui.allocate_exact_size(egui::vec2(width, ui.available_height()), egui::Sense::hover());
            self.shared_windows.area = Some(area.intersect(ui.clip_rect()));
            let rects = column_rects(area, &self.shared_windows.widths);
            let t = theme::Tokens::get(&ctx);
            for (index, pair) in rects.windows(2).enumerate() {
                let separator = egui::Rect::from_min_max(pair[0].right_top(), pair[1].left_bottom());
                let response = ui
                    .interact(separator, ui.id().with(("column-separator", index)), egui::Sense::drag())
                    .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
                ui.painter().rect_filled(separator, 0.0, if response.hovered() || response.dragged() { t.accent } else { t.divider });
                if response.dragged() {
                    let left = pair[0].width();
                    let right = pair[1].width();
                    let minimum = 100.0_f32.min((left + right) * 0.25);
                    let delta = ctx.input(|i| i.pointer.delta().x).clamp(minimum - left, right - minimum);
                    let total_width = (area.width() - count.saturating_sub(1) as f32 * SPLIT_GAP).max(1.0);
                    let total_weight: f32 = self.shared_windows.widths.iter().sum();
                    if let Some(weight) = self.shared_windows.widths.get_mut(index) {
                        *weight += delta * total_weight / total_width;
                    }
                    if let Some(weight) = self.shared_windows.widths.get_mut(index + 1) {
                        *weight -= delta * total_weight / total_width;
                    }
                    ctx.request_repaint();
                }
            }
            // Keep the unclipped row for sibling geometry; focus separately uses the viewport.
            self.shared_windows.primary_rect = self.active.and_then(|_| rects.first().copied());
            if let Some(index) = self.active
                && let Some(rect) = rects.first().copied()
            {
                let title = self.views.get(index).and_then(|v| self.session.get(v.id)).map(|d| d.name.clone()).unwrap_or_default();
                let mut child = ui.new_child(egui::UiBuilder::new().id_salt("primary-document-column").max_rect(rect));
                child.set_clip_rect(rect.intersect(ui.clip_rect()));
                let _keyboard = KeyboardLease::new(&ctx, self.primary_keyboard_active());
                column_header(&mut child, &title, self.shared_windows.focused.is_none(), false);
                self.column_navigation(&mut child);
                match crate::compare_ui::side_by_side(self, index) {
                    Some((old, new)) => crate::compare_ui::compare_area(self, old, new, &mut child),
                    None => canvas::document_area(self, index, &mut child),
                }
            }
            self.show_shared_views(&ctx, Some((ui, rects)));
        });
        true
    }

    fn column_navigation(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.active else { return };
        let Some(view) = self.views.get_mut(index) else { return };
        ui.horizontal(|ui| {
            if ui.small_button("◀").clicked() {
                view.step_page(false);
            }
            let pages = self.session.get(view.id).map_or(0, |d| d.info.pages.len());
            let mut page = view.current.saturating_add(1);
            if ui.add(egui::DragValue::new(&mut page).range(1..=pages.max(1))).changed() {
                view.go_to_page(page.saturating_sub(1));
            }
            ui.label(format!("/ {pages}"));
            if ui.small_button("▶").clicked() {
                view.step_page(true);
            }
            if ui.small_button("−").clicked() {
                view.zoom_step(false);
            }
            if ui.small_button("+").clicked() {
                view.zoom_step(true);
            }
            ui.menu_button(tl!("Zoom"), |ui| {
                if ui.button(tl!("Fit to width")).clicked() {
                    view.fit = canvas::Fit::Width;
                    ui.close();
                }
                if ui.button(tl!("Fit one full page")).clicked() {
                    view.fit = canvas::Fit::Page;
                    ui.close();
                }
            });
        });
    }

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

    /// Add an independent native view over the same editable document.
    pub fn open_document_window(&mut self, id: DocId) -> Result<(), String> {
        self.open_document_view(id, false)
    }

    /// Add a fixed reading column inside the application workspace.
    pub fn open_document_pane(&mut self, id: DocId) -> Result<(), String> {
        self.open_document_view(id, true)
    }

    pub(crate) fn open_document_view(&mut self, id: DocId, embedded: bool) -> Result<(), String> {
        if !embedded && cfg!(target_arch = "wasm32") {
            return Err("Document windows are available in the desktop app".into());
        }
        if self.shared_windows.secondary {
            self.shared_windows.requests.push((id, embedded));
            return Ok(());
        }
        let source = self
            .views
            .iter()
            .chain(self.shared_windows.entries.iter().flat_map(|w| &w.app.views))
            .find(|v| v.id == id)
            .ok_or("The document has no open view")?;
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
        if embedded {
            view.fit = canvas::Fit::Width;
            if !self.has_document_panes()
                && let Some(primary) = self.active.and_then(|index| self.views.get_mut(index))
            {
                primary.fit = canvas::Fit::Width;
            }
        }
        app.views.push(view);
        app.shared_windows.secondary = true;
        app.shared_windows.embedded = embedded;
        app.shared_windows.elsewhere.extend(self.views.iter().map(|v| v.id));
        app.fonts_ready = true;
        app.styled = true;
        self.shared_windows.versions.insert(id, doc.edit_generation());
        self.shared_windows.next = self.shared_windows.next.saturating_add(1);
        let viewport = ViewportId::from_hash_of(("shared-document", self.shared_windows.next));
        if let Some(view) = app.views.first_mut() {
            view.ui_namespace = viewport;
        }
        let view_id = self.shared_windows.next;
        self.shared_windows.entries.push(Window { id: view_id, embedded, rect: None, viewport, app, client: None });
        if embedded {
            self.shared_windows.focused = Some(view_id);
            self.arrange_document_panes();
        }
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
        if !self.shared_windows.entries.iter().any(|w| !w.embedded) || self.shared_windows.secondary {
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

    /// Commit typing and make every document reachable by the primary save prompt. Keep the
    /// original windows alive until an exit decision is made, so Cancel preserves their state.
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
        self.shared_windows.entries = windows;
        for window in &self.shared_windows.entries {
            for source in &window.app.views {
                if !self.views.iter().any(|v| v.id == source.id)
                    && let Some(doc) = self.session.get(source.id)
                {
                    let mut view = DocView::new(source.id, &doc.info, self.view_defaults);
                    view.zoom = source.zoom;
                    view.fit = source.fit;
                    view.layout = source.layout;
                    view.continuous = source.continuous;
                    view.rotation = source.rotation;
                    view.cover = source.cover;
                    view.go_to_page(source.current);
                    self.views.push(view);
                }
            }
        }
        self.shared_windows.hidden = false;
        true
    }

    pub(crate) fn show_shared_windows(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.show_shared_views(ctx, None);
    }

    fn show_shared_views(&mut self, ctx: &egui::Context, mut panes: Option<(&mut egui::Ui, Vec<egui::Rect>)>) {
        // Own edits already refreshed their local view. Only a later change from a sibling
        // should invalidate it on the next pass.
        // Embedded columns run before the later native-window pass. Do not acknowledge a
        // column's edit on behalf of the primary renderer before it has adopted that edit.
        if panes.is_some() || !self.has_document_panes() || self.full_screen {
            for view in &self.views {
                if self.shared_windows.versions.contains_key(&view.id)
                    && let Some(doc) = self.session.get(view.id)
                {
                    self.shared_windows.versions.insert(view.id, doc.edit_generation());
                }
            }
        }
        let mut windows = std::mem::take(&mut self.shared_windows.entries);
        let mut pane_index = usize::from(self.active.is_some());
        let root_modal = self.modal_open() || self.palette_open;
        let mut requests = Vec::new();
        for index in 0..windows.len() {
            let elsewhere = self
                .views
                .iter()
                .map(|v| v.id)
                .chain(windows.iter().enumerate().filter(|(i, _)| *i != index).flat_map(|(_, w)| w.app.views.iter().map(|v| v.id)))
                .collect();
            let Some(window) = windows.get_mut(index) else { continue };
            if window.embedded != panes.is_some() {
                continue;
            }
            window.app.shared_windows.elsewhere = elsewhere;
            window.app.language = self.language.clone();
            window.app.theme = self.theme;
            window.app.shared_windows.input_active = !window.embedded || (self.shared_windows.focused == Some(window.id) && !root_modal);
            let title = window
                .app
                .views
                .first()
                .and_then(|v| self.session.get(v.id))
                .map_or_else(|| "PdfCraft".to_string(), |doc| format!("{} — #{}", doc.name, window.id));
            {
                let lease = Lease::new(&mut self.session, &mut window.app).with_recovery(&mut self.recovery, &mut self.recovery_keys);
                #[cfg(not(target_arch = "wasm32"))]
                let lease = lease.with_watch(&mut self.watch);
                if window.embedded {
                    if let Some((parent, rects)) = panes.as_mut()
                        && let Some(rect) = rects.get(pane_index).copied()
                    {
                        window.rect = Some(rect);
                        let mut child = parent.new_child(egui::UiBuilder::new().id_salt(("document-column", window.id)).max_rect(rect));
                        child.set_clip_rect(rect.intersect(parent.clip_rect()));
                        if root_modal {
                            child.disable();
                        }
                        let _keyboard = KeyboardLease::new(ctx, lease.app.shared_windows.input_active);
                        if column_header(&mut child, &title, self.shared_windows.focused == Some(window.id), true) {
                            lease.app.request_close_tab(0);
                            self.shared_windows.focused = Some(window.id);
                        }
                        lease.app.shared_window_ui(&mut child);
                    }
                    pane_index += 1;
                } else {
                    ctx.show_viewport_immediate(
                        window.viewport,
                        ViewportBuilder::default().with_title(title).with_inner_size([1100.0, 850.0]).with_min_inner_size([640.0, 420.0]),
                        |ui, _class| lease.app.shared_window_ui(ui),
                    );
                }
            }
            requests.append(&mut window.app.shared_windows.requests);
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
        for (id, embedded) in requests {
            if let Err(error) = self.open_document_view(id, embedded) {
                self.notify_error(error);
            }
        }
        if self.shared_windows.hidden && !self.shared_windows.entries.iter().any(|w| !w.embedded) {
            self.shared_windows.hidden = false;
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
            if self.shared_windows.entries.is_empty() {
                self.request_quit(ctx);
            }
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
        if !self.shared_windows.embedded || self.shared_windows.input_active {
            self.shortcuts(&ctx);
        }
        if self.process_pending_edits() {
            for (command, doc) in deferred {
                if self.active_ids().map(|(_, id)| id) == doc {
                    self.execute(command);
                }
            }
        }
        if !self.shared_windows.embedded && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.request_close_tab(0);
        }
        if self.views.is_empty() {
            return;
        }
        for view in &mut self.views {
            view.begin_render_frame();
        }
        let viewport = self.views[0].ui_namespace;
        egui::Panel::top(egui::Id::new(("shared-toolbar", viewport))).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.menu_button(tl!("Menu"), |ui| {
                    for (label, command) in [
                        ("Save", "file.save"),
                        ("Save as…", "file.save_as"),
                        ("Undo", "edit.undo"),
                        ("Redo", "edit.redo"),
                        ("Find…", "edit.find"),
                        ("Open in Right Split", "view.new_pane"),
                    ] {
                        let enabled = pdfcraft_engine::commands::command(command).is_some_and(|spec| self.command_enabled(spec));
                        if ui.add_enabled(enabled, egui::Button::new(tl!(label))).clicked() {
                            self.execute(command);
                            ui.close();
                        }
                    }
                    if !self.shared_windows.embedded
                        && ui.button(tl!("Show primary window")).clicked()
                        && let Some((_, id)) = self.active_ids()
                    {
                        self.shared_windows.restore_primary = Some(id);
                        ui.close();
                    }
                });
                for (label, panel) in
                    [("Comments", RightPanel::Comments), ("Bookmarks", RightPanel::Bookmarks), ("Page thumbnails", RightPanel::Pages)]
                {
                    if ui.selectable_label(self.right == Some(panel), tl!(label)).clicked() {
                        self.right = if self.right == Some(panel) { None } else { Some(panel) };
                    }
                }
            });
            self.column_navigation(ui);
        });
        if self.right.is_some() && self.shared_windows.embedded && ui.available_width() < 560.0 {
            let height = (ui.available_height() * 0.4).clamp(90.0, 300.0);
            egui::Panel::bottom(egui::Id::new(("column-inspector", viewport)))
                .resizable(true)
                .default_size(height)
                .min_size(80.0)
                .max_size(height.max(80.0))
                .show(ui, |ui| panels::right_panel(self, ui));
        } else if self.right.is_some() {
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
        if !self.shared_windows.embedded || self.shared_windows.input_active {
            dialogs::show(self, &ctx);
            widgets::toast(self, &ctx);
        }
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
                        "view": window.id, "kind": if window.embedded { "embedded" } else { "native" },
                        "focused": self.shared_windows.focused == Some(window.id),
                        "rect": window.rect.map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]),
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
        view_id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> Result<std::sync::mpsc::Receiver<crate::control::Reply>, String> {
        if method == "ui.screenshot" {
            return Err(
                "The immediate-window renderer does not support secondary screenshots; capture the primary window or use native window capture"
                    .into(),
            );
        }
        let index = self.shared_windows.entries.iter().position(|w| w.id == view_id).ok_or("No document window has that view id")?;
        let elsewhere = self
            .views
            .iter()
            .map(|v| v.id)
            .chain(self.shared_windows.entries.iter().enumerate().filter(|(i, _)| *i != index).flat_map(|(_, w)| w.app.views.iter().map(|v| v.id)))
            .collect();
        let window = self.shared_windows.entries.get_mut(index).ok_or("The window is closed")?;
        window.app.shared_windows.elsewhere = elsewhere;
        if window.embedded {
            use crate::control::Host;
            let (tx, rx) = std::sync::mpsc::channel();
            let lease = Lease::new(&mut self.session, &mut window.app).with_recovery(&mut self.recovery, &mut self.recovery_keys);
            #[cfg(not(target_arch = "wasm32"))]
            let lease = lease.with_watch(&mut self.watch);
            let reply = match method {
                "ui.state" => Ok(lease.app.state()),
                "ui.set" => {
                    self.shared_windows.focused = Some(view_id);
                    let key = params.get("key").and_then(serde_json::Value::as_str).ok_or("set needs key")?;
                    let value = params.get("value").ok_or("set needs value")?;
                    lease.app.set(key, &value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()))
                }
                "ui.command" => {
                    self.shared_windows.focused = Some(view_id);
                    lease.app.command(params.get("id").and_then(serde_json::Value::as_str).ok_or("command needs id")?)
                }
                _ => Err(
                    "Internal windows share the main widget tree: use untargeted ui.inspect/input/screenshot, or window-targeted state/set/command"
                        .into(),
                ),
            };
            let _ = tx.send(reply);
            return Ok(rx);
        }
        if window.client.is_none() {
            let ctx = self.ctx.as_ref().ok_or("The interface has not drawn yet")?;
            let (control, client) = crate::control::attach_for(ctx, window.viewport);
            window.app.control = Some(control);
            window.client = Some(client);
        }
        window.client.as_ref().map(|client| client.send(method, params)).ok_or_else(|| "The window control is unavailable".into())
    }

    /// Opt-in UI control surface. A view id disambiguates multiple views of the same PDF.
    pub(crate) fn shared_window_control(&mut self, params: &serde_json::Value) -> crate::control::Reply {
        use crate::control::Host;
        let action = params.get("action").and_then(serde_json::Value::as_str).unwrap_or("state");
        if action == "state" {
            return Ok(self.shared_windows_state());
        }
        if action == "show-primary" {
            self.shared_windows.hidden = false;
            if let Some(ctx) = &self.ctx {
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
            }
            return Ok(serde_json::json!({"primary_hidden":false}));
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
            .or_else(|| {
                params
                    .get("view")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|target| self.shared_windows.entries.iter().find(|w| w.id == target))
                    .and_then(|w| w.app.views.first())
                    .map(|v| v.id)
            })
            .or_else(|| self.active_ids().map(|(_, id)| id))
            .or_else(|| self.shared_windows.entries.iter().flat_map(|w| &w.app.views).next().map(|v| v.id))
            .ok_or("No document is open")?;
        if action == "open" {
            let embedded = match params.get("kind").and_then(serde_json::Value::as_str).unwrap_or("native") {
                "embedded" => true,
                "native" => false,
                _ => return Err("kind must be embedded or native".into()),
            };
            self.open_document_view(id, embedded)?;
            return Ok(self.shared_windows_state());
        }
        let target = params.get("view").and_then(serde_json::Value::as_u64);
        let index = self
            .shared_windows
            .entries
            .iter()
            .position(|w| target.map_or_else(|| w.app.views.iter().any(|v| v.id == id), |view| w.id == view))
            .ok_or("No document window matches the requested view")?;
        let elsewhere = self
            .views
            .iter()
            .map(|v| v.id)
            .chain(self.shared_windows.entries.iter().enumerate().filter(|(i, _)| *i != index).flat_map(|(_, w)| w.app.views.iter().map(|v| v.id)))
            .collect();
        let window = self.shared_windows.entries.get_mut(index).ok_or("The window is closed")?;
        if window.embedded {
            self.shared_windows.focused = Some(window.id);
        }
        window.app.shared_windows.elsewhere = elsewhere;
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
            "focus" => {
                if let Some(ctx) = &lease.app.ctx
                    && !window.embedded
                {
                    ctx.send_viewport_cmd_to(window.viewport, ViewportCommand::Focus);
                }
                Ok(serde_json::json!({"view":window.id}))
            }
            _ => Err("ui.window action must be state, open, focus, set, command, close, show-primary or hide-primary".into()),
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
        assert_eq!(app.shared_windows.entries.len(), 2, "each request creates an independent reading view");
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
    fn native_pass_does_not_acknowledge_an_embedded_edit_before_primary_refresh() {
        let (mut app, id) = app();
        app.open_document_pane(id).unwrap();
        app.sync_shared_document_views();
        let before = app.shared_windows.versions[&id];
        {
            let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[0].app);
            assert!(lease.app.apply_edit(note(1, "refresh all reading columns")));
        }
        let generation = app.session.get(id).unwrap().edit_generation();
        assert_ne!(before, generation);
        app.show_shared_views(&egui::Context::default(), None);
        assert_eq!(app.shared_windows.versions[&id], before, "the primary has not rendered the embedded edit yet");
        app.sync_shared_document_views();
        assert_eq!(app.shared_windows.versions[&id], generation);
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
    fn quit_includes_a_document_whose_primary_tab_was_closed_and_cancel_keeps_its_windows() {
        let (mut app, id) = app();
        app.open_document_window(id).unwrap();
        assert!(app.apply_edit(note(0, "unsaved")));
        app.request_close_tab(0);
        assert!(app.prepare_shared_quit());
        assert_eq!(app.shared_windows.entries.len(), 1);
        assert_eq!(app.views.len(), 1);
        assert_eq!(app.first_dirty(), Some(0));
        assert!(!app.request_quit(&egui::Context::default()));
        assert_eq!(app.close_request, Some(CloseRequest::Quit));
        app.resolve_close(&egui::Context::default(), None);
        assert_eq!(app.shared_windows.entries.len(), 1);
        assert!(app.session.get(id).unwrap().dirty);
    }

    #[test]
    fn cancelled_quit_preserves_all_internal_reading_positions() {
        let (mut app, id) = app();
        for page in [1, 2] {
            app.open_document_pane(id).unwrap();
            app.shared_windows.entries.last_mut().unwrap().app.views[0].go_to_page(page);
        }
        assert!(app.apply_edit(note(0, "save prompt")));
        let ctx = egui::Context::default();
        assert!(!app.request_quit(&ctx));
        assert_eq!(app.close_request, Some(CloseRequest::Quit));
        app.resolve_close(&ctx, None);
        assert_eq!(app.shared_windows.entries.len(), 2);
        assert_eq!(app.shared_windows_state()[0]["page"], 2);
        assert_eq!(app.shared_windows_state()[1]["page"], 3);
        assert!(app.session.get(id).unwrap().dirty);
    }

    #[test]
    fn native_close_protects_an_internal_only_document_and_its_unposted_note() {
        let (mut app, id) = app();
        app.open_document_pane(id).unwrap();
        app.request_close_tab(0);
        assert!(app.views.is_empty());
        app.shared_windows.entries[0].app.views[0].comments.add_box = "internal draft".into();
        let ctx = egui::Context::default();
        let close = || {
            let mut input = egui::RawInput::default();
            input.viewports.get_mut(&ViewportId::ROOT).unwrap().events.push(egui::ViewportEvent::Close);
            input
        };
        let output = ctx.run_ui(close(), |_| app.guard_quit(&ctx));
        assert!(output.viewport_output[&ViewportId::ROOT].commands.iter().any(|c| matches!(c, ViewportCommand::CancelClose)));
        output.drop_without_applying_deltas();
        assert!(!app.allow_quit);
        assert_eq!(app.shared_windows.entries[0].app.views[0].comments.add_box, "internal draft");
        app.shared_windows.entries[0].app.views[0].comments.add_box.clear();
        let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[0].app);
        assert!(lease.app.apply_edit(note(0, "dirty internal document")));
        drop(lease);
        let output = ctx.run_ui(close(), |_| app.guard_quit(&ctx));
        output.drop_without_applying_deltas();
        assert_eq!(app.close_request, Some(CloseRequest::Quit));
        app.resolve_close(&ctx, None);
        assert_eq!(app.shared_windows.entries.len(), 1);
        assert!(app.session.get(id).unwrap().dirty);
    }

    #[test]
    fn consecutive_targeted_close_commands_do_not_use_stale_sibling_state() {
        let (mut app, id) = app();
        app.open_document_pane(id).unwrap();
        app.open_document_pane(id).unwrap();
        assert!(app.apply_edit(note(0, "do not discard")));
        app.request_close_tab(0);
        for view in [1, 2] {
            let reply = app.route_shared_window(view, "ui.command", json!({"id":"file.close"})).unwrap();
            assert!(reply.recv().unwrap().is_ok());
        }
        assert!(app.shared_windows.entries[0].app.views.is_empty());
        assert_eq!(app.shared_windows.entries[1].app.views.len(), 1);
        assert_eq!(app.shared_windows.entries[1].app.close_request, Some(CloseRequest::Tab(id)));
        assert!(app.session.get(id).unwrap().dirty);
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

    #[test]
    fn new_installation_defaults_to_chinese_and_remembers_explicit_language_choices() {
        let app = PdfCraftApp::new();
        assert_eq!(app.language, "zh-hans");
        let mut restored = PdfCraftApp::new();
        restored.restore(&app.persist());
        assert_eq!(restored.language, "zh-hans");
        restored.set_option("language", "en").unwrap();
        let mut again = PdfCraftApp::new();
        again.restore(&restored.persist());
        assert_eq!(again.language, "en");
    }

    #[test]
    fn three_sibling_views_share_edits_and_protect_the_last_view_after_primary_closes() {
        let (mut app, id) = app();
        for _ in 0..3 {
            app.open_document_pane(id).unwrap();
        }
        for index in 0..3 {
            let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[index].app);
            lease.app.views[0].go_to_page(index);
            assert!(lease.app.apply_edit(note(index, &format!("note {index}"))));
        }
        assert_eq!(app.session.docs().len(), 1);
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 3);
        assert_eq!(app.shared_windows_state().as_array().unwrap().iter().map(|v| v["page"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2, 3]);
        app.request_close_tab(0);
        for view in [1, 2] {
            let close = app.shared_window_control(&json!({"action":"close","view":view})).unwrap();
            assert_eq!(close["closed"], true);
            assert!(app.session.get(id).is_some());
        }
        let last = app.shared_window_control(&json!({"action":"close","view":3})).unwrap();
        assert_eq!(last["closed"], false);
        assert_eq!(last["close_prompt"], true);
        assert_eq!(app.session.get(id).unwrap().info.annotations.len(), 3);
    }

    #[test]
    fn mixed_document_panes_route_by_view_id_and_keep_drafts_independent() {
        let (mut app, first) = app();
        app.open_bytes("other.pdf", None, app.session.get(first).unwrap().bytes.as_ref().clone()).unwrap();
        let second = app.views[1].id;
        app.open_document_pane(first).unwrap();
        app.open_document_pane(first).unwrap();
        app.open_document_pane(second).unwrap();
        app.shared_windows.entries[0].app.views[0].comments.add_box = "unposted".into();
        assert_eq!(app.shared_window_control(&json!({"action":"close","view":1})).unwrap()["closed"], false);
        assert_eq!(app.shared_window_control(&json!({"action":"close","view":2})).unwrap()["closed"], true);
        let lease = Lease::new(&mut app.session, &mut app.shared_windows.entries[2].app);
        assert!(lease.app.apply_edit(note(0, "second document only")));
        drop(lease);
        assert_eq!(app.session.get(first).unwrap().info.annotations.len(), 0);
        assert_eq!(app.session.get(second).unwrap().info.annotations.len(), 1);
        assert!(!app.prepare_shared_quit());
        assert_eq!(app.shared_windows.entries[0].app.views[0].comments.add_box, "unposted");
    }

    #[test]
    fn fixed_columns_are_disjoint_and_within_the_workspace() {
        let area = egui::Rect::from_min_max(egui::pos2(20.0, 90.0), egui::pos2(1420.0, 950.0));
        for count in 2..=16 {
            let rects: Vec<_> = (0..count).map(|index| column_rects(area, &vec![1.0; count])[index]).collect();
            for (i, rect) in rects.iter().enumerate() {
                assert!(area.contains_rect(*rect));
                assert!(rect.is_positive());
                assert!(!rects.iter().skip(i + 1).any(|r| rect.intersects(*r)));
            }
        }
    }
}
