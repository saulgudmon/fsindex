use crate::{
    table::{count_label, Action, Table},
    theme,
    worker::{SearchWorker, Update},
};
use eframe::egui::{self, Color32, RichText};
use fsindex_core::{query::ResultSnapshot, Engine, EntryKind, SearchQuery, StatusResponse};
use std::{
    path::Path,
    sync::{mpsc, Arc},
    time::Duration,
};

pub struct Desktop {
    worker: SearchWorker,
    query: SearchQuery,
    text: String,
    revision: u64,
    rows_revision: Option<u64>,
    rows: Option<ResultSnapshot>,
    pending: Option<ResultSnapshot>,
    status: Option<StatusResponse>,
    error: Option<String>,
    selected: Option<usize>,
    // Once the user browses results, live updates wait for explicit application.
    browsing: bool,
    scroll_epoch: u64,
    searching: bool,
    config_path: String,
    show_details: bool,
    show_about: bool,
    table: Table,
    action_tx: mpsc::Sender<String>,
    action_rx: mpsc::Receiver<String>,
}

impl Desktop {
    pub fn new(cc: &eframe::CreationContext<'_>, engine: Arc<Engine>) -> Self {
        Self::with_engine(&cc.egui_ctx, engine)
    }

    fn with_engine(context: &egui::Context, engine: Arc<Engine>) -> Self {
        let ctx = context.clone();
        theme::apply(&ctx);
        let config_path = engine.config_path().display().to_string();
        let worker = SearchWorker::new(engine, move || ctx.request_repaint());
        let (action_tx, action_rx) = mpsc::channel();
        Self {
            worker,
            query: SearchQuery::default(),
            text: String::new(),
            revision: 1,
            rows_revision: None,
            rows: None,
            pending: None,
            status: None,
            error: None,
            selected: None,
            browsing: false,
            scroll_epoch: 0,
            searching: true,
            config_path,
            show_details: false,
            show_about: false,
            table: Table::default(),
            action_tx,
            action_rx,
        }
    }

    fn search(&mut self) {
        self.query.name = (!self.text.is_empty()).then(|| self.text.clone());
        self.revision = self.worker.submit(self.query.clone());
        self.searching = true;
        self.error = None;
        self.worker.retire(self.pending.take());
        self.selected = None;
        self.browsing = false;
        if let Some(rows) = self.worker.cached(&self.query) {
            self.worker.retire(self.rows.replace(rows));
            self.rows_revision = Some(self.revision);
            self.scroll_epoch += 1;
        }
        // Keep the previous display until the new revision is ready. Its rows
        // are visibly retained but cannot be acted on as current search results.
    }

    fn collect(&mut self) {
        if let Some(update) = self.worker.take() {
            self.accept_update(update);
        }
        for error in self.action_rx.try_iter() {
            self.error = Some(error);
        }
    }

    fn accept_update(&mut self, update: Update) {
        self.status = Some(update.status);
        if update.revision == self.revision {
            if let Some(result) = update.result {
                let new_query = self.searching;
                self.searching = false;
                match result {
                    Ok(rows) => {
                        self.error = None;
                        if !new_query && self.browsing && self.rows.is_some() {
                            self.worker.retire(self.pending.replace(rows));
                        } else {
                            self.worker.retire(self.rows.replace(rows));
                            self.rows_revision = Some(self.revision);
                            if new_query {
                                self.selected = None;
                                self.browsing = false;
                                self.scroll_epoch += 1;
                            }
                        }
                    }
                    Err(error) => {
                        self.error = Some(error);
                        // Keep the previous display while the expression is corrected.
                    }
                }
            }
        }
    }

    fn open(&self, path: String) {
        let errors = self.action_tx.clone();
        std::thread::spawn(move || {
            let result = std::process::Command::new("xdg-open").arg(&path).status();
            let error = match result {
                Ok(status) if status.success() => None,
                Ok(status) => Some(format!("Could not open {path}: {status}")),
                Err(error) => Some(format!("Could not open {path}: {error}")),
            };
            if let Some(error) = error {
                let _ = errors.send(error);
            }
        });
    }
}

impl Desktop {
    fn action(&self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Open(path) => self.open(path),
            Action::OpenFolder(path) => {
                if let Some(parent) = Path::new(&path).parent() {
                    self.open(parent.display().to_string());
                }
            }
            Action::Copy(path) => ctx.copy_text(path),
        }
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().interact_size.y = 32.0;
            ui.menu_button("Menu", |ui| {
                if ui.button("Index details").clicked() {
                    self.show_details = !self.show_details;
                    ui.close();
                }
                ui.separator();
                if ui.button("About fsindex").clicked() {
                    self.show_about = true;
                    ui.close();
                }
            });
            let search_width = (ui.available_width() - 128.0).max(180.0);
            egui::Frame::new()
                .fill(theme::BACKGROUND)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(8, 2))
                .show(ui, |ui| {
                    ui.set_width(search_width - 16.0);
                    ui.horizontal(|ui| {
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(16.0, 28.0), egui::Sense::hover());
                        let center = r.center() - egui::vec2(2.0, 2.0);
                        ui.painter().circle_stroke(
                            center,
                            4.5,
                            egui::Stroke::new(1.6, theme::MUTED),
                        );
                        ui.painter().line_segment(
                            [center + egui::vec2(3.5, 3.5), center + egui::vec2(7.0, 7.0)],
                            egui::Stroke::new(1.6, theme::MUTED),
                        );
                        let input = ui.add_sized(
                            [search_width - 84.0, 28.0],
                            egui::TextEdit::singleline(&mut self.text)
                                .id(egui::Id::new("search-input"))
                                .frame(egui::Frame::NONE)
                                .vertical_align(egui::Align::Center)
                                .hint_text("Search filenames…"),
                        );
                        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
                            input.request_focus();
                        }
                        if input.changed() {
                            changed = true;
                        }
                        let clear = ui
                            .add_enabled(
                                !self.text.is_empty(),
                                egui::Button::new("×")
                                    .frame(false)
                                    .min_size(egui::vec2(22.0, 26.0)),
                            )
                            .on_hover_text("Clear search");
                        if clear.clicked()
                            || (input.has_focus()
                                && ctx.input(|i| i.key_pressed(egui::Key::Escape)))
                        {
                            self.text.clear();
                            changed = true;
                            input.request_focus();
                        }
                    });
                });
            egui::ComboBox::from_id_salt("kind")
                .width(104.0)
                .selected_text(match self.query.kind {
                    Some(EntryKind::File) => "Files",
                    Some(EntryKind::Dir) => "Folders",
                    Some(EntryKind::Symlink) => "Links",
                    _ => "All",
                })
                .show_ui(ui, |ui| {
                    for (label, kind) in [
                        ("All", None),
                        ("Files", Some(EntryKind::File)),
                        ("Folders", Some(EntryKind::Dir)),
                        ("Links", Some(EntryKind::Symlink)),
                    ] {
                        changed |= ui
                            .selectable_value(&mut self.query.kind, kind, label)
                            .changed();
                    }
                });
        });
        if changed {
            self.search();
        }
    }
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.spacing_mut().button_padding = egui::vec2(5.0, 2.0);
            changed |= ui
                .toggle_value(&mut self.query.case_sensitive, "Aa")
                .on_hover_text("Match case")
                .changed();
            changed |= ui
                .toggle_value(&mut self.query.regex, ".*")
                .on_hover_text("Regular expression")
                .changed();
            changed |= ui
                .toggle_value(&mut self.query.search_path, "Path")
                .on_hover_text("Search the full path")
                .changed();
            ui.separator();
            ui.label(
                RichText::new(format!(
                    "{} {}",
                    count_label(self.rows.as_ref().map_or(0, |r| r.rows.len())),
                    if self.rows.is_some() && self.rows_revision != Some(self.revision) {
                        "previous items"
                    } else {
                        "items"
                    }
                ))
                .color(theme::MUTED),
            );
            if ui.available_width() > 420.0 {
                if let Some(status) = &self.status {
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{} volumes", status.volumes.len()))
                            .color(theme::MUTED),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "{} indexed",
                            count_label(status.totals.entries as usize)
                        ))
                        .color(theme::MUTED),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.pending.is_some() {
                    if ui
                        .button("Apply updates")
                        .on_hover_text("Files changed. Refresh results and return to the top.")
                        .clicked()
                    {
                        self.worker.retire(self.rows.take());
                        self.rows = self.pending.take();
                        self.selected = None;
                        self.browsing = false;
                        self.scroll_epoch += 1;
                    }
                } else if self
                    .rows
                    .as_ref()
                    .is_some_and(|rows| self.worker.refreshing(rows.generation))
                {
                    ui.label(RichText::new("Updating index…").color(theme::MUTED));
                } else if self.searching {
                    ui.label(RichText::new("Searching…").color(theme::MUTED));
                    ui.spinner();
                } else if self.rows_revision != Some(self.revision) && self.error.is_some() {
                    ui.label(RichText::new("Search error").color(Color32::LIGHT_RED));
                } else {
                    let (label, color, note) = match self.status.as_ref() {
                        Some(status) if status.scan_state.scanning => (
                            "Scanning…",
                            Color32::from_rgb(227, 177, 86),
                            "Initial results may be incomplete",
                        ),
                        Some(status) if status.scan_state.complete => (
                            "Index current",
                            theme::ACCENT,
                            "Watching for filesystem changes",
                        ),
                        Some(status) => (
                            "Index incomplete",
                            Color32::from_rgb(227, 177, 86),
                            status
                                .scan_state
                                .note
                                .as_deref()
                                .unwrap_or("Some files may be missing"),
                        ),
                        None => ("Starting…", theme::MUTED, "Starting the index"),
                    };
                    ui.label(RichText::new(label).color(theme::MUTED))
                        .on_hover_text(note);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 16.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, color);
                }
            });
        });
        if changed {
            self.search();
        }
    }
}

impl eframe::App for Desktop {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.collect();
        let ctx = ui.ctx().clone();
        #[cfg(feature = "capture")]
        crate::capture::poll(&ctx, self.rows.is_some());
        ctx.request_repaint_after(Duration::from_millis(250));
        egui::Panel::top("search")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 8)),
            )
            .show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 3)),
            )
            .show(ui, |ui| self.status_bar(ui));
        if self.show_details {
            egui::Panel::right("details")
                .default_size(260.0)
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.heading("Index details");
                        if ui.small_button("×").clicked() {
                            self.show_details = false;
                        }
                    });
                    ui.label(RichText::new("Filenames and metadata only").color(theme::MUTED));
                    ui.add_space(12.0);
                    egui::ScrollArea::vertical()
                        .id_salt("root-details")
                        .show(ui, |ui| {
                            if let Some(status) = &self.status {
                                for root in &status.roots {
                                    ui.label(&root.path);
                                    ui.small(
                                        RichText::new(format!("{:?}", root.state))
                                            .color(theme::MUTED),
                                    );
                                    ui.add_space(8.0);
                                }
                                if let Some(note) = &status.scan_state.note {
                                    ui.label(note);
                                }
                            }
                            ui.separator();
                            ui.label("Configuration");
                            ui.label(&self.config_path);
                            ui.small("Edit this file and restart to change indexed folders.");
                            ui.add_space(12.0);
                            ui.small("Closing the app stops indexing.");
                        });
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BACKGROUND))
            .show(ui, |ui| {
                if let Some(error) = &self.error {
                    ui.horizontal(|ui| {
                        ui.colored_label(Color32::LIGHT_RED, error);
                    });
                }
                self.table.pending_query = self.rows_revision != Some(self.revision);
                let output = self.table.show(
                    ui,
                    self.rows.as_ref(),
                    self.scroll_epoch,
                    &mut self.selected,
                    self.query.sort,
                    self.query.desc,
                );
                if output.scroll.state.offset.y > 0.0 || self.selected.is_some() {
                    self.browsing = true;
                }
                if let Some(action) = output.action {
                    self.action(action, &ctx);
                }
                if let Some(key) = output.sort {
                    if self.query.sort == key {
                        self.query.desc = !self.query.desc;
                    } else {
                        self.query.sort = key;
                        self.query.desc = false;
                    }
                    self.search();
                }
                if !self.searching && self.rows.as_ref().is_some_and(|s| s.rows.is_empty()) {
                    let rect = output.scroll.inner_rect;
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        if self.status.as_ref().is_some_and(|s| s.scan_state.scanning) {
                            "No matches yet. Scanning is still in progress."
                        } else {
                            "No matching files. Try another search or filter."
                        },
                        egui::FontId::proportional(14.0),
                        theme::MUTED,
                    );
                }
            });
        if self.rows_revision == Some(self.revision) && !ctx.egui_wants_keyboard_input() {
            if let Some(hit) = self
                .rows
                .as_ref()
                .and_then(|s| self.selected.and_then(|i| s.rows.get(i)))
            {
                if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.open(hit.path.clone());
                }
                if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) {
                    ctx.copy_text(hit.path.clone());
                }
            }
        }
        egui::Window::new("About fsindex")
            .open(&mut self.show_about)
            .collapsible(false)
            .resizable(false)
            .show(&ctx, |ui| {
                ui.heading("fsindex");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(8.0);
                ui.label("Fast, live filename search for Linux.");
                ui.label("Search names and metadata across your indexed folders.");
                ui.add_space(8.0);
                ui.small("Ctrl+F  Search     Enter  Open selection     Ctrl+C  Copy path");
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsindex_core::{Config, SearchHit};

    fn fixture(name: &str) -> ResultSnapshot {
        ResultSnapshot {
            generation: 1,
            rows: vec![Arc::new(SearchHit {
                name: name.into(),
                path: format!("/fixture/{name}"),
                volume: 1,
                volume_name: "test".into(),
                kind: EntryKind::File,
                size: 1,
                mtime: 0,
                mtime_ns: 0,
                inode: 1,
                depth: 1,
            })]
            .into(),
        }
    }
    fn app() -> (egui::Context, Arc<Engine>, Desktop) {
        let ctx = egui::Context::default();
        let engine = Engine::build(
            Config {
                roots: vec![],
                ..Default::default()
            },
            Default::default(),
        )
        .unwrap();
        let mut app = Desktop::with_engine(&ctx, Arc::clone(&engine));
        app.accept_update(Update {
            revision: 1,
            status: engine.status(),
            result: Some(Ok(fixture("previous.txt"))),
        });
        (ctx, engine, app)
    }

    #[test]
    fn clearing_zero_results_restores_baseline_in_same_frame() {
        use std::time::{Duration, Instant};
        let (_, engine, mut app) = app();
        {
            let mut state = engine.shared().write();
            let volume = state.index.ensure_volume(1, "fixture", "/fixture");
            state.index.ensure_root(volume, "/fixture");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .worker
            .cached(&SearchQuery::default())
            .is_none_or(|s| s.rows.is_empty())
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        app.text = "nothing-matches-this".into();
        app.search();
        while app.searching {
            app.collect();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.rows.as_ref().unwrap().rows.is_empty());
        let locked_index = engine.shared().write();
        app.text.clear();
        app.search();
        assert!(!app.rows.as_ref().unwrap().rows.is_empty());
        assert_eq!(app.rows_revision, Some(app.revision));
        drop(locked_index);
    }

    #[test]
    fn typed_character_submits_in_same_frame_without_blanking_results() {
        let (ctx, _, mut app) = app();
        let previous = app.rows.as_ref().unwrap().clone();
        let mut output = ctx.run_ui(Default::default(), |ui| app.toolbar(ui));
        output.textures_delta.clear();
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("search-input")));
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Text("m".into())],
                ..Default::default()
            },
            |ui| app.toolbar(ui),
        );
        output.textures_delta.clear();
        assert_eq!(app.text, "m");
        assert_eq!(app.query.name.as_deref(), Some("m"));
        assert_eq!(
            app.revision, 2,
            "do not wait for a debounce or repaint timer"
        );
        assert!(app.searching);
        if app.rows_revision == Some(app.revision) {
            // The empty fixture engine can finish before same-frame cache lookup.
            assert!(app.rows.as_ref().unwrap().rows.is_empty());
        } else {
            assert!(previous.rows.ptr_eq(&app.rows.as_ref().unwrap().rows));
        }
    }

    #[test]
    fn old_revisions_cannot_replace_view_and_new_queries_bypass_live_update_hold() {
        let (_, engine, mut app) = app();
        app.text = "m".into();
        app.search();
        let old = app.revision;
        app.text = "music".into();
        app.search();
        let newest = app.revision;
        app.accept_update(Update {
            revision: old,
            status: engine.status(),
            result: Some(Ok(fixture("outdated.txt"))),
        });
        assert_eq!(app.rows.as_ref().unwrap().rows[0].name, "previous.txt");
        assert!(app.searching);
        // Scrolling the retained view must not trap the new query behind Apply updates.
        app.browsing = true;
        app.accept_update(Update {
            revision: newest,
            status: engine.status(),
            result: Some(Ok(fixture("music.flac"))),
        });
        assert_eq!(app.rows.as_ref().unwrap().rows[0].name, "music.flac");
        assert!(app.pending.is_none());
        assert!(!app.searching);
        assert_eq!(app.rows_revision, Some(newest));
        // Same-query live changes still respect the user's held browsing view.
        app.browsing = true;
        app.accept_update(Update {
            revision: newest,
            status: engine.status(),
            result: Some(Ok(fixture("music-new.flac"))),
        });
        assert!(app.pending.is_some());
        assert_eq!(app.rows.as_ref().unwrap().rows[0].name, "music.flac");
    }

    #[test]
    fn invalid_expression_keeps_last_view_and_valid_empty_query_replaces_it() {
        let (_, engine, mut app) = app();
        app.text = "[".into();
        app.query.regex = true;
        app.search();
        app.accept_update(Update {
            revision: app.revision,
            status: engine.status(),
            result: Some(Err("invalid regex".into())),
        });
        assert_eq!(app.rows.as_ref().unwrap().rows[0].name, "previous.txt");
        assert_ne!(app.rows_revision, Some(app.revision));
        app.text = "no matches".into();
        app.search();
        app.accept_update(Update {
            revision: app.revision,
            status: engine.status(),
            result: Some(Ok(ResultSnapshot {
                generation: 1,
                rows: Default::default(),
            })),
        });
        assert!(app.rows.as_ref().unwrap().rows.is_empty());
        assert_eq!(app.rows_revision, Some(app.revision));
        assert!(!app.searching);
    }
}
