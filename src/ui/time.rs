//! Date math and small formatting helpers for the views. Timed events are
//! shown in local time; all-day events by their calendar dates.

use crate::config::WeekStart;
use crate::model::Occurrence;
use chrono::{Duration, Local, NaiveDate, TimeZone};

pub use crate::cli::local_midnight;
pub use material_3_ui::calendar::{add_months, month_grid_start, week_start, weekday_names};
pub use material_3_ui::theme::palette::{hex, text_on};

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

/// [first day, day after last) of the week containing `d`.
pub fn week_range(d: NaiveDate, ws: WeekStart) -> (NaiveDate, NaiveDate) {
    let a = week_start(d, ws.weekday());
    (a, a + Duration::days(7))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_math() {
        let wed = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
        assert_eq!(week_range(wed, WeekStart::Monday).0, NaiveDate::from_ymd_opt(2026, 10, 19).unwrap());
        assert_eq!(week_range(wed, WeekStart::Sunday).0, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap());
        assert_eq!(week_range(wed, WeekStart::Saturday).1, NaiveDate::from_ymd_opt(2026, 10, 24).unwrap());
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
        assert_eq!(fmt_hour(13, false), "1 PM");
    }
}
