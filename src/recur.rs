//! Recurrence expansion with the `rrule` crate. Timed events recur in their
//! own time zone, so a 09:00 meeting stays at 09:00 across DST changes;
//! all-day events recur on calendar days.

use crate::model::Event;
use chrono::{DateTime, TimeZone, Utc};
use rrule::{RRuleSet, Tz};

/// The event's zone for recurrence (UTC for all-day events and unknown ids).
pub fn zone(ev: &Event) -> Tz {
    if ev.all_day || ev.tz.is_empty() {
        return Tz::UTC;
    }
    ev.tz.parse::<chrono_tz::Tz>().map(Tz::from).unwrap_or(Tz::UTC)
}

/// The local IANA zone name ("UTC" when it can't be found).
pub fn local_zone_name() -> String {
    std::env::var("TZ")
        .ok()
        .filter(|z| z.parse::<chrono_tz::Tz>().is_ok())
        .or_else(|| iana_time_zone::get_timezone().ok())
        .unwrap_or_else(|| "UTC".into())
}

/// RFC 5545 requires UNTIL in UTC when DTSTART has a zone; date-only and
/// floating UNTIL values (common in the wild) are made UTC end of day/as is.
fn normalize_rule(rule: &str) -> String {
    let rule = rule.trim().trim_start_matches("RRULE:");
    rule.split(';')
        .filter(|p| !p.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) if k.eq_ignore_ascii_case("UNTIL") => {
                if v.len() == 8 {
                    format!("UNTIL={v}T235959Z")
                } else if !v.ends_with('Z') {
                    format!("UNTIL={v}Z")
                } else {
                    part.to_string()
                }
            }
            _ => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn build(ev: &Event) -> Option<RRuleSet> {
    let rule = ev.rrule.as_deref()?;
    let tz = zone(ev);
    let start = tz.timestamp_opt(ev.start, 0).single()?;
    let dtstart = match tz {
        Tz::Tz(chrono_tz::Tz::UTC) => format!("DTSTART:{}", start.format("%Y%m%dT%H%M%SZ")),
        _ => format!("DTSTART;TZID={}:{}", tz.name(), start.format("%Y%m%dT%H%M%S")),
    };
    let text = format!("{dtstart}\nRRULE:{}", normalize_rule(rule));
    let mut set: RRuleSet = match text.parse() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("bad recurrence {rule:?} for {:?}: {e}", ev.title);
            return None;
        }
    };
    for ex in &ev.exdates {
        if let Some(dt) = tz.timestamp_opt(*ex, 0).single() {
            set = set.exdate(dt);
        }
    }
    Some(set)
}

/// Start/end of each occurrence overlapping `[from, to)`. Events without a
/// rule give themselves.
pub fn expand(ev: &Event, from: i64, to: i64) -> Vec<(i64, i64)> {
    let dur = ev.duration();
    let overlaps = |s: i64| s < to && (s + dur > from || (dur == 0 && s >= from));
    if ev.rrule.is_none() {
        return if overlaps(ev.start) { vec![(ev.start, ev.end)] } else { vec![] };
    }
    let Some(set) = build(ev) else {
        // An unreadable rule still shows the first occurrence.
        return if overlaps(ev.start) { vec![(ev.start, ev.end)] } else { vec![] };
    };
    let tz = zone(ev);
    let utc = |t: i64| Utc.timestamp_opt(t, 0).single().map(|d: DateTime<Utc>| d.with_timezone(&tz));
    let (Some(after), Some(before)) = (utc(from - dur - 1), utc(to)) else { return vec![] };
    set.after(after)
        .before(before)
        .all(5000)
        .dates
        .into_iter()
        .map(|d| d.timestamp())
        .filter(|s| !ev.exdates.contains(s) && overlaps(*s))
        .map(|s| (s, s + dur))
        .collect()
}

/// Common repeat choices for the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    None,
    Daily,
    Weekdays,
    Weekly,
    Monthly,
    Yearly,
    Custom,
}

impl Repeat {
    pub const CHOICES: [Repeat; 6] = [
        Repeat::None,
        Repeat::Daily,
        Repeat::Weekdays,
        Repeat::Weekly,
        Repeat::Monthly,
        Repeat::Yearly,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Repeat::None => "Doesn't repeat",
            Repeat::Daily => "Every day",
            Repeat::Weekdays => "Every weekday (Mon–Fri)",
            Repeat::Weekly => "Every week",
            Repeat::Monthly => "Every month",
            Repeat::Yearly => "Every year",
            Repeat::Custom => "Custom",
        }
    }

    pub fn rule(self) -> Option<&'static str> {
        match self {
            Repeat::None | Repeat::Custom => None,
            Repeat::Daily => Some("FREQ=DAILY"),
            Repeat::Weekdays => Some("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"),
            Repeat::Weekly => Some("FREQ=WEEKLY"),
            Repeat::Monthly => Some("FREQ=MONTHLY"),
            Repeat::Yearly => Some("FREQ=YEARLY"),
        }
    }

    pub fn from_rule(rule: Option<&str>) -> Repeat {
        match rule.map(|r| r.trim().to_ascii_uppercase()) {
            None => Repeat::None,
            Some(r) => Self::CHOICES
                .into_iter()
                .find(|c| c.rule() == Some(r.as_str()))
                .unwrap_or(Repeat::Custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn berlin(y: i32, m: u32, d: u32, h: u32) -> i64 {
        chrono_tz::Europe::Berlin
            .with_ymd_and_hms(y, m, d, h, 0, 0)
            .unwrap()
            .timestamp()
    }

    #[test]
    fn weekly_keeps_local_time_across_dst() {
        let ev = Event {
            title: "Standup".into(),
            start: berlin(2026, 10, 19, 9),
            end: berlin(2026, 10, 19, 10),
            tz: "Europe/Berlin".into(),
            rrule: Some("FREQ=WEEKLY;COUNT=4".into()),
            ..Default::default()
        };
        let occ = expand(&ev, berlin(2026, 10, 1, 0), berlin(2026, 12, 1, 0));
        assert_eq!(occ.len(), 4);
        // DST ends on 25 Oct 2026: 09:00 is 07:00Z before and 08:00Z after.
        assert_eq!(occ[0].0, berlin(2026, 10, 19, 9));
        assert_eq!(occ[1].0, berlin(2026, 10, 26, 9));
        assert_eq!(occ[1].0 - occ[0].0, 7 * 86400 + 3600);
        assert!(occ.iter().all(|(s, e)| e - s == 3600));
    }

    #[test]
    fn exdates_and_window() {
        let start = berlin(2026, 1, 1, 12);
        let mut ev = Event {
            start,
            end: start + 1800,
            tz: "Europe/Berlin".into(),
            rrule: Some("FREQ=DAILY".into()),
            ..Default::default()
        };
        ev.exdates.push(start + 86400);
        let occ = expand(&ev, start, start + 5 * 86400);
        assert_eq!(occ.iter().map(|o| o.0).collect::<Vec<_>>(), vec![start, start + 2 * 86400, start + 3 * 86400, start + 4 * 86400]);
        // An occurrence already in progress at `from` is included.
        let occ = expand(&ev, start + 600, start + 700);
        assert_eq!(occ, vec![(start, start + 1800)]);
    }

    #[test]
    fn all_day_yearly_with_date_until() {
        let d = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
        let ev = Event {
            start: d,
            end: d + 86400,
            all_day: true,
            rrule: Some("FREQ=YEARLY;UNTIL=20330101".into()),
            ..Default::default()
        };
        let occ = expand(&ev, d, d + 10 * 366 * 86400);
        // Feb 29 only exists in leap years.
        assert_eq!(occ.len(), 3);
    }

    #[test]
    fn repeat_presets() {
        assert_eq!(Repeat::from_rule(Some("FREQ=WEEKLY")), Repeat::Weekly);
        assert_eq!(Repeat::from_rule(Some("FREQ=WEEKLY;INTERVAL=2")), Repeat::Custom);
        assert_eq!(Repeat::from_rule(None), Repeat::None);
        assert_eq!(normalize_rule("RRULE:FREQ=DAILY;UNTIL=20260101T100000"), "FREQ=DAILY;UNTIL=20260101T100000Z");
    }
}
