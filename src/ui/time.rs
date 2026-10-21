//! Date math and small formatting helpers for the views. Timed events are
//! shown in local time; all-day events by their calendar dates.

use crate::config::WeekStart;
use crate::model::Occurrence;
use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use material_3_ui::egui::Color32;

pub use crate::cli::local_midnight;

pub fn utc_midnight(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()
}

/// The local date an event starts on.
pub fn event_date(ts: i64, all_day: bool) -> NaiveDate {
    if all_day {
        chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default().date_naive()
    } else {
        Local.timestamp_opt(ts, 0).single().map(|d| d.date_naive()).unwrap_or_default()
    }
}

/// Every day an occurrence touches.
pub fn occurrence_days(o: &Occurrence) -> Vec<NaiveDate> {
    let first = event_date(o.start, o.all_day);
    // Ends are exclusive: an event ending at midnight doesn't touch that day.
    let last = if o.end > o.start { event_date(o.end - 1, o.all_day) } else { first };
    let mut days = vec![];
    let mut d = first;
    while d <= last && days.len() < 62 {
        days.push(d);
        d += Duration::days(1);
    }
    days
}

pub fn add_months(d: NaiveDate, n: i32) -> NaiveDate {
    let total = d.year() * 12 + d.month0() as i32 + n;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let mut day = d.day();
    loop {
        if let Some(x) = NaiveDate::from_ymd_opt(y, m, day) {
            return x;
        }
        day -= 1;
    }
}

pub fn week_start(d: NaiveDate, ws: WeekStart) -> NaiveDate {
    let first = ws.weekday().num_days_from_monday() as i64;
    let back = (d.weekday().num_days_from_monday() as i64 - first).rem_euclid(7);
    d - Duration::days(back)
}

/// [first day, day after last) of the week containing `d`.
pub fn week_range(d: NaiveDate, ws: WeekStart) -> (NaiveDate, NaiveDate) {
    let a = week_start(d, ws);
    (a, a + Duration::days(7))
}

/// The six weeks a month view shows.
pub fn month_grid_start(d: NaiveDate, ws: WeekStart) -> NaiveDate {
    week_start(d.with_day(1).unwrap(), ws)
}

pub fn hex(c: &str) -> Color32 {
    material_3_ui::theme::palette::parse_hex(c).unwrap_or(Color32::from_rgb(0x42, 0x85, 0xF4))
}

/// Black or white, whichever reads better on `bg`.
pub fn text_on(bg: Color32) -> Color32 {
    let l = 0.2126 * bg.r() as f32 + 0.7152 * bg.g() as f32 + 0.0722 * bg.b() as f32;
    if l > 150.0 { Color32::from_rgb(0x1b, 0x1b, 0x1f) } else { Color32::WHITE }
}

pub fn fmt_time(ts: i64, h24: bool) -> String {
    let t = Local.timestamp_opt(ts, 0).single().unwrap_or_default();
    if h24 {
        t.format("%H:%M").to_string()
    } else if t.format("%M").to_string() == "00" {
        t.format("%-I%P").to_string()
    } else {
        t.format("%-I:%M%P").to_string()
    }
}

pub fn fmt_hour(h: u32, h24: bool) -> String {
    if h24 {
        format!("{h:02}:00")
    } else {
        match h {
            0 => "12 AM".into(),
            12 => "12 PM".into(),
            h if h < 12 => format!("{h} AM"),
            h => format!("{} PM", h - 12),
        }
    }
}

pub fn weekday_names(ws: WeekStart) -> [String; 7] {
    let start = week_start(NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(), ws);
    std::array::from_fn(|i| (start + Duration::days(i as i64)).format("%a").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_math() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 31).unwrap();
        assert_eq!(add_months(d, 1), NaiveDate::from_ymd_opt(2026, 2, 28).unwrap());
        assert_eq!(add_months(d, -2), NaiveDate::from_ymd_opt(2025, 11, 30).unwrap());
        let wed = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
        assert_eq!(week_start(wed, WeekStart::Monday), NaiveDate::from_ymd_opt(2026, 10, 19).unwrap());
        assert_eq!(week_start(wed, WeekStart::Sunday), NaiveDate::from_ymd_opt(2026, 10, 18).unwrap());
        assert_eq!(week_start(wed, WeekStart::Saturday), NaiveDate::from_ymd_opt(2026, 10, 17).unwrap());
        assert_eq!(weekday_names(WeekStart::Sunday)[0], "Sun");
        let o = Occurrence {
            key: String::new(),
            event_id: String::new(),
            calendar_id: String::new(),
            calendar: String::new(),
            color: String::new(),
            title: String::new(),
            location: String::new(),
            description: String::new(),
            start: utc_midnight(wed),
            end: utc_midnight(wed) + 3 * 86400,
            all_day: true,
            recurring: false,
            reminders: vec![],
        };
        assert_eq!(occurrence_days(&o).len(), 3);
        assert_eq!(text_on(Color32::WHITE), Color32::from_rgb(0x1b, 0x1b, 0x1f));
        assert_eq!(fmt_hour(13, false), "1 PM");
    }
}
