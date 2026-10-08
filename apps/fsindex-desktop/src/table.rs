//! Native virtual table: painting, hit testing and formatting only. No index access.
use crate::theme;
use eframe::egui::{self, pos2, vec2, Align, Color32, FontId, Rect, Sense, Stroke};
use fsindex_core::{query::ResultSnapshot, EntryKind, SearchHit, SortKey};
use std::path::Path;

pub const ROW_HEIGHT: f32 = 24.0;
const HEADER_HEIGHT: f32 = 28.0;

#[derive(Default)]
pub struct Table {
    pub pending_query: bool,
    name_width: Option<f32>,
    pub reveal: Option<usize>,
}

pub enum Action {
    Open(String),
    OpenFolder(String),
    Copy(String),
}

pub struct TableOutput {
    pub scroll: egui::scroll_area::ScrollAreaOutput<()>,
    pub sort: Option<SortKey>,
    pub action: Option<Action>,
}

#[derive(Clone, Copy)]
struct Columns {
    widths: [f32; 5],
}
impl Columns {
    fn new(width: f32, name: Option<f32>) -> Self {
        // Narrow panes retain all five columns; cell text truncates independently.
        if width < 480.0 {
            return Self {
                widths: [
                    width * 0.28,
                    width * 0.22,
                    width * 0.14,
                    width * 0.25,
                    width * 0.11,
                ],
            };
        }
        // The details pane can make the table narrow: keep every column in bounds.
        let modified = if width >= 720.0 { 150.0 } else { 126.0 };
        let fixed = 84.0 + modified + 64.0;
        let name = name
            .unwrap_or(width * 0.30)
            .clamp(120.0, (width - fixed - 90.0).max(120.0));
        Self {
            widths: [name, (width - name - fixed).max(40.0), 84.0, modified, 64.0],
        }
    }
    fn rect(&self, row: Rect, column: usize) -> Rect {
        let left = row.left() + self.widths[..column].iter().sum::<f32>();
        Rect::from_min_size(
            pos2(left, row.top()),
            vec2(self.widths[column], row.height()),
        )
    }
}

impl Table {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: Option<&ResultSnapshot>,
        epoch: u64,
        selected: &mut Option<usize>,
        sort: SortKey,
        desc: bool,
    ) -> TableOutput {
        ui.spacing_mut().item_spacing.y = 0.0;
        let gutter = ui.spacing().scroll.allocated_width();
        let width = ui.available_width() - gutter;
        let columns = Columns::new(width, self.name_width);
        let (header, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), HEADER_HEIGHT), Sense::hover());
        ui.painter().rect_filled(header, 0.0, theme::PANEL);
        let mut next_sort = None;
        for (i, (title, key)) in [
            ("Name", SortKey::Name),
            ("Path", SortKey::Path),
            ("Size", SortKey::Size),
            ("Modified", SortKey::Mtime),
            ("Type", SortKey::Extension),
        ]
        .into_iter()
        .enumerate()
        {
            let cell = columns.rect(header, i);
            // Leave the Name/Path divider available for the resize handle.
            let click_rect = if i == 0 {
                Rect::from_min_max(cell.min, pos2(cell.right() - 4.0, cell.bottom()))
            } else {
                cell
            };
            let response = ui.interact(click_rect, ui.id().with(("sort", i)), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    format!("Sort by {title}"),
                )
            });
            if response.hovered() {
                ui.painter()
                    .rect_filled(cell, 0.0, Color32::from_rgb(47, 52, 57));
            }
            if response.clicked() {
                next_sort = Some(key);
            }
            let label_rect = if sort == key {
                cell.shrink2(vec2(10.0, 0.0))
            } else {
                cell
            };
            text(
                ui,
                label_rect,
                title,
                theme::MUTED,
                if i == 2 { Align::Max } else { Align::Min },
            );
            if sort == key {
                let x = if i == 2 {
                    cell.left() + 10.0
                } else {
                    cell.right() - 9.0
                };
                let y = cell.center().y;
                let sign = if desc { -1.0 } else { 1.0 };
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        pos2(x - 3.0, y + 2.0 * sign),
                        pos2(x + 3.0, y + 2.0 * sign),
                        pos2(x, y - 2.0 * sign),
                    ],
                    theme::MUTED,
                    Stroke::NONE,
                ));
            }
        }
        let divider = header.left() + columns.widths[0];
        let resize = ui
            .interact(
                Rect::from_min_max(
                    pos2(divider - 4.0, header.top()),
                    pos2(divider + 4.0, header.bottom()),
                ),
                ui.id().with("name-resize"),
                Sense::drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if resize.dragged() {
            self.name_width = Some(
                (columns.widths[0] + ui.input(|i| i.pointer.delta().x))
                    .clamp(120.0, (width - 400.0).max(120.0)),
            );
        }
        if resize.double_clicked() {
            self.name_width = None;
        }
        ui.painter().line_segment(
            [
                pos2(divider, header.top() + 6.0),
                pos2(divider, header.bottom() - 6.0),
            ],
            Stroke::new(1.0, theme::BORDER),
        );
        ui.painter().hline(
            header.x_range(),
            header.bottom() - 1.0,
            Stroke::new(1.0, theme::BORDER),
        );
        let mut action = None;
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt(("results", epoch))
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .auto_shrink([false, false]);
        if let Some(row) = self.reveal.take() {
            scroll = scroll.vertical_scroll_offset(row as f32 * ROW_HEIGHT);
        }
        let count = snapshot.map_or(0, |s| s.rows.len());
        let scroll = scroll.show_rows(ui, ROW_HEIGHT, count, |ui, visible| {
            let Some(snapshot) = snapshot else {
                return;
            };
            let columns = Columns::new(ui.available_width(), self.name_width);
            for row in visible {
                let hit = &snapshot.rows[row];
                let (rect, response) = ui.allocate_exact_size(
                    vec2(ui.available_width(), ROW_HEIGHT),
                    if self.pending_query {
                        Sense::hover()
                    } else {
                        Sense::click()
                    },
                );
                let response = response.on_hover_text(&hit.path);
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::SelectableLabel,
                        !self.pending_query,
                        *selected == Some(row),
                        &hit.name,
                    )
                });
                if !self.pending_query && (response.clicked() || response.secondary_clicked()) {
                    *selected = Some(row);
                }
                if !self.pending_query && response.has_focus() {
                    if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        action = Some(Action::Open(hit.path.clone()));
                    }
                    if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C)) {
                        action = Some(Action::Copy(hit.path.clone()));
                    }
                }
                if !self.pending_query && response.double_clicked() {
                    action = Some(Action::Open(hit.path.clone()));
                }
                let fill = if *selected == Some(row) {
                    theme::SELECTED
                } else if response.hovered() {
                    Color32::from_rgb(42, 48, 53)
                } else if row % 2 == 0 {
                    theme::BACKGROUND
                } else {
                    Color32::from_rgb(29, 33, 36)
                };
                ui.painter().rect_filled(rect, 0.0, fill);
                let name = columns.rect(rect, 0);
                file_icon(
                    ui,
                    Rect::from_min_size(
                        pos2(name.left() + 8.0, name.center().y - 7.0),
                        vec2(14.0, 14.0),
                    ),
                    hit,
                );
                text(
                    ui,
                    Rect::from_min_max(pos2(name.left() + 24.0, name.top()), name.max),
                    &hit.name,
                    theme::TEXT,
                    Align::Min,
                );
                let parent = Path::new(&hit.path)
                    .parent()
                    .map(|p| p.to_string_lossy())
                    .unwrap_or_default();
                text(ui, columns.rect(rect, 1), &parent, theme::MUTED, Align::Min);
                let size = if hit.kind == EntryKind::Dir {
                    "—".into()
                } else {
                    size_label(hit.size)
                };
                text(ui, columns.rect(rect, 2), &size, theme::TEXT, Align::Max);
                text(
                    ui,
                    columns.rect(rect, 3),
                    &if hit.depth == 0 && hit.mtime == 0 {
                        "—".into()
                    } else {
                        modified_label(hit.mtime)
                    },
                    theme::TEXT,
                    Align::Min,
                );
                text(
                    ui,
                    columns.rect(rect, 4),
                    &type_label(hit),
                    theme::MUTED,
                    Align::Min,
                );
                if !self.pending_query {
                    response.context_menu(|ui| {
                        if ui.button("Open").clicked() {
                            action = Some(Action::Open(hit.path.clone()));
                            ui.close();
                        }
                        if ui.button("Open containing folder").clicked() {
                            action = Some(Action::OpenFolder(hit.path.clone()));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Copy path").clicked() {
                            action = Some(Action::Copy(hit.path.clone()));
                            ui.close();
                        }
                    });
                }
            }
        });
        TableOutput {
            scroll,
            sort: next_sort,
            action,
        }
    }
}

/// Layout text to the actual cell width; ellipsis and clipping share the same bounds.
fn text(ui: &egui::Ui, cell: Rect, value: &str, color: Color32, align: Align) {
    let bounds = cell.shrink2(vec2(8.0, 0.0));
    if bounds.width() <= 0.0 {
        return;
    }
    let mut job =
        egui::text::LayoutJob::simple_singleline(value.into(), FontId::proportional(13.0), color);
    job.wrap.max_width = bounds.width();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let x = if align == Align::Max {
        bounds.right() - galley.size().x
    } else {
        bounds.left()
    };
    ui.painter()
        .with_clip_rect(ui.clip_rect().intersect(bounds))
        .galley(
            pos2(x, bounds.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
}

fn extension(hit: &SearchHit) -> String {
    Path::new(&hit.name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}
fn type_label(hit: &SearchHit) -> String {
    match hit.kind {
        EntryKind::Dir => "Folder".into(),
        EntryKind::Symlink => "Link".into(),
        _ => {
            let ext = extension(hit);
            if ext.is_empty() {
                "File".into()
            } else {
                ext.to_uppercase()
            }
        }
    }
}
fn file_icon(ui: &egui::Ui, r: Rect, hit: &SearchHit) {
    let p = ui.painter();
    if hit.kind == EntryKind::Dir {
        p.rect_filled(
            Rect::from_min_size(r.min, vec2(7.0, 4.0)),
            1.0,
            Color32::from_rgb(126, 178, 203),
        );
        p.rect_filled(
            Rect::from_min_max(pos2(r.left(), r.top() + 3.0), r.max),
            1.5,
            Color32::from_rgb(159, 203, 224),
        );
        return;
    }
    let ext = extension(hit);
    let (color, mark) = match ext.as_str() {
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "opus" | "aiff" => {
            (Color32::from_rgb(227, 169, 66), "audio")
        }
        "png" | "jpg" | "jpeg" | "webp" | "svg" | "gif" | "ico" => {
            (Color32::from_rgb(82, 173, 177), "image")
        }
        "mp4" | "mkv" | "webm" | "mov" => (Color32::from_rgb(172, 135, 211), "video"),
        "rs" | "js" | "ts" | "py" | "toml" | "json" | "sh" => {
            (Color32::from_rgb(108, 169, 215), "code")
        }
        _ => (Color32::from_rgb(176, 183, 191), "file"),
    };
    p.rect_filled(r.shrink2(vec2(1.0, 0.0)), 1.0, color);
    let ink = Color32::from_rgb(245, 248, 250);
    match mark {
        "audio" => {
            p.line_segment(
                [r.min + vec2(8.0, 3.0), r.min + vec2(8.0, 10.0)],
                Stroke::new(1.5, ink),
            );
            p.line_segment(
                [r.min + vec2(8.0, 3.0), r.min + vec2(11.0, 4.0)],
                Stroke::new(1.5, ink),
            );
            p.circle_filled(r.min + vec2(6.0, 10.0), 2.0, ink);
        }
        "image" => {
            p.circle_filled(r.min + vec2(5.0, 4.0), 1.5, ink);
            p.add(egui::Shape::convex_polygon(
                vec![
                    r.min + vec2(3.0, 11.0),
                    r.min + vec2(7.0, 6.0),
                    r.min + vec2(12.0, 11.0),
                ],
                ink,
                Stroke::NONE,
            ));
        }
        "video" => {
            p.add(egui::Shape::convex_polygon(
                vec![
                    r.min + vec2(5.0, 3.0),
                    r.min + vec2(11.0, 7.0),
                    r.min + vec2(5.0, 11.0),
                ],
                ink,
                Stroke::NONE,
            ));
        }
        _ => {
            for y in [4.0, 7.0, 10.0] {
                p.line_segment(
                    [r.min + vec2(4.0, y), r.min + vec2(10.0, y)],
                    Stroke::new(1.0, ink),
                );
            }
        }
    }
}

pub fn size_label(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}
fn modified_label(seconds: i64) -> String {
    let native_seconds = seconds as libc::time_t;
    if native_seconds as i128 != seconds as i128 {
        return "—".into();
    }
    let seconds = native_seconds;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r writes only to our stack-local tm and returns null on overflow.
    let local = unsafe {
        if libc::localtime_r(&seconds, local.as_mut_ptr()).is_null() {
            return "—".into();
        }
        local.assume_init()
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        i64::from(local.tm_year) + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min
    )
}

pub fn count_label(count: usize) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(c);
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{epaint::Shape, vec2, Rect};
    use fsindex_core::SearchHit;

    fn render(
        ctx: &egui::Context,
        snapshot: &ResultSnapshot,
    ) -> (egui::FullOutput, egui::scroll_area::ScrollAreaOutput<()>) {
        let mut scroll = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.set_max_size(vec2(900.0, 400.0));
            egui::CentralPanel::default().show(ui, |ui| {
                ui.set_max_size(vec2(880.0, 380.0));
                scroll = Some(
                    Table::default()
                        .show(ui, Some(snapshot), 0, &mut None, SortKey::Name, false)
                        .scroll,
                );
            });
        });
        output.textures_delta.clear();
        (output, scroll.unwrap())
    }

    fn visible_text(output: &egui::FullOutput, name: &str, viewport: Rect) -> bool {
        output.shapes.iter().any(|clipped| {
            if let Shape::Text(text) = &clipped.shape {
                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                text.galley.text().contains(name)
                    && bounds.intersects(viewport.intersect(clipped.clip_rect))
            } else {
                false
            }
        })
    }

    #[test]
    fn deep_scroll_renders_last_row_inside_viewport_without_laying_out_all_rows() {
        let ctx = egui::Context::default();
        let snapshot = ResultSnapshot {
            generation: 1,
            rows: (0..100_000)
                .map(|n| {
                    std::sync::Arc::new(SearchHit {
                        name: format!("file-{n:06}"),
                        path: format!("/fixture/file-{n:06}"),
                        volume: 1,
                        volume_name: "fixture".into(),
                        kind: EntryKind::File,
                        size: n,
                        mtime: 0,
                        mtime_ns: 0,
                        inode: n,
                        depth: 1,
                    })
                })
                .collect(),
        };
        // First pass initializes fonts/layout; the following pass is stable.
        render(&ctx, &snapshot);
        let (top, area) = render(&ctx, &snapshot);
        assert!(visible_text(&top, "file-000000", area.inner_rect));
        assert!(
            top.shapes.len() < 500,
            "rendering must scale with visible rows"
        );
        assert!((area.content_size.y - 100_000.0 * ROW_HEIGHT).abs() < 1.0);
        let id = area.id;
        let mut state = area.state;
        state.offset.y = area.content_size.y - area.inner_rect.height();
        state.store(&ctx, id);
        let (bottom, area) = render(&ctx, &snapshot);
        assert!(area.state.offset.y > 2_000_000.0);
        assert!(
            visible_text(&bottom, "file-099999", area.inner_rect),
            "last row must actually be visible"
        );
        assert!(!visible_text(&bottom, "file-000000", area.inner_rect));
        assert!(bottom.shapes.len() < 500);
    }
}

#[cfg(test)]
mod interaction_tests {
    use super::*;

    fn fixture() -> ResultSnapshot {
        ResultSnapshot { generation: 1, rows: vec![std::sync::Arc::new(SearchHit {
            name: "A very long filename that should be clipped before the next column starts.flac".into(),
            path: "/a/very/long/directory/with/many/segments/that/must/not/overlap/size/audio.flac".into(),
            volume: 1, volume_name: "test".into(), kind: EntryKind::File, size: 8388608,
            mtime: 1791385200, mtime_ns: 0, inode: 1, depth: 1,
        })].into() }
    }

    fn frame(
        ctx: &egui::Context,
        table: &mut Table,
        snapshot: &ResultSnapshot,
        selected: &mut Option<usize>,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, TableOutput) {
        let mut result = None;
        let mut output = ctx.run_ui(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ui| {
                ui.set_max_size(vec2(900.0, 400.0));
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.set_max_size(vec2(880.0, 380.0));
                    result =
                        Some(table.show(ui, Some(snapshot), 0, selected, SortKey::Name, false));
                });
            },
        );
        output.textures_delta.clear();
        (output, result.unwrap())
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            },
        ]
    }

    #[test]
    fn columns_align_and_long_text_is_clipped_to_its_cell() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let snapshot = fixture();
        let mut table = Table::default();
        let mut selected = None;
        frame(&ctx, &mut table, &snapshot, &mut selected, vec![]);
        let (output, layout) = frame(&ctx, &mut table, &snapshot, &mut selected, vec![]);
        let texts: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((s.clip_rect, t)),
                _ => None,
            })
            .collect();
        let header = texts
            .iter()
            .find(|(_, t)| t.galley.text() == "Path")
            .unwrap();
        let path = texts
            .iter()
            .find(|(_, t)| t.galley.text().starts_with("/a/very"))
            .unwrap();
        assert!(
            (header.1.pos.x - path.1.pos.x).abs() < 1.0,
            "Path header and rows must align"
        );
        let name = texts
            .iter()
            .find(|(_, t)| t.galley.text().starts_with("A very long"))
            .unwrap();
        assert!(
            name.0.right() < path.1.pos.x,
            "Filename must not paint over the Path cell"
        );
        assert!(
            path.0.right() < layout.scroll.inner_rect.right() - 200.0,
            "Path must leave space for metadata"
        );
        assert!(name.1.galley.size().x <= name.0.width() + 1.0);
    }

    #[test]
    fn path_cell_selects_whole_row_and_size_header_requests_sort() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let snapshot = fixture();
        let mut table = Table::default();
        let mut selected = None;
        frame(&ctx, &mut table, &snapshot, &mut selected, vec![]);
        let (_, layout) = frame(&ctx, &mut table, &snapshot, &mut selected, vec![]);
        let pos = pos2(
            layout.scroll.inner_rect.left() + 400.0,
            layout.scroll.inner_rect.top() + ROW_HEIGHT / 2.0,
        );
        frame(
            &ctx,
            &mut table,
            &snapshot,
            &mut selected,
            pointer(pos, true),
        );
        frame(
            &ctx,
            &mut table,
            &snapshot,
            &mut selected,
            pointer(pos, false),
        );
        assert_eq!(selected, Some(0), "Clicking the path must select the row");
        let columns = Columns::new(layout.scroll.inner_rect.width(), None);
        let header_row = Rect::from_min_size(
            layout.scroll.inner_rect.min - vec2(0.0, HEADER_HEIGHT),
            vec2(layout.scroll.inner_rect.width(), HEADER_HEIGHT),
        );
        let size_pos = columns.rect(header_row, 2).center();
        frame(
            &ctx,
            &mut table,
            &snapshot,
            &mut selected,
            pointer(size_pos, true),
        );
        let (_, result) = frame(
            &ctx,
            &mut table,
            &snapshot,
            &mut selected,
            pointer(size_pos, false),
        );
        assert_eq!(result.sort, Some(SortKey::Size));
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;
    #[test]
    fn previous_rows_remain_painted_but_cannot_be_selected_while_query_is_pending() {
        let ctx = egui::Context::default();
        let snapshot = ResultSnapshot {
            generation: 1,
            rows: vec![std::sync::Arc::new(SearchHit {
                name: "previous.flac".into(),
                path: "/fixture/previous.flac".into(),
                volume: 1,
                volume_name: "test".into(),
                kind: EntryKind::File,
                size: 10,
                mtime: 0,
                mtime_ns: 0,
                inode: 1,
                depth: 1,
            })]
            .into(),
        };
        let mut table = Table {
            pending_query: true,
            ..Default::default()
        };
        let mut selected = None;
        let mut position = pos2(100.0, HEADER_HEIGHT + ROW_HEIGHT / 2.0);
        for pressed in [None, Some(true), Some(false)] {
            let events = pressed
                .map(|pressed| {
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: Default::default(),
                        },
                    ]
                })
                .unwrap_or_default();
            let mut visible = false;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    let out =
                        table.show(ui, Some(&snapshot), 0, &mut selected, SortKey::Name, false);
                    position = pos2(
                        out.scroll.inner_rect.left() + 100.0,
                        out.scroll.inner_rect.top() + ROW_HEIGHT / 2.0,
                    );
                    assert!(out.action.is_none());
                },
            );
            for clipped in &output.shapes {
                if let egui::Shape::Text(text) = &clipped.shape {
                    visible |= text.galley.text() == "previous.flac"
                        && clipped
                            .clip_rect
                            .intersects(text.galley.rect.translate(text.pos.to_vec2()));
                }
            }
            output.textures_delta.clear();
            assert!(visible, "retained rows must actually be painted");
            assert!(
                selected.is_none(),
                "a retained result must not be treated as a new-query match"
            );
        }
    }
}
