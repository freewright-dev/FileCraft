//! The quick search bar: a small always-on-top window, opened by the global hotkey, that finds and
//! opens things like a launcher does.
//!
//! It is a deferred viewport, because those render on their own: `eframe` runs no UI for the main
//! window while that is hidden, and an immediate viewport only renders as part of its parent. That
//! is also why the state lives behind a mutex and the bar has a searcher of its own.

use crate::actions;
use crate::hotkey::{self, BarSignals};
use crate::index::{ATTR_HIDDEN, ATTR_SYSTEM, Index, VolumeIndex};
use crate::search::{Hit, SearchOptions, Searcher, Sort};
use crate::util::{group_digits, win32_alias};
use crossbeam_channel::Sender;
use eframe::egui::{self, Align2, FontId, Key, Modifiers, RichText};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// The title identifies the window to `hotkey::platform`, which shows and hides it.
pub const TITLE: &str = "FileCraft Quick Search";

const WIDTH: f32 = 640.0;
const INPUT_HEIGHT: f32 = 52.0;
const ROW_HEIGHT: f32 = 44.0;
const FOOTER_HEIGHT: f32 = 26.0;
/// How many results the bar shows.
const MAX_ROWS: usize = 8;
/// How many hits are ranked. Beyond this a query matches so much that ranking all of it would take
/// longer than the next keystroke does to arrive, and it would not narrow things down anyway.
const RANK_LIMIT: usize = 500_000;

pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("quick-search-bar")
}

fn height(rows: usize, has_footer: bool) -> f32 {
    INPUT_HEIGHT + rows as f32 * ROW_HEIGHT + if has_footer { FOOTER_HEIGHT } else { 0.0 }
}

// ─── Ranking ────────────────────────────────────────────────────────────────

/// How well a name matches what was typed; lower is better.
fn name_tier(lower_name: &str, needle: &str) -> u8 {
    if needle.is_empty() {
        return 5;
    }
    if lower_name == needle {
        return 0;
    }
    if lower_name.rsplit_once('.').is_some_and(|(stem, _)| stem == needle) {
        return 1;
    }
    if lower_name.starts_with(needle) {
        return 2;
    }
    let at_word_start = lower_name.match_indices(needle).any(|(at, _)| {
        lower_name[..at].chars().next_back().is_some_and(|c| !c.is_alphanumeric())
    });
    if at_word_start {
        3
    } else if lower_name.contains(needle) {
        4
    } else {
        5
    }
}

/// Splits a query at the whitespace outside quotes.
fn terms(query: &str) -> impl Iterator<Item = &str> {
    let mut in_quotes = false;
    query
        .split(move |c: char| {
            if c == '"' {
                in_quotes = !in_quotes;
            }
            c.is_whitespace() && !in_quotes
        })
        .filter(|term| !term.is_empty())
}

/// The first search term that is plain text, which is what names are compared with. Terms with
/// operators (`ext:pdf`, `*.rs`, `!draft`, `a|b`, `src\main`) filter but do not say what the name
/// is. A quoted phrase is plain text whatever it holds; with an operator in front
/// (`!"old copy"`, `folder:"my docs"`) it is not.
fn plain_term(query: &str) -> String {
    terms(query)
        .find_map(|term| {
            if term.starts_with('"') {
                Some(term.trim_matches('"'))
            } else if term.contains([':', '*', '?', '\\', '/', '|', '!', '"']) {
                None
            } else {
                Some(term)
            }
        })
        .unwrap_or_default()
        .to_lowercase()
}

fn is_launchable(lower_name: &str) -> bool {
    lower_name.ends_with(".lnk") || lower_name.ends_with(".exe")
}

fn depth(v: &VolumeIndex, mut idx: u32) -> u32 {
    let mut depth = 0;
    while idx != 0 && depth < 64 {
        idx = v.entry(idx).parent;
        depth += 1;
    }
    depth
}

/// Name tier, not launchable, hidden, name length, depth. Lower is better.
type Score = (u8, bool, bool, usize, u32);

/// Picks the hits to show, best first: how well the name matches, then programs and shortcuts,
/// then visible items, shorter names and shallower paths. Equal hits keep the search's order.
pub fn rank(index: &Index, hits: &[Hit], query: &str, limit: usize) -> Vec<Hit> {
    let needle = plain_term(query);
    let mut best: Vec<(Score, Hit)> = Vec::with_capacity(limit + 1);
    for &hit in hits.iter().take(RANK_LIMIT) {
        let v = index.volume(hit.vol);
        let name = v.lower_name(hit.idx);
        let attrs = v.entry(hit.idx).attrs;
        let launchable = is_launchable(name);
        // Typing a program's name means that program, more than a file called exactly that.
        let tier = match name_tier(name, &needle) {
            1 if launchable => 0,
            tier => tier,
        };
        let cheap = (tier, !launchable, attrs & (ATTR_HIDDEN | ATTR_SYSTEM) != 0, name.len());
        // The depth takes a walk up the parents, so it is only worked out for contenders.
        if best.len() == limit && best.last().is_some_and(|(s, _)| cheap > (s.0, s.1, s.2, s.3)) {
            continue;
        }
        let score = (cheap.0, cheap.1, cheap.2, cheap.3, depth(v, hit.idx));
        if best.len() == limit && best.last().is_some_and(|(s, _)| score >= *s) {
            continue;
        }
        let at = best.partition_point(|(s, _)| *s <= score);
        best.insert(at, (score, hit));
        best.truncate(limit);
    }
    best.into_iter().map(|(_, hit)| hit).collect()
}

// ─── The bar ────────────────────────────────────────────────────────────────

/// What to do with the selected result.
#[derive(Clone, Copy)]
enum Activate {
    Open,
    Reveal,
    CopyPath,
}

pub struct QuickBar {
    ctx: egui::Context,
    index: Index,
    query: String,
    searcher: Searcher,
    latest_generation: u64,
    needs_search: bool,
    searching: bool,
    /// Snapshot the rows refer to.
    shown: Index,
    rows: Vec<Hit>,
    total: usize,
    selected: usize,
    error: Option<String>,
    signals: Arc<BarSignals>,
    is_open: bool,
    focus_input: bool,
    /// The window has had the focus since it opened. Until then it not being focused says nothing.
    was_focused: bool,
    /// Where "show in FileCraft" sends the query.
    to_main: Sender<String>,
    last_height: f32,
}

impl QuickBar {
    pub fn new(ctx: egui::Context, signals: Arc<BarSignals>, to_main: Sender<String>) -> Self {
        let repaint = ctx.clone();
        QuickBar {
            ctx,
            index: Index::default(),
            query: String::new(),
            searcher: Searcher::with_ranker(
                move || repaint.request_repaint_of(viewport_id()),
                Box::new(|index, hits, text| rank(index, hits, text, MAX_ROWS)),
            ),
            latest_generation: 0,
            needs_search: false,
            searching: false,
            shown: Index::default(),
            rows: Vec::new(),
            total: 0,
            selected: 0,
            error: None,
            signals,
            is_open: false,
            focus_input: false,
            was_focused: false,
            to_main,
            last_height: height(0, false),
        }
    }

    /// A newer index snapshot. While the bar is closed it is only remembered for the next search.
    pub fn set_index(&mut self, index: Index) {
        self.index = index;
        if self.is_open && !self.query.trim().is_empty() {
            self.needs_search = true;
            self.ctx.request_repaint_of(viewport_id());
        }
    }

    fn clear_results(&mut self) {
        self.rows.clear();
        self.total = 0;
        self.selected = 0;
        self.error = None;
        self.searching = false;
        self.shown = Index::default();
        // Results of a search still running are dropped when they arrive.
        self.latest_generation = 0;
    }

    fn close(&mut self) {
        hotkey::hide_bar();
        self.is_open = false;
        self.query.clear();
        self.needs_search = false;
        self.clear_results();
    }

    fn poll(&mut self) {
        while let Ok(res) = self.searcher.results.try_recv() {
            if res.generation != self.latest_generation {
                continue;
            }
            self.searching = false;
            self.error = res.error;
            if self.error.is_none() {
                self.rows = res.top;
                self.total = res.hits.len();
                self.selected = 0;
                self.shown = res.index;
            }
        }
        if std::mem::take(&mut self.needs_search) {
            if self.query.trim().is_empty() {
                self.clear_results();
            } else {
                self.searching = true;
                self.latest_generation =
                    self.searcher.submit(self.index.clone(), &self.query, SearchOptions::default(), Sort::default());
            }
        }
    }

    fn activate(&mut self, action: Activate, ctx: &egui::Context) {
        let Some(&hit) = self.rows.get(self.selected) else { return };
        let full = self.shown.volume(hit.vol).full_path(hit.idx);
        let result = match action {
            Activate::CopyPath => {
                ctx.copy_text(full);
                Ok(())
            }
            // The shell would act on a different item for these, as in the main window (FC-15).
            _ if win32_alias(&full) => Err(std::io::Error::other(
                "the name ends in a dot or space or is a device name, so Windows would open a different item",
            )),
            Activate::Open => actions::open(&PathBuf::from(&full)),
            Activate::Reveal => actions::reveal(&PathBuf::from(&full)),
        };
        match result {
            Ok(()) => self.close(),
            Err(e) => self.error = Some(format!("Failed: {e}")),
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (mut close, mut to_main, mut action) = (false, false, None);
        let mut nav = 0isize;
        ctx.input_mut(|i| {
            close = i.consume_key(Modifiers::NONE, Key::Escape);
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                nav = 1;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                nav = -1;
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::C) {
                action = Some(Activate::CopyPath);
            } else if i.consume_key(Modifiers::COMMAND, Key::Enter) {
                action = Some(Activate::Reveal);
            } else if i.consume_key(Modifiers::SHIFT, Key::Enter) {
                to_main = true;
            } else if i.consume_key(Modifiers::NONE, Key::Enter) {
                action = Some(Activate::Open);
            }
        });
        if nav != 0 && !self.rows.is_empty() {
            self.selected = (self.selected as isize + nav).rem_euclid(self.rows.len() as isize) as usize;
        }
        if close {
            self.close();
        } else if to_main {
            let _ = self.to_main.send(self.query.clone());
            ctx.request_repaint_of(egui::ViewportId::ROOT);
            self.close();
        } else if let Some(action) = action {
            self.activate(action, ctx);
        }
    }

    fn row(&mut self, ui: &mut egui::Ui, i: usize, hit: Hit) -> bool {
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_HEIGHT), egui::Sense::click());
        // Only a moving pointer selects: the bar may open with the pointer resting over a row.
        if resp.hovered() && ui.input(|inp| inp.pointer.delta() != egui::Vec2::ZERO) {
            self.selected = i;
        }
        let visuals = ui.visuals();
        if self.selected == i {
            ui.painter().rect_filled(rect, 4.0, visuals.selection.bg_fill);
        }
        let (strong, weak) = if self.selected == i {
            (visuals.selection.stroke.color, visuals.selection.stroke.color.gamma_multiply(0.75))
        } else {
            (visuals.strong_text_color(), visuals.weak_text_color())
        };
        let v = self.shown.volume(hit.vol);
        let icon = if v.entry(hit.idx).is_dir() { "🗀" } else { "🗋" };
        let painter = ui.painter().with_clip_rect(rect.shrink2(egui::vec2(8.0, 0.0)));
        let left = rect.left() + 10.0;
        painter.text(egui::pos2(left, rect.center().y), Align2::LEFT_CENTER, icon, FontId::proportional(20.0), weak);
        let text_left = left + 30.0;
        painter.text(
            egui::pos2(text_left, rect.top() + 13.0),
            Align2::LEFT_CENTER,
            v.name(hit.idx),
            FontId::proportional(15.0),
            strong,
        );
        painter.text(
            egui::pos2(text_left, rect.top() + 31.0),
            Align2::LEFT_CENTER,
            v.parent_path(hit.idx),
            FontId::proportional(11.5),
            weak,
        );
        resp.clicked()
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Alt+F4 and the like: eframe leaves closing a secondary window to the viewport's owner.
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if self.signals.close.swap(false, Ordering::AcqRel) || close_requested {
            self.close();
        }
        if self.signals.open.swap(false, Ordering::AcqRel) {
            self.is_open = true;
            self.focus_input = true;
            self.was_focused = false;
        }
        match ctx.input(|i| i.viewport().focused) {
            Some(true) => self.was_focused = true,
            // Clicking elsewhere dismisses the bar.
            Some(false) if self.was_focused && self.is_open => self.close(),
            _ => {}
        }
        self.poll();
        if self.is_open {
            self.handle_keys(&ctx);
        }

        let mut clicked = None;
        let frame = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(10, 0));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.allocate_ui(egui::vec2(ui.available_width(), INPUT_HEIGHT), |ui| {
                ui.set_min_height(INPUT_HEIGHT);
                ui.centered_and_justified(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Search files, folders and programs…")
                            .font(FontId::proportional(20.0))
                            .frame(egui::Frame::NONE)
                            .vertical_align(egui::Align::Center),
                    );
                    if std::mem::take(&mut self.focus_input) || !resp.has_focus() {
                        resp.request_focus();
                    }
                    if resp.changed() {
                        self.needs_search = true;
                        ctx.request_repaint();
                    }
                });
            });
            let rows = self.rows.clone();
            for (i, hit) in rows.into_iter().enumerate() {
                if self.row(ui, i, hit) {
                    clicked = Some(i);
                }
            }
            if self.has_footer() {
                ui.allocate_ui(egui::vec2(ui.available_width(), FOOTER_HEIGHT), |ui| {
                    ui.set_min_height(FOOTER_HEIGHT);
                    ui.centered_and_justified(|ui| {
                        ui.label(self.footer(ui.visuals()));
                    });
                });
            }
        });
        if let Some(i) = clicked {
            self.selected = i;
            self.activate(Activate::Open, &ctx);
        }

        let wanted = height(self.rows.len(), self.has_footer());
        if wanted != self.last_height {
            self.last_height = wanted;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(WIDTH, wanted)));
        }
    }

    fn has_footer(&self) -> bool {
        !self.query.trim().is_empty()
    }

    fn footer(&self, visuals: &egui::Visuals) -> RichText {
        if let Some(err) = &self.error {
            return RichText::new(err).small().color(visuals.error_fg_color);
        }
        let text = if !self.rows.is_empty() {
            format!(
                "{} results   ·   Enter open   ·   Ctrl+Enter show in folder   ·   Shift+Enter all results",
                group_digits(self.total)
            )
        } else if self.searching {
            "Searching…".to_string()
        } else if self.index.volumes.is_empty() {
            "Indexing drives…".to_string()
        } else {
            "No results".to_string()
        };
        RichText::new(text).small().weak()
    }
}

/// Keeps the bar's window alive. Called on every pass of the main window; the window itself stays
/// hidden until the hotkey shows it.
pub fn register(ctx: &egui::Context, bar: &Arc<Mutex<QuickBar>>) {
    let builder = egui::ViewportBuilder::default()
        .with_title(TITLE)
        .with_inner_size([WIDTH, height(0, false)])
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top()
        .with_taskbar(false)
        .with_visible(false);
    let bar = Arc::clone(bar);
    ctx.show_viewport_deferred(viewport_id(), builder, move |ui, _class| {
        bar.lock().unwrap().ui(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ATTR_DIRECTORY, Source, VolumeBuilder};
    use std::time::Duration;

    #[test]
    fn plain_term_reads_quoted_phrases() {
        assert_eq!(plain_term("\"Visual Studio\" ext:exe"), "visual studio");
        assert_eq!(plain_term("ext:exe \"Visual Studio\""), "visual studio");
        // Inside the quotes everything is literal text.
        assert_eq!(plain_term("\"a:b\""), "a:b");
        // Still being typed.
        assert_eq!(plain_term("\"visual stu"), "visual stu");
    }

    #[test]
    fn quoted_queries_rank_by_name() {
        let names = [("my visual studio notes", 0), ("visual studio", 0), ("visual studio.exe", 0)];
        assert_eq!(
            ranked(&names, "\"visual studio\"", 10),
            ["visual studio.exe", "visual studio", "my visual studio notes"]
        );
    }

    #[test]
    fn name_tiers_prefer_closer_matches() {
        assert_eq!(name_tier("code", "code"), 0);
        assert_eq!(name_tier("code.exe", "code"), 1);
        assert_eq!(name_tier("codec.dll", "code"), 2);
        assert_eq!(name_tier("visual code.txt", "code"), 3);
        assert_eq!(name_tier("my_code.txt", "code"), 3);
        assert_eq!(name_tier("barcode.txt", "code"), 4);
        assert_eq!(name_tier("other.txt", "code"), 5);
        assert_eq!(name_tier("anything", ""), 5);
    }

    #[test]
    fn plain_term_skips_operators() {
        assert_eq!(plain_term("Report ext:pdf"), "report");
        assert_eq!(plain_term("ext:pdf Report"), "report");
        assert_eq!(plain_term("!draft *.rs src\\main a|b Notes"), "notes");
        assert_eq!(plain_term("!\"old copy\" folder:\"my docs\" Notes"), "notes");
        assert_eq!(plain_term("ext:pdf"), "");
        assert_eq!(plain_term("  "), "");
    }

    fn index(names: &[(&str, u32)]) -> (Index, Vec<Hit>) {
        let mut b = VolumeBuilder::new('C');
        let deep = b.push("deep", 0, ATTR_DIRECTORY, 0, 0);
        let mut ids = Vec::new();
        for &(name, attrs) in names {
            // A name starting with `~` goes one folder deeper, under the name after it.
            let (name, parent) = name.strip_prefix('~').map_or((name, 0), |n| (n, deep));
            ids.push(b.push(name, parent, attrs, 0, 0));
        }
        let v = b.finish(Source::Walk, Duration::ZERO);
        let index = Index::default().with_volume(Arc::new(v));
        let hits = ids.into_iter().map(|idx| Hit { vol: 0, idx }).collect();
        (index, hits)
    }

    fn ranked(names: &[(&str, u32)], query: &str, limit: usize) -> Vec<String> {
        let (index, hits) = index(names);
        rank(&index, &hits, query, limit).iter().map(|h| index.volume(h.vol).name(h.idx).to_string()).collect()
    }

    #[test]
    fn ranks_closer_names_first() {
        let names = [("barcode.txt", 0), ("codec.dll", 0), ("code.exe", 0), ("my code.txt", 0), ("code", 0)];
        assert_eq!(ranked(&names, "CODE", 10), ["code.exe", "code", "codec.dll", "my code.txt", "barcode.txt"]);
    }

    /// Git for Windows ships an extensionless `notepad` that must not beat the real program.
    #[test]
    fn a_program_beats_a_file_named_exactly_like_it() {
        let names = [("notepad", 0), ("notepad.exe", 0), ("notepad.txt", 0)];
        assert_eq!(ranked(&names, "notepad", 10), ["notepad.exe", "notepad", "notepad.txt"]);
    }

    #[test]
    fn programs_and_shortcuts_beat_other_files() {
        let names = [("notes.txt", 0), ("notes.lnk", 0), ("notes.exe", 0)];
        assert_eq!(ranked(&names, "notes", 10), ["notes.lnk", "notes.exe", "notes.txt"]);
    }

    #[test]
    fn hidden_items_and_deep_paths_come_later() {
        let names = [("a.txt", ATTR_HIDDEN), ("~b.txt", 0), ("c.txt", 0), ("d.txt", ATTR_SYSTEM)];
        assert_eq!(ranked(&names, "txt", 10), ["c.txt", "b.txt", "a.txt", "d.txt"]);
    }

    #[test]
    fn equal_hits_keep_the_search_order() {
        let names = [("b.txt", 0), ("a.txt", 0), ("c.txt", 0)];
        assert_eq!(ranked(&names, "ext:txt", 10), ["b.txt", "a.txt", "c.txt"]);
    }

    #[test]
    fn the_limit_keeps_the_best() {
        let names = [("zz code", 0), ("code b", 0), ("code", 0), ("code a", 0)];
        assert_eq!(ranked(&names, "code", 2), ["code", "code b"]);
        assert_eq!(ranked(&names, "code", 0), Vec::<String>::new());
    }
}
