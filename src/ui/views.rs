//! Month, week/day (time grid) and agenda views, all painted directly.

use super::*;
use chrono::{TimeZone, Timelike, Weekday};
use material_3_ui::egui::{pos2, vec2, Align2, CornerRadius, FontId, Rect, Sense, Stroke, StrokeKind};
use material_3_ui::theme::palette::blend;

const HEADER_H: f32 = 30.0;

/// A rounded event chip: filled with the calendar color for all-day events,
/// a colored dot and time for timed ones.
fn chip(app: &mut App, ui: &mut egui::Ui, rect: Rect, o: &Occurrence, filled: bool, id: egui::Id) {
    let p = widgets::palette(ui);
    let color = hex(&o.color);
    let resp = ui.interact(rect, id, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    let font = FontId::proportional(12.0);
    if filled {
        let fill = if resp.hovered() { blend(color, Color32::WHITE, 0.12) } else { color };
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        let g = widgets::truncated_galley(ui, &o.title, widgets::title_font(12.0), text_on(fill), rect.width() - 10.0);
        ui.painter().galley(pos2(rect.left() + 6.0, rect.center().y - g.size().y / 2.0), g, text_on(fill));
    } else {
        if resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(6), p.on_surface.gamma_multiply(0.08));
        }
        ui.painter().circle_filled(pos2(rect.left() + 7.0, rect.center().y), 3.5, color);
        // Narrow cells drop the time.
        let time = if rect.width() >= 120.0 { fmt_time(o.start, app.settings.time_24h) } else { String::new() };
        let tg = ui.painter().layout_no_wrap(time, font.clone(), p.on_surface_variant);
        let tw = tg.size().x;
        ui.painter().galley(pos2(rect.left() + 15.0, rect.center().y - tg.size().y / 2.0), tg, p.on_surface_variant);
        let gap = if tw > 0.0 { 4.0 } else { 0.0 };
        let g = widgets::truncated_galley(ui, &o.title, widgets::title_font(12.0), p.on_surface, rect.width() - 18.0 - tw - gap);
        ui.painter().galley(pos2(rect.left() + 15.0 + tw + gap, rect.center().y - g.size().y / 2.0), g, p.on_surface);
    }
    let resp = resp.on_hover_ui(|ui| tooltip(app, ui, o));
    if resp.clicked() {
        app.open_occurrence(o);
    }
}

fn tooltip(app: &App, ui: &mut egui::Ui, o: &Occurrence) {
    ui.label(widgets::title_text(&o.title, 14.0));
    ui.label(when_text(o, app.settings.time_24h));
    if !o.location.is_empty() {
        ui.label(format!("📍 {}", o.location));
    }
    ui.weak(&o.calendar);
}

pub fn when_text(o: &Occurrence, h24: bool) -> String {
    if o.all_day {
        let days = occurrence_days(o);
        match (days.first(), days.last()) {
            (Some(a), Some(b)) if a != b => format!("{} – {}", a.format("%a %e %b"), b.format("%a %e %b")),
            (Some(a), _) => format!("{}, all day", a.format("%A %e %B")),
            _ => String::new(),
        }
    } else {
        let a = event_date(o.start, false);
        let b = event_date(o.end, false);
        if a == b || o.end == o.start {
            format!("{}, {} – {}", a.format("%a %e %b"), fmt_time(o.start, h24), fmt_time(o.end, h24))
        } else {
            format!("{} {} – {} {}", a.format("%a %e %b"), fmt_time(o.start, h24), b.format("%a %e %b"), fmt_time(o.end, h24))
        }
    }
}

pub fn month(app: &mut App, ui: &mut egui::Ui) {
    let p = widgets::palette(ui);
    let ws = app.settings.week_start;
    let start = month_grid_start(app.cursor, ws.weekday());
    let from = local_midnight(start);
    let to = local_midnight(start + Duration::days(42));
    let occ = app.occurrences(from, to);
    let mut by_day: std::collections::HashMap<NaiveDate, Vec<Occurrence>> = Default::default();
    for o in &occ {
        for d in occurrence_days(o) {
            by_day.entry(d).or_default().push(o.clone());
        }
    }
    let today = Local::now().date_naive();
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, Sense::hover());
    let week_col = if app.settings.show_week_numbers { 28.0 } else { 0.0 };
    let cw = (rect.width() - week_col) / 7.0;
    let ch = (rect.height() - HEADER_H) / 6.0;
    let painter = ui.painter_at(rect);
    for (i, name) in weekday_names(ws.weekday()).iter().enumerate() {
        painter.text(
            pos2(rect.left() + week_col + cw * (i as f32 + 0.5), rect.top() + HEADER_H / 2.0),
            Align2::CENTER_CENTER,
            name.to_uppercase(),
            widgets::title_font(11.5),
            p.on_surface_variant,
        );
    }
    let line = Stroke::new(1.0, p.outline_variant.gamma_multiply(0.6));
    for row in 0..6 {
        let y = rect.top() + HEADER_H + ch * row as f32;
        painter.hline(rect.left() + week_col..=rect.right(), y, line);
        if week_col > 0.0 {
            let d = start + Duration::days(row * 7);
            painter.text(pos2(rect.left() + week_col / 2.0, y + 14.0), Align2::CENTER_CENTER, d.iso_week().week().to_string(), FontId::proportional(11.0), p.outline);
        }
    }
    for col in 1..7 {
        let x = rect.left() + week_col + cw * col as f32;
        painter.vline(x, rect.top() + HEADER_H..=rect.bottom(), line);
    }
    for i in 0..42 {
        let day = start + Duration::days(i);
        let (row, col) = (i / 7, i % 7);
        let cell = Rect::from_min_size(
            pos2(rect.left() + week_col + cw * col as f32, rect.top() + HEADER_H + ch * row as f32),
            vec2(cw, ch),
        );
        let in_month = day.month() == app.cursor.month();
        let cell_resp = ui.interact(cell, egui::Id::unique(("cell", i)), Sense::click());
        if cell_resp.hovered() {
            ui.painter().rect_filled(cell.shrink(1.0), CornerRadius::same(8), p.on_surface.gamma_multiply(0.04));
        }
        if day == app.cursor {
            ui.painter().rect_stroke(cell.shrink(2.0), CornerRadius::same(10), Stroke::new(1.5, p.primary.gamma_multiply(0.5)), StrokeKind::Inside);
        }
        // Day number.
        let num_c = pos2(cell.left() + 18.0, cell.top() + 16.0);
        let (bg, fg) = if day == today {
            (p.primary, p.on_primary)
        } else if in_month {
            (Color32::TRANSPARENT, p.on_surface)
        } else {
            (Color32::TRANSPARENT, p.on_surface_variant.gamma_multiply(0.5))
        };
        if bg != Color32::TRANSPARENT {
            ui.painter().circle_filled(num_c, 12.0, bg);
        }
        let label = if day.day() == 1 { day.format("%e %b").to_string().trim().to_string() } else { day.day().to_string() };
        let num_pos = if day.day() == 1 && day != today { pos2(cell.left() + 8.0, num_c.y) } else { num_c };
        let align = if day.day() == 1 && day != today { Align2::LEFT_CENTER } else { Align2::CENTER_CENTER };
        ui.painter().text(num_pos, align, label, widgets::title_font(12.5), fg);

        let list = by_day.remove(&day).unwrap_or_default();
        let row_h = 20.0;
        let top = cell.top() + 32.0;
        let fits = (((cell.bottom() - 4.0 - top) / row_h).floor() as usize).max(1);
        let shown = if list.len() > fits { fits.saturating_sub(1) } else { list.len() };
        for (k, o) in list.iter().take(shown).enumerate() {
            let r = Rect::from_min_size(pos2(cell.left() + 3.0, top + row_h * k as f32), vec2(cw - 6.0, row_h - 2.0));
            chip(app, ui, r, o, o.all_day, egui::Id::unique(("m", i, k)));
        }
        if list.len() > shown {
            let r = Rect::from_min_size(pos2(cell.left() + 3.0, top + row_h * shown as f32), vec2(cw - 6.0, row_h - 2.0));
            let more = ui.interact(r, egui::Id::unique(("more", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            ui.painter().text(
                pos2(r.left() + 6.0, r.center().y),
                Align2::LEFT_CENTER,
                format!("{} more", list.len() - shown),
                widgets::title_font(12.0),
                if more.hovered() { p.primary } else { p.on_surface_variant },
            );
            if more.clicked() {
                app.cursor = day;
                app.set_view(ViewKind::Day);
            }
        }
        if cell_resp.double_clicked() {
            app.new_event_at(day, Some(9 * 60));
        } else if cell_resp.clicked() {
            app.cursor = day;
            app.mini_month = day.with_day(1).unwrap();
        }
    }
}

/// Lays out overlapping timed events in columns: (column, columns).
fn columns(spans: &[(i64, i64)]) -> Vec<(usize, usize)> {
    let mut out = vec![(0, 1); spans.len()];
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| (spans[i].0, -(spans[i].1)));
    let mut cluster: Vec<usize> = vec![];
    let mut cluster_end = i64::MIN;
    let mut col_ends: Vec<i64> = vec![];
    let flush = |cluster: &mut Vec<usize>, col_ends: &mut Vec<i64>, out: &mut Vec<(usize, usize)>| {
        let n = col_ends.len().max(1);
        for &i in cluster.iter() {
            out[i].1 = n;
        }
        cluster.clear();
        col_ends.clear();
    };
    for i in order {
        let (s, e) = spans[i];
        let e = e.max(s + 15 * 60);
        if s >= cluster_end && !cluster.is_empty() {
            flush(&mut cluster, &mut col_ends, &mut out);
        }
        let col = match col_ends.iter().position(|&end| end <= s) {
            Some(c) => c,
            None => {
                col_ends.push(i64::MIN);
                col_ends.len() - 1
            }
        };
        col_ends[col] = e;
        out[i].0 = col;
        cluster.push(i);
        cluster_end = cluster_end.max(e);
    }
    flush(&mut cluster, &mut col_ends, &mut out);
    out
}

pub fn time_grid(app: &mut App, ui: &mut egui::Ui, days: i64) {
    let p = widgets::palette(ui);
    let first = if days == 7 { week_start(app.cursor, app.settings.week_start.weekday()) } else { app.cursor };
    let from = local_midnight(first);
    let to = local_midnight(first + Duration::days(days));
    let occ = app.occurrences(from, to);
    let today = Local::now().date_naive();
    let gutter = 56.0;
    let full = ui.available_rect_before_wrap();
    let col_w = (full.width() - gutter - 8.0) / days as f32;
    let h24 = app.settings.time_24h;

    // Day headers.
    let (hrect, _) = ui.allocate_exact_size(vec2(full.width(), 52.0), Sense::hover());
    for i in 0..days {
        let d = first + Duration::days(i);
        let x = hrect.left() + gutter + col_w * (i as f32 + 0.5);
        let r = Rect::from_center_size(pos2(x, hrect.center().y), vec2(col_w - 4.0, 50.0));
        let resp = ui.interact(r, egui::Id::unique(("dh", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let is_today = d == today;
        ui.painter().text(pos2(x, hrect.top() + 12.0), Align2::CENTER_CENTER, d.format("%a").to_string().to_uppercase(), widgets::title_font(11.0), if is_today { p.primary } else { p.on_surface_variant });
        let c = pos2(x, hrect.top() + 34.0);
        if is_today {
            ui.painter().circle_filled(c, 15.0, p.primary);
        } else if resp.hovered() {
            ui.painter().circle_filled(c, 15.0, p.on_surface.gamma_multiply(0.08));
        }
        ui.painter().text(c, Align2::CENTER_CENTER, d.day().to_string(), widgets::title_font(17.0), if is_today { p.on_primary } else { p.on_surface });
        if resp.clicked() && days > 1 {
            app.cursor = d;
            app.set_view(ViewKind::Day);
        }
    }

    // All-day row.
    let mut all_day: Vec<Vec<Occurrence>> = vec![vec![]; days as usize];
    for o in occ.iter().filter(|o| o.all_day) {
        for d in occurrence_days(o) {
            let i = (d - first).num_days();
            if (0..days).contains(&i) {
                all_day[i as usize].push(o.clone());
            }
        }
    }
    let rows = all_day.iter().map(Vec::len).max().unwrap_or(0).min(4);
    let ad_h = (rows as f32 * 22.0 + 6.0).max(26.0);
    let (arect, aresp) = ui.allocate_exact_size(vec2(full.width(), ad_h), Sense::click());
    ui.painter().text(pos2(arect.left() + gutter - 8.0, arect.top() + 13.0), Align2::RIGHT_CENTER, "all day", FontId::proportional(10.5), p.on_surface_variant);
    for (i, list) in all_day.iter().enumerate() {
        for (k, o) in list.iter().take(4).enumerate() {
            let r = Rect::from_min_size(pos2(arect.left() + gutter + col_w * i as f32 + 2.0, arect.top() + 3.0 + 22.0 * k as f32), vec2(col_w - 4.0, 20.0));
            chip(app, ui, r, o, true, egui::Id::unique(("ad", i, k)));
        }
    }
    if aresp.double_clicked() {
        if let Some(pos) = aresp.interact_pointer_pos() {
            let i = ((pos.x - arect.left() - gutter) / col_w).floor() as i64;
            if (0..days).contains(&i) {
                app.new_event_at(first + Duration::days(i), None);
            }
        }
    }
    ui.painter().hline(full.left()..=full.right(), arect.bottom(), Stroke::new(1.0, p.outline_variant));

    // The hours.
    let hour_h = 52.0;
    let mut scroll = egui::ScrollArea::vertical().id_salt(("grid", days)).auto_shrink([false, false]);
    if app.scroll_to_hour {
        app.scroll_to_hour = false;
        let now = Local::now();
        let hour = if (first..first + Duration::days(days)).contains(&today) { now.hour().saturating_sub(1) } else { 7 };
        scroll = scroll.vertical_scroll_offset(hour as f32 * hour_h);
    }
    scroll.show(ui, |ui| {
        let (grid, gresp) = ui.allocate_exact_size(vec2(full.width() - 8.0, hour_h * 24.0), Sense::click());
        let painter = ui.painter_at(grid.expand(1.0));
        for h in 0..24 {
            let y = grid.top() + hour_h * h as f32;
            painter.hline(grid.left() + gutter..=grid.right(), y, Stroke::new(1.0, p.outline_variant.gamma_multiply(0.5)));
            if h > 0 {
                painter.text(pos2(grid.left() + gutter - 8.0, y), Align2::RIGHT_CENTER, fmt_hour(h, h24), FontId::proportional(10.5), p.on_surface_variant);
            }
        }
        for i in 0..=days {
            let x = grid.left() + gutter + col_w * i as f32;
            painter.vline(x, grid.top()..=grid.bottom(), Stroke::new(1.0, p.outline_variant.gamma_multiply(0.5)));
        }
        // Weekends are tinted.
        for i in 0..days {
            let d = first + Duration::days(i);
            if days > 1 && matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
                let r = Rect::from_min_size(pos2(grid.left() + gutter + col_w * i as f32, grid.top()), vec2(col_w, grid.height()));
                painter.rect_filled(r, CornerRadius::ZERO, p.on_surface.gamma_multiply(0.025));
            }
        }
        // Events.
        for i in 0..days {
            let d = first + Duration::days(i);
            let (d0, d1) = (local_midnight(d), local_midnight(d + Duration::days(1)));
            let list: Vec<&Occurrence> = occ.iter().filter(|o| !o.all_day && o.start < d1 && (o.end > d0 || (o.start == o.end && o.start >= d0))).collect();
            let spans: Vec<(i64, i64)> = list.iter().map(|o| (o.start.max(d0), o.end.min(d1))).collect();
            let cols = columns(&spans);
            let x0 = grid.left() + gutter + col_w * i as f32;
            // Wall-clock hours, so DST days still line up with the labels.
            let hours = |t: i64| -> f32 {
                if t <= d0 {
                    0.0
                } else if t >= d1 {
                    24.0
                } else {
                    let l = Local.timestamp_opt(t, 0).single().unwrap_or_default();
                    l.hour() as f32 + l.minute() as f32 / 60.0
                }
            };
            for (k, o) in list.iter().enumerate() {
                let (s, e) = spans[k];
                let y0 = grid.top() + hours(s) * hour_h;
                let y1 = (grid.top() + hours(e) * hour_h).max(y0 + 20.0);
                let (c, n) = cols[k];
                let w = (col_w - 6.0) / n as f32;
                let r = Rect::from_min_max(pos2(x0 + 2.0 + w * c as f32, y0 + 1.0), pos2(x0 + 2.0 + w * (c + 1) as f32 - 2.0, y1 - 1.0));
                let id = egui::Id::unique(("ev", i, k));
                let resp = ui.interact(r, id, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let color = hex(&o.color);
                let fill = blend(p.surface_container_low, color, if resp.hovered() { 0.42 } else { 0.32 });
                painter.rect_filled(r, CornerRadius::same(8), fill);
                painter.rect_filled(Rect::from_min_size(r.min, vec2(4.0, r.height())), CornerRadius { nw: 8, sw: 8, ne: 0, se: 0 }, color);
                let fg = p.on_surface;
                let inner = r.shrink2(vec2(8.0, 3.0));
                let title = widgets::truncated_galley(ui, &o.title, widgets::title_font(12.5), fg, inner.width());
                let th = title.size().y;
                painter.with_clip_rect(r).galley(pos2(inner.left() + 2.0, inner.top()), title, fg);
                if inner.height() > th * 2.0 {
                    let mut sub = format!("{} – {}", fmt_time(o.start, h24), fmt_time(o.end, h24));
                    if !o.location.is_empty() {
                        sub.push_str(&format!(" · {}", o.location));
                    }
                    let g = widgets::truncated_galley(ui, &sub, FontId::proportional(11.0), p.on_surface_variant, inner.width());
                    painter.with_clip_rect(r).galley(pos2(inner.left() + 2.0, inner.top() + th + 1.0), g, p.on_surface_variant);
                }
                let resp = resp.on_hover_ui(|ui| tooltip(app, ui, o));
                if resp.clicked() {
                    app.open_occurrence(o);
                }
            }
        }
        // Now.
        let now = Local::now();
        let ti = (today - first).num_days();
        if (0..days).contains(&ti) {
            let y = grid.top() + (now.hour() as f32 + now.minute() as f32 / 60.0) * hour_h;
            let x0 = grid.left() + gutter + col_w * ti as f32;
            painter.circle_filled(pos2(x0, y), 5.0, p.error);
            painter.hline(x0..=x0 + col_w, y, Stroke::new(2.0, p.error));
        }
        // Click an empty slot to add an event there (snapped to 30 min).
        if gresp.clicked() || gresp.double_clicked() {
            if let Some(pos) = gresp.interact_pointer_pos() {
                let i = ((pos.x - grid.left() - gutter) / col_w).floor() as i64;
                if (0..days).contains(&i) {
                    let minute = (((pos.y - grid.top()) / hour_h * 2.0).floor() as i64 * 30).clamp(0, 23 * 60 + 30);
                    app.new_event_at(first + Duration::days(i), Some(minute));
                }
            }
        }
    });
}

/// Rows grouped under day headers; shared by the agenda and search.
fn day_list(app: &mut App, ui: &mut egui::Ui, occ: Vec<Occurrence>, empty: &str) {
    let p = widgets::palette(ui);
    let today = Local::now().date_naive();
    let h24 = app.settings.time_24h;
    egui::ScrollArea::vertical().id_salt("agenda").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(ui.available_width().min(860.0));
        if occ.is_empty() {
            ui.add_space(40.0);
            material_3_ui::common::empty_hint(ui, empty);
            return;
        }
        let mut last = None;
        for (k, o) in occ.iter().enumerate() {
            let d = event_date(o.start, o.all_day).max(if app.search.is_empty() { app.cursor } else { NaiveDate::MIN });
            if last != Some(d) {
                last = Some(d);
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
                    if d == today {
                        ui.painter().circle_filled(r.center(), 20.0, p.primary);
                    }
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, d.day().to_string(), widgets::title_font(18.0), if d == today { p.on_primary } else { p.on_surface });
                    ui.vertical(|ui| {
                        ui.add_space(3.0);
                        ui.label(widgets::title_text(d.format("%A").to_string(), 14.0).color(if d == today { p.primary } else { p.on_surface }));
                        ui.label(egui::RichText::new(d.format("%B %Y").to_string()).size(11.5).color(p.on_surface_variant));
                    });
                });
            }
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::click());
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(14), p.on_surface.gamma_multiply(0.06));
            }
            let color = hex(&o.color);
            ui.painter().rect_filled(Rect::from_min_size(pos2(r.left() + 52.0, r.top() + 10.0), vec2(4.0, 32.0)), CornerRadius::same(2), color);
            let when = if o.all_day { "All day".to_string() } else { format!("{} – {}", fmt_time(o.start, h24), fmt_time(o.end, h24)) };
            ui.painter().text(pos2(r.left() + 68.0, r.top() + 17.0), Align2::LEFT_CENTER, &o.title, widgets::title_font(14.5), p.on_surface);
            let mut sub = when;
            if !o.location.is_empty() {
                sub.push_str(&format!("  ·  {}", o.location));
            }
            sub.push_str(&format!("  ·  {}", o.calendar));
            if o.recurring {
                sub.push_str("  ·  repeats");
            }
            let g = widgets::truncated_galley(ui, &sub, FontId::proportional(12.5), p.on_surface_variant, r.width() - 80.0);
            ui.painter().galley(pos2(r.left() + 68.0, r.top() + 28.0), g, p.on_surface_variant);
            if resp.clicked() {
                app.open_occurrence(&occ[k]);
            }
        }
        ui.add_space(24.0);
    });
}

pub fn agenda(app: &mut App, ui: &mut egui::Ui) {
    let from = local_midnight(app.cursor);
    let to = local_midnight(app.cursor + Duration::days(60));
    let occ = app.occurrences(from, to);
    day_list(app, ui, occ, "Nothing in the next 60 days. Press N to add an event.");
}

pub fn search(app: &mut App, ui: &mut egui::Ui) {
    let today = Local::now().date_naive();
    let from = local_midnight(today - Duration::days(365));
    let to = local_midnight(today + Duration::days(730));
    let occ: Vec<Occurrence> = app.store.search(app.search.trim(), from, to).unwrap_or_default().into_iter().take(300).collect();
    day_list(app, ui, occ, "No events match.");
}

#[cfg(test)]
mod tests {
    #[test]
    fn overlap_columns() {
        let c = super::columns(&[(0, 3600), (1800, 5400), (3600, 7200), (9000, 9600)]);
        assert_eq!(c[0], (0, 2));
        assert_eq!(c[1], (1, 2));
        assert_eq!(c[2], (0, 2));
        assert_eq!(c[3], (0, 1));
    }
}
