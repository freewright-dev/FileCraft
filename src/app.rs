//! The desktop UI.

use crate::actions;
use crate::index::{ATTR_HIDDEN, ATTR_SYSTEM, Index, SortKey, Source, data_dir};
use crate::indexer::{Drive, IndexEvent, Indexer, Phase, fixed_drives};
use crate::preview::{Content, Preview, PreviewLoader, SaveError};
use crate::search::{Hit, SearchOptions, Searcher, Sort};
use crate::util::{format_filetime, format_size, group_digits, local_offset};
use eframe::egui::{self, Align, Color32, Key, Layout, Modifiers, RichText};
use egui_extras::{Column, TableBuilder};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const KEY_EDITOR: &str = "editor";
const KEY_EXCLUDED: &str = "excluded_drives";
const KEY_MAX_AGE: &str = "reindex_after_minutes";
const KEY_PREVIEW: &str = "show_preview";
const KEY_OPTIONS: &str = "search_options";
const KEY_SORT: &str = "sort";
/// Upper end of the "re-index when older than" setting: one week.
const MAX_AGE_LIMIT_MINUTES: u64 = 10_080;

enum Pending {
    Select(usize),
    Close,
}

#[derive(Clone, Copy)]
enum RowAction {
    Open,
    Reveal,
    Properties,
    EditInline,
    EditExternal,
    CopyPath,
    CopyName,
}

pub struct FileCraftApp {
    indexer: Indexer,
    searcher: Searcher,
    drives: Vec<Drive>,
    /// Latest snapshot of all indexed volumes.
    index: Index,

    query: String,
    options: SearchOptions,
    sort: Sort,
    needs_search: bool,
    latest_generation: u64,
    searching: bool,
    search_error: Option<String>,
    search_time: Duration,
    /// Snapshot the current `hits` refer to.
    shown: Index,
    hits: Arc<Vec<Hit>>,

    selected: Option<Hit>,
    selected_row: Option<usize>,
    selected_path: Option<(String, String)>,
    scroll_to_selected: bool,
    visible_rows: usize,

    show_preview: bool,
    preview: Option<Preview>,
    preview_loader: PreviewLoader,
    preview_request: Option<PathBuf>,
    edit_when_ready: bool,
    editor_focused: bool,
    pending: Option<Pending>,
    allow_close: bool,

    editor: String,
    excluded: BTreeSet<char>,
    max_age_minutes: u64,
    show_settings: bool,
    show_help: bool,
    focus_search: bool,
    status: Option<(String, Instant)>,
    tz_offset: i64,
}

impl FileCraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let get = |key: &str| cc.storage.and_then(|s| s.get_string(key));
        let editor = get(KEY_EDITOR).filter(|s| !s.trim().is_empty()).unwrap_or_else(actions::default_editor);
        let excluded: BTreeSet<char> = get(KEY_EXCLUDED).unwrap_or_default().chars().filter(char::is_ascii_uppercase).collect();
        // Clamped like the settings field, so an edited value cannot overflow the duration.
        let max_age_minutes = get(KEY_MAX_AGE).and_then(|s| s.parse().ok()).unwrap_or(60).min(MAX_AGE_LIMIT_MINUTES);
        let show_preview = get(KEY_PREVIEW).is_none_or(|s| s == "1");
        let flags = get(KEY_OPTIONS).unwrap_or_default();
        let options = SearchOptions {
            match_case: flags.contains('c'),
            match_path: flags.contains('p'),
            regex: flags.contains('r'),
        };
        let sort = get(KEY_SORT).map(|s| decode_sort(&s)).unwrap_or_default();

        let ctx = cc.egui_ctx.clone();
        let searcher = Searcher::new(move || ctx.request_repaint());
        let ctx = cc.egui_ctx.clone();
        let mut indexer = Indexer::new(move || ctx.request_repaint());
        let ctx = cc.egui_ctx.clone();
        let preview_loader = PreviewLoader::new(move || ctx.request_repaint());

        let drives = fixed_drives();
        let included: Vec<Drive> = drives.iter().filter(|d| !excluded.contains(&d.letter)).cloned().collect();
        indexer.start(included, Duration::from_secs(max_age_minutes * 60), false);

        FileCraftApp {
            indexer,
            searcher,
            drives,
            index: Index::default(),
            query: String::new(),
            options,
            sort,
            needs_search: true,
            latest_generation: 0,
            searching: false,
            search_error: None,
            search_time: Duration::ZERO,
            shown: Index::default(),
            hits: Arc::new(Vec::new()),
            selected: None,
            selected_row: None,
            selected_path: None,
            scroll_to_selected: false,
            visible_rows: 20,
            show_preview,
            preview: None,
            preview_loader,
            preview_request: None,
            edit_when_ready: false,
            editor_focused: false,
            pending: None,
            allow_close: false,
            editor,
            excluded,
            max_age_minutes,
            show_settings: false,
            show_help: false,
            focus_search: true,
            status: None,
            tz_offset: local_offset(),
        }
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    fn max_age(&self) -> Duration {
        Duration::from_secs(self.max_age_minutes * 60)
    }

    fn included_drives(&self) -> Vec<Drive> {
        self.drives.iter().filter(|d| !self.excluded.contains(&d.letter)).cloned().collect()
    }

    // ── Background results ──

    fn poll(&mut self) {
        while let Ok(IndexEvent::Ready(volume)) = self.indexer.events.try_recv() {
            if !self.excluded.contains(&volume.letter) {
                self.index = self.index.with_volume(volume);
                self.needs_search = true;
            }
        }

        while let Ok(res) = self.searcher.results.try_recv() {
            if res.generation != self.latest_generation {
                continue;
            }
            self.searching = false;
            if let Some(err) = res.error {
                // Keep the previous results visible while the query is being typed.
                self.search_error = Some(err);
                continue;
            }
            self.search_error = None;
            self.search_time = res.elapsed;
            self.hits = res.hits;
            self.shown = res.index;
            self.restore_selection();
        }

        if let Some(preview) = self.preview_loader.poll()
            && self.preview_request.as_ref() == Some(&preview.path)
        {
            self.preview_request = None;
            self.preview = Some(preview);
            if std::mem::take(&mut self.edit_when_ready) {
                self.start_edit();
            }
        }

        if self.needs_search {
            self.needs_search = false;
            self.searching = true;
            self.latest_generation = self.searcher.submit(self.index.clone(), &self.query, self.options, self.sort);
        }
    }

    /// Finds the previously selected item in the new results (by name, then full path).
    fn restore_selection(&mut self) {
        self.selected = None;
        self.selected_row = None;
        let Some((lower_name, path)) = self.selected_path.clone() else { return };
        let found = self.hits.iter().position(|h| {
            let v = self.shown.volume(h.vol);
            v.lower_name(h.idx) == lower_name && v.full_path(h.idx) == path
        });
        match found {
            Some(row) => {
                self.selected_row = Some(row);
                self.selected = Some(self.hits[row]);
            }
            None => {
                self.selected_path = None;
                if !self.preview.as_ref().is_some_and(|p| p.dirty) {
                    self.preview = None;
                    self.preview_request = None;
                    self.edit_when_ready = false;
                }
            }
        }
    }

    fn submit_search(&mut self) {
        self.needs_search = true;
    }

    // ── Selection ──

    fn request_select(&mut self, row: usize) {
        if row >= self.hits.len() {
            return;
        }
        let same = self.selected_row == Some(row);
        if !same && self.preview.as_ref().is_some_and(|p| p.dirty) {
            self.pending = Some(Pending::Select(row));
            return;
        }
        self.select(row);
    }

    fn select(&mut self, row: usize) {
        let hit = self.hits[row];
        let v = self.shown.volume(hit.vol);
        self.selected = Some(hit);
        self.selected_row = Some(row);
        self.selected_path = Some((v.lower_name(hit.idx).to_string(), v.full_path(hit.idx)));
        self.scroll_to_selected = true;
        self.load_preview();
    }

    fn load_preview(&mut self) {
        if !self.show_preview {
            return;
        }
        let Some(hit) = self.selected else { return };
        let v = self.shown.volume(hit.vol);
        let path = PathBuf::from(v.full_path(hit.idx));
        if self.preview.as_ref().is_some_and(|p| p.path == path || p.dirty)
            || self.preview_request.as_ref() == Some(&path)
        {
            return;
        }
        self.request_preview(path, v.entry(hit.idx).is_dir());
    }

    fn request_preview(&mut self, path: PathBuf, is_dir: bool) {
        self.preview = None;
        self.edit_when_ready = false;
        self.preview_request = Some(path.clone());
        self.preview_loader.submit(path, is_dir);
    }

    /// Starts editing the loaded preview. A truncated text preview is first read in full on the
    /// preview thread (the current preview stays visible); editing starts when that arrives.
    fn start_edit(&mut self) {
        let Some(p) = self.preview.as_mut() else { return };
        if p.needs_full_read() {
            let path = p.path.clone();
            self.preview_request = Some(path.clone());
            self.preview_loader.submit_edit(path);
            self.edit_when_ready = true;
            self.set_status("Reading the whole file for editing…");
        } else if let Err(e) = p.begin_edit() {
            self.set_status(e);
        }
    }

    fn run_action(&mut self, action: RowAction, ctx: &egui::Context) {
        let Some(hit) = self.selected else { return };
        let v = self.shown.volume(hit.vol);
        let path = PathBuf::from(v.full_path(hit.idx));
        let name = v.name(hit.idx).to_string();
        let result = match action {
            RowAction::Open => actions::open(&path),
            RowAction::Reveal => actions::reveal(&path),
            RowAction::Properties => actions::properties(&path),
            RowAction::EditExternal => actions::edit_externally(&self.editor, &path),
            RowAction::CopyPath => {
                ctx.copy_text(path.display().to_string());
                self.set_status("Copied full path");
                Ok(())
            }
            RowAction::CopyName => {
                ctx.copy_text(name);
                self.set_status("Copied name");
                Ok(())
            }
            RowAction::EditInline => {
                if !self.show_preview {
                    self.show_preview = true;
                    self.load_preview();
                }
                if self.preview_request.as_ref() == Some(&path) {
                    self.edit_when_ready = true;
                }
                if self.preview.as_ref().is_some_and(|p| p.path == path) {
                    self.start_edit();
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            self.set_status(format!("Failed: {e}"));
        }
    }

    fn rescan(&mut self) {
        let drives = self.included_drives();
        self.indexer.start(drives, self.max_age(), true);
        self.set_status("Re-indexing all drives…");
    }

    fn restart_as_admin(&mut self, ctx: &egui::Context) {
        match actions::restart_elevated() {
            Ok(()) => {
                self.allow_close = !self.preview.as_ref().is_some_and(|p| p.dirty);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => self.set_status(format!("Could not restart as administrator: {e}")),
        }
    }

    fn save_edit(&mut self, force: bool) -> bool {
        let Some(p) = self.preview.as_mut() else { return false };
        match p.save(force) {
            Ok(()) => {
                let name = p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.set_status(format!("Saved {name}"));
                true
            }
            Err(SaveError::Conflict) => {
                self.set_status("The file changed or was deleted on disk — choose Overwrite or Reload");
                false
            }
            Err(SaveError::Io(e)) => {
                self.set_status(format!("Save failed: {e}"));
                false
            }
        }
    }

    // ── Keyboard ──

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let editing_text = self.editor_focused;
        let mut nav: isize = 0;
        let mut jump: Option<bool> = None; // Some(true) = end, Some(false) = start
        let mut action = None;
        let (mut f1, mut f2, mut f5, mut find, mut save, mut esc) = (false, false, false, false, false, false);
        let page = self.visible_rows.max(1) as isize;

        ctx.input_mut(|i| {
            f1 = i.consume_key(Modifiers::NONE, Key::F1);
            f2 = i.consume_key(Modifiers::NONE, Key::F2);
            f5 = i.consume_key(Modifiers::NONE, Key::F5);
            find = i.consume_key(Modifiers::COMMAND, Key::F);
            save = i.consume_key(Modifiers::COMMAND, Key::S);
            if editing_text {
                return;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                nav = 1;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                nav = -1;
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                nav = page;
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                nav = -page;
            }
            if i.consume_key(Modifiers::COMMAND, Key::Home) {
                jump = Some(false);
            }
            if i.consume_key(Modifiers::COMMAND, Key::End) {
                jump = Some(true);
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::C) {
                action = Some(RowAction::CopyPath);
            } else if i.consume_key(Modifiers::COMMAND, Key::Enter) {
                action = Some(RowAction::Reveal);
            } else if i.consume_key(Modifiers::ALT, Key::Enter) {
                action = Some(RowAction::Properties);
            } else if i.consume_key(Modifiers::NONE, Key::Enter) {
                action = Some(RowAction::Open);
            } else if i.consume_key(Modifiers::COMMAND, Key::E) {
                action = Some(RowAction::EditExternal);
            } else if i.consume_key(Modifiers::NONE, Key::F4) {
                action = Some(RowAction::EditInline);
            }
            esc = i.consume_key(Modifiers::NONE, Key::Escape);
        });

        if f1 {
            self.show_help = !self.show_help;
        }
        if f2 {
            self.show_preview = !self.show_preview;
            self.load_preview();
        }
        if f5 {
            self.rescan();
        }
        if find {
            self.focus_search = true;
        }
        if save && self.preview.as_ref().is_some_and(|p| p.editing) {
            self.save_edit(false);
        }
        if esc {
            if self.show_help || self.show_settings {
                self.show_help = false;
                self.show_settings = false;
            } else if !self.query.is_empty() {
                self.query.clear();
                self.submit_search();
            }
            self.focus_search = true;
        }

        let len = self.hits.len();
        if len > 0 {
            if nav != 0 {
                let target = match self.selected_row {
                    Some(r) => (r as isize + nav).clamp(0, len as isize - 1) as usize,
                    None => 0,
                };
                self.request_select(target);
            }
            match jump {
                Some(true) => self.request_select(len - 1),
                Some(false) => self.request_select(0),
                None => {}
            }
        }
        if let Some(a) = action {
            if self.selected.is_none() && len > 0 && matches!(a, RowAction::Open) {
                self.request_select(0);
            }
            self.run_action(a, ctx);
        }
    }

    // ── Panels ──

    fn menu_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("Re-index all drives").shortcut_text("F5")).clicked() {
                    self.rescan();
                    ui.close();
                }
                if !self.indexer.elevated && ui.button("Restart as administrator (fast NTFS indexing)").clicked() {
                    ui.close();
                    self.restart_as_admin(ctx);
                }
                if ui.button("Settings…").clicked() {
                    self.show_settings = true;
                    ui.close();
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Search", |ui| {
                let before = self.options;
                ui.checkbox(&mut self.options.match_case, "Match case");
                ui.checkbox(&mut self.options.match_path, "Match full path");
                ui.checkbox(&mut self.options.regex, "Regular expression");
                if self.options != before {
                    self.submit_search();
                }
            });
            ui.menu_button("View", |ui| {
                if ui.add(egui::Button::selectable(self.show_preview, "Preview pane").shortcut_text("F2")).clicked() {
                    self.show_preview = !self.show_preview;
                    self.load_preview();
                    ui.close();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.add(egui::Button::new("Search syntax & shortcuts").shortcut_text("F1")).clicked() {
                    self.show_help = true;
                    ui.close();
                }
            });
        });
    }

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        let row = egui::vec2(ui.available_width(), 30.0);
        ui.allocate_ui_with_layout(row, Layout::right_to_left(Align::Center), |ui| {
            let toggle = |ui: &mut egui::Ui, on: &mut bool, label: &str, tip: &str| {
                if ui.selectable_label(*on, RichText::new(label).monospace()).on_hover_text(tip).clicked() {
                    *on = !*on;
                    true
                } else {
                    false
                }
            };
            let mut changed = false;
            changed |= toggle(ui, &mut self.options.regex, ".*", "Regular expression");
            changed |= toggle(ui, &mut self.options.match_path, "\\", "Match full path");
            changed |= toggle(ui, &mut self.options.match_case, "Aa", "Match case");

            let resp = ui.add_sized(
                [ui.available_width(), 28.0],
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search everything…   e.g.  report ext:pdf   *.rs   size:>100mb   dm:today   folder:src")
                    .font(egui::TextStyle::Body)
                    .vertical_align(Align::Center),
            );
            if self.focus_search {
                resp.request_focus();
                self.focus_search = false;
            }
            if resp.changed() {
                changed = true;
            }
            if changed {
                self.submit_search();
            }
        });
        if let Some(err) = &self.search_error {
            ui.colored_label(ui.visuals().error_fg_color, format!("⚠ {err}"));
        }
        ui.add_space(4.0);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let total = self.shown.total_entries();
            let count = group_digits(self.hits.len());
            let text = if self.query.trim().is_empty() {
                format!("{count} objects")
            } else {
                format!("{count} of {} objects", group_digits(total))
            };
            ui.label(RichText::new(text).strong());
            if self.searching {
                ui.spinner();
            } else if !self.hits.is_empty() || !self.query.is_empty() {
                ui.weak(format!("{:.0} ms", self.search_time.as_secs_f64() * 1000.0));
            }
            if let Some((msg, at)) = &self.status
                && at.elapsed() < Duration::from_secs(6)
            {
                ui.separator();
                ui.label(RichText::new(msg).color(ui.visuals().warn_fg_color));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.indexer.elevated
                    && ui
                        .small_button("⚡ Run as admin")
                        .on_hover_text("Restart elevated to read the NTFS master file table directly: indexing takes seconds instead of minutes.")
                        .clicked()
                {
                    self.restart_as_admin(ctx);
                }
                let statuses: Vec<_> = self.indexer.status.lock().unwrap().values().cloned().collect();
                for s in statuses.iter().rev() {
                    drive_chip(ui, s);
                }
            });
        });
    }

    fn results_table(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.hits.is_empty() {
            ui.centered_and_justified(|ui| {
                let msg = if self.index.volumes.is_empty() && self.indexer.is_busy() {
                    "Indexing drives…"
                } else if self.searching {
                    "Searching…"
                } else if self.index.volumes.is_empty() {
                    "No drives indexed. Check File → Settings."
                } else {
                    "No results"
                };
                ui.weak(msg);
            });
            return;
        }

        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        self.visible_rows = ((ui.available_height() - 24.0) / row_height).max(1.0) as usize;
        let mut clicked: Option<usize> = None;
        let mut double_clicked = false;
        let mut menu_action: Option<(usize, RowAction)> = None;
        let mut header_click: Option<SortKey> = None;

        let weak = ui.visuals().weak_text_color();
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(280.0).at_least(80.0).clip(true))
            .column(Column::initial(420.0).at_least(80.0).clip(true))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::remainder().at_least(120.0))
            .min_scrolled_height(0.0);
        if self.scroll_to_selected {
            if let Some(row) = self.selected_row {
                table = table.scroll_to_row(row, None);
            }
            self.scroll_to_selected = false;
        }

        let sort = self.sort;
        table
            .header(24.0, |mut header| {
                for (key, title) in [
                    (SortKey::Name, "Name"),
                    (SortKey::Path, "Path"),
                    (SortKey::Size, "Size"),
                    (SortKey::Modified, "Date Modified"),
                ] {
                    header.col(|ui| {
                        let arrow = match (sort.key == key, sort.descending) {
                            (true, false) => " ⏶",
                            (true, true) => " ⏷",
                            _ => "",
                        };
                        if ui.add(egui::Button::new(RichText::new(format!("{title}{arrow}")).strong()).frame(false)).clicked() {
                            header_click = Some(key);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, self.hits.len(), |mut row| {
                    let i = row.index();
                    let hit = self.hits[i];
                    let v = self.shown.volume(hit.vol);
                    let e = *v.entry(hit.idx);
                    row.set_selected(self.selected_row == Some(i));
                    let dim = e.attrs & (ATTR_HIDDEN | ATTR_SYSTEM) != 0;

                    row.col(|ui| {
                        let icon = if e.is_dir() { "🗀" } else { "🗋" };
                        let mut text = RichText::new(format!("{icon} {}", v.name(hit.idx)));
                        if dim {
                            text = text.color(weak);
                        }
                        ui.add(egui::Label::new(text).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(v.parent_path(hit.idx)).color(weak)).truncate().selectable(false));
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add(egui::Label::new(format_size(e.size)).selectable(false));
                        });
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(format_filetime(e.mtime, self.tz_offset)).selectable(false));
                    });

                    let resp = row.response();
                    if resp.clicked() || resp.secondary_clicked() {
                        clicked = Some(i);
                    }
                    if resp.double_clicked() {
                        clicked = Some(i);
                        double_clicked = true;
                    }
                    resp.context_menu(|ui| {
                        let items = [
                            ("Open", RowAction::Open),
                            ("Show in folder", RowAction::Reveal),
                            ("Edit in preview pane", RowAction::EditInline),
                            ("Open in external editor", RowAction::EditExternal),
                            ("Copy full path", RowAction::CopyPath),
                            ("Copy name", RowAction::CopyName),
                            ("Properties", RowAction::Properties),
                        ];
                        for (label, act) in items {
                            if matches!(act, RowAction::EditInline | RowAction::EditExternal) && e.is_dir() {
                                continue;
                            }
                            if ui.button(label).clicked() {
                                menu_action = Some((i, act));
                                ui.close();
                            }
                        }
                    });
                });
            });

        if let Some(key) = header_click {
            self.sort = if self.sort.key == key {
                Sort { key, descending: !self.sort.descending }
            } else {
                Sort { key, descending: matches!(key, SortKey::Size | SortKey::Modified) }
            };
            self.submit_search();
        }
        if let Some(row) = clicked {
            self.request_select(row);
            self.scroll_to_selected = false;
        }
        if double_clicked && self.selected_row == clicked {
            self.run_action(RowAction::Open, ctx);
        }
        if let Some((row, act)) = menu_action {
            self.request_select(row);
            if self.selected_row == Some(row) {
                self.run_action(act, ctx);
            }
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.editor_focused = false;
        let Some(hit) = self.selected else {
            ui.add_space(12.0);
            ui.weak("Select a result to preview it here.");
            return;
        };
        self.load_preview();
        let v = self.shown.volume(hit.vol);
        let e = *v.entry(hit.idx);
        let name = v.name(hit.idx).to_string();
        let path = v.full_path(hit.idx);
        let source = v.source;

        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(&name).heading()).wrap());
        ui.add(egui::Label::new(RichText::new(&path).small().weak()).wrap());
        ui.weak(format!(
            "{} · {} · indexed by {}",
            format_size(e.size),
            format_filetime(e.mtime, self.tz_offset),
            source.label()
        ));
        ui.add_space(4.0);

        let mut action = None;
        let (mut begin_edit, mut save, mut force_save, mut cancel, mut reload) = (false, false, false, false, false);
        let editing = self.preview.as_ref().is_some_and(|p| p.editing);
        let can_edit = self.preview.as_ref().is_some_and(|p| p.can_edit());
        let not_editable = self.preview.as_ref().and_then(|p| p.not_editable.clone());

        ui.horizontal_wrapped(|ui| {
            if ui.button("Open").clicked() {
                action = Some(RowAction::Open);
            }
            if ui.button("Show in folder").clicked() {
                action = Some(RowAction::Reveal);
            }
            if ui.button("Copy path").clicked() {
                action = Some(RowAction::CopyPath);
            }
            if !e.is_dir() {
                if !editing {
                    let resp = ui.add_enabled(can_edit, egui::Button::new("✏ Edit"));
                    let resp = match &not_editable {
                        Some(reason) => resp.on_disabled_hover_text(format!("Cannot edit inline: {reason}")),
                        None => resp.on_hover_text("Edit this file here (F4)"),
                    };
                    if resp.clicked() {
                        begin_edit = true;
                    }
                }
                if ui.button("External editor").on_hover_text(format!("{} (Ctrl+E)", self.editor)).clicked() {
                    action = Some(RowAction::EditExternal);
                }
            }
            if ui.button("Properties").clicked() {
                action = Some(RowAction::Properties);
            }
        });

        if editing {
            let (dirty, conflict) = self.preview.as_ref().map_or((false, false), |p| (p.dirty, p.conflict));
            ui.horizontal(|ui| {
                if ui.add_enabled(dirty, egui::Button::new("💾 Save")).on_hover_text("Ctrl+S").clicked() {
                    save = true;
                }
                if ui.button(if dirty { "Discard changes" } else { "Done" }).clicked() {
                    cancel = true;
                }
                if dirty {
                    ui.colored_label(ui.visuals().warn_fg_color, "● modified");
                }
            });
            if let Some(style) = self.preview.as_ref().and_then(|p| p.mixed_newlines_saved_as()) {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("This file mixes CRLF and LF line endings. Saving uses {style} for every line."),
                );
            }
            if conflict {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(ui.visuals().error_fg_color, "This file changed or was deleted on disk since you opened it.");
                    if ui.button("Overwrite").clicked() {
                        force_save = true;
                    }
                    if ui.button("Reload from disk").clicked() {
                        reload = true;
                    }
                });
            }
        }
        ui.separator();

        if let Some(p) = self.preview.as_mut() {
            if p.editing {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut p.buffer)
                            .code_editor()
                            .desired_width(f32::INFINITY)
                            .desired_rows(40)
                            .lock_focus(true),
                    );
                    if resp.changed() {
                        p.dirty = true;
                    }
                    self.editor_focused = resp.has_focus();
                });
            } else {
                show_content(ui, &mut p.content);
            }
        } else if self.preview_request.is_some() {
            ui.weak("Loading preview…");
        }

        if begin_edit {
            self.start_edit();
        }
        if save {
            self.save_edit(false);
        }
        if force_save {
            self.save_edit(true);
        }
        if cancel && let Some(p) = self.preview.as_mut() {
            p.cancel_edit();
        }
        if reload {
            self.request_preview(PathBuf::from(&path), e.is_dir());
        }
        if let Some(a) = action {
            self.run_action(a, ctx);
        }
    }

    fn unsaved_modal(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending else { return };
        let name = self
            .preview
            .as_ref()
            .and_then(|p| p.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let closing = matches!(pending, Pending::Close);
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("unsaved-changes")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading("Unsaved changes");
            ui.label(format!("Save your changes to {name}?"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(0);
                }
                if ui.button("Don't save").clicked() {
                    choice = Some(1);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(2);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(2);
        }
        let Some(choice) = choice else { return };
        let pending = self.pending.take();
        let proceed = match choice {
            0 => self.save_edit(false),
            1 => {
                if let Some(p) = self.preview.as_mut() {
                    p.cancel_edit();
                }
                true
            }
            _ => false,
        };
        if !proceed {
            return;
        }
        match pending {
            Some(Pending::Select(row)) => {
                self.preview = None;
                if row < self.hits.len() {
                    self.select(row);
                }
            }
            Some(Pending::Close) if closing => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            _ => {}
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut changed_drives: Vec<(char, bool)> = Vec::new();
        let mut clear_cache = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.strong("External editor");
                ui.label("Command used by “External editor” and Ctrl+E. The file path is appended.");
                ui.text_edit_singleline(&mut self.editor);
                ui.add_space(10.0);

                ui.strong("Drives");
                for d in &self.drives {
                    let mut on = !self.excluded.contains(&d.letter);
                    let label = if d.label.is_empty() {
                        format!("{}:  ({})", d.letter, d.filesystem)
                    } else {
                        format!("{}:  {}  ({})", d.letter, d.label, d.filesystem)
                    };
                    if ui.checkbox(&mut on, label).changed() {
                        changed_drives.push((d.letter, on));
                    }
                }
                ui.add_space(10.0);

                ui.strong("Index");
                ui.horizontal(|ui| {
                    ui.label("Re-index on startup when older than");
                    ui.add(egui::DragValue::new(&mut self.max_age_minutes).range(0..=MAX_AGE_LIMIT_MINUTES).suffix(" min"));
                });
                ui.weak(format!("Cache: {}", data_dir().join("index").display()));
                if ui.button("Delete cached indexes").clicked() {
                    clear_cache = true;
                }
            });
        self.show_settings = open;

        for (letter, on) in changed_drives {
            if on {
                self.excluded.remove(&letter);
                if let Some(d) = self.drives.iter().find(|d| d.letter == letter).cloned() {
                    self.indexer.start(vec![d], self.max_age(), false);
                }
            } else {
                self.excluded.insert(letter);
                self.indexer.remove(letter);
                self.index = self.index.without_volume(letter);
                self.submit_search();
            }
        }
        if clear_cache {
            match std::fs::remove_dir_all(data_dir().join("index")) {
                Ok(()) => self.set_status("Deleted cached indexes; they are rebuilt on the next re-index"),
                Err(e) => self.set_status(format!("Could not delete cache: {e}")),
            }
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Search syntax & shortcuts")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let rows: &[(&str, &str)] = &[
                        ("foo bar", "names containing foo and bar"),
                        ("\"foo bar\"", "names containing the exact text foo bar"),
                        ("!foo", "names not containing foo"),
                        ("foo|bar", "names containing foo or bar"),
                        ("*.rs   a?c", "wildcards, matched against the whole name"),
                        ("src\\main", "a term with \\ or / matches the full path"),
                        ("ext:pdf;docx", "file extension"),
                        ("file:   folder:", "only files / only folders (folder:src works too)"),
                        ("size:>100mb", "size; also >=, <, <=, 1mb..10mb, empty"),
                        ("dm:today", "modified: today, yesterday, 24h, 7d, 2w, 1y"),
                        ("dm:2026-01-31", "modified that day; also >date, <date, a..b"),
                        ("regex:^a.*z$", "regular expression on the name"),
                        ("path:foo", "match this term against the full path"),
                    ];
                    ui.strong("Search");
                    egui::Grid::new("syntax").striped(true).num_columns(2).show(ui, |ui| {
                        for (q, d) in rows {
                            ui.monospace(*q);
                            ui.label(*d);
                            ui.end_row();
                        }
                    });
                    ui.add_space(10.0);
                    ui.strong("Keyboard");
                    let keys: &[(&str, &str)] = &[
                        ("↑ ↓  PgUp PgDn", "move the selection"),
                        ("Enter / double-click", "open"),
                        ("Ctrl+Enter", "show in folder"),
                        ("Alt+Enter", "properties"),
                        ("F4", "edit in the preview pane"),
                        ("Ctrl+S", "save the file being edited"),
                        ("Ctrl+E", "open in the external editor"),
                        ("Ctrl+Shift+C", "copy full path"),
                        ("Ctrl+F / Esc", "focus the search box / clear it"),
                        ("F2", "toggle the preview pane"),
                        ("F5", "re-index all drives"),
                    ];
                    egui::Grid::new("keys").striped(true).num_columns(2).show(ui, |ui| {
                        for (k, d) in keys {
                            ui.monospace(*k);
                            ui.label(*d);
                            ui.end_row();
                        }
                    });
                });
            });
        self.show_help = open;
    }
}

fn show_content(ui: &mut egui::Ui, content: &mut Content) {
    // Uploaded once; the texture is freed when the preview is replaced.
    if let Content::Image(image) = content
        && let Some(pixels) = image.pixels.take()
    {
        image.texture = Some(ui.ctx().load_texture("preview-image", pixels, egui::TextureOptions::LINEAR));
    }
    match &*content {
        Content::Text { text, truncated } => {
            if *truncated {
                ui.weak("Showing the beginning of the file.");
            }
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                let mut view: &str = text;
                ui.add(egui::TextEdit::multiline(&mut view).code_editor().desired_width(f32::INFINITY));
            });
        }
        Content::Image(image) => {
            if let Some(texture) = &image.texture {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    ui.add(egui::Image::new(texture).max_size(ui.available_size()).maintain_aspect_ratio(true));
                });
            }
        }
        Content::Binary(dump) => {
            ui.weak("Binary file");
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(dump).monospace()).extend());
            });
        }
        Content::Directory { entries, total } => {
            ui.weak(format!("{} items", group_digits(*total)));
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                for (is_dir, name) in entries {
                    ui.label(format!("{} {name}", if *is_dir { "🗀" } else { "🗋" }));
                }
                if *total > entries.len() {
                    ui.weak(format!("… {} more", total - entries.len()));
                }
            });
        }
        Content::Message(msg) => {
            ui.weak(msg);
        }
    }
}

fn drive_chip(ui: &mut egui::Ui, s: &crate::indexer::VolumeStatus) {
    use std::sync::atomic::Ordering;
    let letter = s.drive.letter;
    let progress = s.progress.load(Ordering::Relaxed);
    let total = s.total.load(Ordering::Relaxed);
    let (text, color, tip) = match &s.phase {
        Phase::Waiting => (format!("{letter}: …"), None, "Waiting".to_string()),
        Phase::Indexing(Source::Mft) => {
            let pct = (progress * 100).checked_div(total).unwrap_or(0);
            (format!("{letter}: {pct}%"), None, "Reading the NTFS master file table".to_string())
        }
        Phase::Indexing(Source::Walk) => (
            format!("{letter}: {}", group_digits(progress)),
            None,
            "Scanning folders (run as administrator for much faster indexing)".to_string(),
        ),
        Phase::Ready => {
            let mut tip = format!("{} entries", group_digits(s.entries));
            if let Some(src) = s.source {
                tip.push_str(&format!(" · {}", src.label()));
            }
            if let Some(at) = s.built_at {
                tip.push_str(&format!(" · indexed {}", ago(at)));
            }
            if let Some(t) = s.build_time {
                tip.push_str(&format!(" in {:.1} s", t.as_secs_f64()));
            }
            if s.refreshing {
                tip.push_str(" · refreshing now");
            }
            if s.skipped > 0 {
                tip.push_str(&format!("\n{} items could not be read and are missing", group_digits(s.skipped)));
            }
            if let Some(err) = &s.cache_error {
                tip.push_str(&format!("\nThe index could not be cached: {err}"));
            }
            let mark = if s.refreshing { "⟳" } else { "✔" };
            (format!("{letter}: {mark}"), Some(Color32::from_rgb(90, 170, 90)), tip)
        }
        Phase::Failed(err) => (format!("{letter}: ✖"), Some(ui.visuals().error_fg_color), format!("Indexing failed: {err}")),
    };
    let mut rt = RichText::new(text).small();
    if let Some(c) = color {
        rt = rt.color(c);
    }
    ui.label(rt).on_hover_text(tip);
    if matches!(s.phase, Phase::Indexing(_)) || s.refreshing {
        ui.spinner();
    }
}

fn ago(t: SystemTime) -> String {
    let secs = t.elapsed().unwrap_or_default().as_secs();
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

fn encode_sort(s: Sort) -> String {
    let key = match s.key {
        SortKey::Name => "name",
        SortKey::Path => "path",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
    };
    format!("{key}:{}", if s.descending { "desc" } else { "asc" })
}

fn decode_sort(s: &str) -> Sort {
    let (key, dir) = s.split_once(':').unwrap_or((s, "asc"));
    let key = match key {
        "path" => SortKey::Path,
        "size" => SortKey::Size,
        "modified" => SortKey::Modified,
        _ => SortKey::Name,
    };
    Sort { key, descending: dir == "desc" }
}

impl eframe::App for FileCraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.preview.as_ref().is_some_and(|p| p.dirty)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending = Some(Pending::Close);
        }

        self.poll();
        if self.pending.is_none() {
            self.handle_keys(&ctx);
        }
        self.poll();
        if self.indexer.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui, &ctx));
        egui::Panel::top("search").show(ui, |ui| self.search_bar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui, &ctx));
        if self.show_preview {
            egui::Panel::right("preview")
                .resizable(true)
                .default_size(420.0)
                .min_size(240.0)
                .show(ui, |ui| self.preview_panel(ui, &ctx));
        }
        egui::CentralPanel::default().show(ui, |ui| self.results_table(ui, &ctx));

        self.settings_window(&ctx);
        self.help_window(&ctx);
        self.unsaved_modal(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(KEY_EDITOR, self.editor.clone());
        storage.set_string(KEY_EXCLUDED, self.excluded.iter().collect());
        storage.set_string(KEY_MAX_AGE, self.max_age_minutes.to_string());
        storage.set_string(KEY_PREVIEW, if self.show_preview { "1" } else { "0" }.into());
        let mut flags = String::new();
        if self.options.match_case {
            flags.push('c');
        }
        if self.options.match_path {
            flags.push('p');
        }
        if self.options.regex {
            flags.push('r');
        }
        storage.set_string(KEY_OPTIONS, flags);
        storage.set_string(KEY_SORT, encode_sort(self.sort));
    }
}
