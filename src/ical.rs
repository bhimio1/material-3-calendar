//! A small, forgiving iCalendar (RFC 5545) reader and writer for what a
//! calendar app needs: VEVENT (times, zones, all-day, RRULE/EXDATE,
//! RECURRENCE-ID, VALARM) and VTODO. Used by CalDAV and .ics import/export.

use crate::model::{Event, Task};
use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};

#[derive(Debug, Default)]
pub struct Parsed {
    pub name: Option<String>,
    pub color: Option<String>,
    pub events: Vec<Event>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone)]
struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix([' ', '\t']) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

fn parse_line(line: &str) -> Option<Prop> {
    // Split name;params from the value at the first ':' outside quotes.
    let mut in_quotes = false;
    let mut split = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                split = Some(i);
                break;
            }
            _ => {}
        }
    }
    let split = split?;
    let (head, value) = (&line[..split], &line[split + 1..]);
    let mut parts = Vec::new();
    let mut cur = String::new();
    in_quotes = false;
    for c in head.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    let name = parts.remove(0).to_ascii_uppercase();
    let params = parts
        .into_iter()
        .filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())))
        .collect();
    Some(Prop { name, params, value: value.to_string() })
}

pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
}

/// A parsed DTSTART/DTEND-like value: UTC seconds, whether it was a date,
/// and the zone it was given in.
fn parse_time(prop: &Prop, fallback_tz: &str) -> Option<(i64, bool, String)> {
    let v = prop.value.trim();
    let is_date = prop.param("VALUE").is_some_and(|x| x.eq_ignore_ascii_case("DATE")) || v.len() == 8;
    if is_date {
        let d = NaiveDate::parse_from_str(&v[..8.min(v.len())], "%Y%m%d").ok()?;
        return Some((d.and_hms_opt(0, 0, 0)?.and_utc().timestamp(), true, String::new()));
    }
    if let Some(utc) = v.strip_suffix('Z') {
        let dt = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some((dt.and_utc().timestamp(), false, fallback_tz.to_string()));
    }
    let dt = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
    let tzid = prop
        .param("TZID")
        .map(|t| t.trim_matches('"').trim_start_matches('/').to_string())
        .and_then(|t| map_tzid(&t))
        .unwrap_or_else(|| fallback_tz.to_string());
    let zone: chrono_tz::Tz = tzid.parse().unwrap_or(chrono_tz::UTC);
    let ts = zone
        .from_local_datetime(&dt)
        .earliest()
        .or_else(|| zone.from_local_datetime(&(dt + chrono::Duration::hours(1))).earliest())?
        .timestamp();
    Some((ts, false, tzid))
}

/// Accepts IANA ids, plus the Windows names Outlook/Exchange put in TZID.
fn map_tzid(t: &str) -> Option<String> {
    if t.parse::<chrono_tz::Tz>().is_ok() {
        return Some(t.to_string());
    }
    let windows = [
        ("Pacific Standard Time", "America/Los_Angeles"),
        ("Mountain Standard Time", "America/Denver"),
        ("Central Standard Time", "America/Chicago"),
        ("Eastern Standard Time", "America/New_York"),
        ("GMT Standard Time", "Europe/London"),
        ("W. Europe Standard Time", "Europe/Berlin"),
        ("Romance Standard Time", "Europe/Paris"),
        ("Central Europe Standard Time", "Europe/Budapest"),
        ("Central European Standard Time", "Europe/Warsaw"),
        ("E. Europe Standard Time", "Europe/Bucharest"),
        ("India Standard Time", "Asia/Kolkata"),
        ("China Standard Time", "Asia/Shanghai"),
        ("Tokyo Standard Time", "Asia/Tokyo"),
        ("AUS Eastern Standard Time", "Australia/Sydney"),
        ("UTC", "UTC"),
    ];
    windows
        .iter()
        .find(|(w, _)| w.eq_ignore_ascii_case(t))
        .map(|(_, iana)| iana.to_string())
}

/// `-PT15M`, `-P1D`, `PT1H30M`, `-P1W` → seconds (signed).
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, rest) = match s.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let rest = rest.strip_prefix('P')?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut in_time = false;
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            _ => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match (c, in_time) {
                    ('W', _) => 7 * 86400,
                    ('D', _) => 86400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    Some(sign * total)
}

pub fn fmt_utc(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0).single().unwrap_or_default().format("%Y%m%dT%H%M%SZ").to_string()
}

pub fn fmt_date(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0).single().unwrap_or_default().format("%Y%m%d").to_string()
}

/// Parses a VCALENDAR (one resource or a whole .ics file). Floating times
/// are read in `local_tz`.
pub fn parse(text: &str, local_tz: &str) -> Parsed {
    let mut out = Parsed::default();
    let mut stack: Vec<String> = Vec::new();
    let mut props: Vec<Prop> = Vec::new();
    let mut alarms: Vec<i64> = Vec::new();
    let mut alarm_trigger: Option<Prop> = None;
    for line in unfold(text) {
        let Some(prop) = parse_line(&line) else { continue };
        match prop.name.as_str() {
            "BEGIN" => {
                let comp = prop.value.to_ascii_uppercase();
                if comp == "VEVENT" || comp == "VTODO" {
                    props.clear();
                    alarms.clear();
                }
                stack.push(comp);
            }
            "END" => {
                let comp = stack.pop().unwrap_or_default();
                match comp.as_str() {
                    "VALARM" => {
                        if let Some(t) = alarm_trigger.take() {
                            let related_end = t.param("RELATED").is_some_and(|r| r.eq_ignore_ascii_case("END"));
                            if let (Some(secs), false) = (parse_duration(&t.value), related_end) {
                                alarms.push((-secs / 60).max(0));
                            }
                        }
                    }
                    "VEVENT" => {
                        if let Some(ev) = build_event(&props, &alarms, local_tz) {
                            out.events.push(ev);
                        }
                    }
                    "VTODO" => {
                        if let Some(t) = build_task(&props, local_tz) {
                            out.tasks.push(t);
                        }
                    }
                    _ => {}
                }
            }
            _ => match stack.last().map(String::as_str) {
                Some("VALARM") => {
                    if prop.name == "TRIGGER" {
                        alarm_trigger = Some(prop);
                    }
                }
                Some("VEVENT") | Some("VTODO") => props.push(prop),
                Some("VCALENDAR") => match prop.name.as_str() {
                    "X-WR-CALNAME" => out.name = Some(unescape(&prop.value)),
                    "X-APPLE-CALENDAR-COLOR" | "COLOR" => out.color = Some(prop.value.chars().take(7).collect()),
                    _ => {}
                },
                _ => {}
            },
        }
    }
    out
}

fn text(props: &[Prop], name: &str) -> String {
    props.iter().find(|p| p.name == name).map(|p| unescape(&p.value)).unwrap_or_default()
}

fn build_event(props: &[Prop], alarms: &[i64], local_tz: &str) -> Option<Event> {
    let find = |n: &str| props.iter().find(|p| p.name == n);
    let (start, all_day, tz) = parse_time(find("DTSTART")?, local_tz)?;
    let end = match (find("DTEND").and_then(|p| parse_time(p, local_tz)), find("DURATION")) {
        (Some((e, _, _)), _) => e,
        (None, Some(d)) => start + parse_duration(&d.value).unwrap_or(0),
        (None, None) if all_day => start + 86400,
        _ => start,
    };
    let mut exdates = Vec::new();
    for p in props.iter().filter(|p| p.name == "EXDATE") {
        for v in p.value.split(',') {
            let single = Prop { value: v.to_string(), ..p.clone() };
            if let Some((t, _, _)) = parse_time(&single, &tz) {
                exdates.push(t);
            }
        }
    }
    if props.iter().any(|p| p.name == "STATUS" && p.value.eq_ignore_ascii_case("CANCELLED"))
        && find("RECURRENCE-ID").is_none()
    {
        return None;
    }
    let mut reminders = alarms.to_vec();
    reminders.sort_unstable();
    reminders.dedup();
    Some(Event {
        uid: text(props, "UID"),
        title: text(props, "SUMMARY"),
        location: text(props, "LOCATION"),
        description: text(props, "DESCRIPTION"),
        start,
        end: end.max(start),
        all_day,
        tz: if all_day { String::new() } else { tz.clone() },
        rrule: find("RRULE").map(|p| p.value.clone()),
        exdates,
        recurrence_id: find("RECURRENCE-ID").and_then(|p| parse_time(p, &tz)).map(|t| t.0),
        reminders,
        ..Default::default()
    })
}

fn build_task(props: &[Prop], local_tz: &str) -> Option<Task> {
    let find = |n: &str| props.iter().find(|p| p.name == n);
    let due = find("DUE").and_then(|p| parse_time(p, local_tz)).map(|(t, _, _)| t - t.rem_euclid(86400));
    let completed = find("COMPLETED").is_some()
        || find("STATUS").is_some_and(|p| p.value.eq_ignore_ascii_case("COMPLETED"));
    Some(Task {
        uid: text(props, "UID"),
        title: text(props, "SUMMARY"),
        notes: text(props, "DESCRIPTION"),
        due,
        completed,
        ..Default::default()
    })
}

/// Folds a content line at 75 octets (on char boundaries).
fn fold(line: &str, out: &mut String) {
    let mut count = 0;
    for c in line.chars() {
        let len = c.len_utf8();
        if count + len > 75 {
            out.push_str("\r\n ");
            count = 1;
        }
        out.push(c);
        count += len;
    }
    out.push_str("\r\n");
}

fn time_prop(name: &str, ts: i64, ev: &Event) -> String {
    if ev.all_day {
        return format!("{name};VALUE=DATE:{}", fmt_date(ts));
    }
    // Recurring events keep their zone so they follow DST; one-off events
    // are written in UTC, which every server understands.
    match ev.tz.parse::<chrono_tz::Tz>() {
        Ok(zone) if ev.rrule.is_some() || ev.recurrence_id.is_some() => {
            let local = zone.timestamp_opt(ts, 0).single().unwrap_or_else(|| zone.timestamp_opt(0, 0).unwrap());
            format!("{name};TZID={}:{}", zone.name(), local.format("%Y%m%dT%H%M%S"))
        }
        _ => format!("{name}:{}", fmt_utc(ts)),
    }
}

fn write_event(ev: &Event, out: &mut String) {
    let mut lines = vec![
        "BEGIN:VEVENT".to_string(),
        format!("UID:{}", ev.uid),
        format!("DTSTAMP:{}", fmt_utc(Utc::now().timestamp())),
        time_prop("DTSTART", ev.start, ev),
        time_prop("DTEND", ev.end.max(ev.start + if ev.all_day { 86400 } else { 0 }), ev),
        format!("SUMMARY:{}", escape(&ev.title)),
    ];
    if let Some(rid) = ev.recurrence_id {
        lines.push(time_prop("RECURRENCE-ID", rid, ev));
    }
    if !ev.location.is_empty() {
        lines.push(format!("LOCATION:{}", escape(&ev.location)));
    }
    if !ev.description.is_empty() {
        lines.push(format!("DESCRIPTION:{}", escape(&ev.description)));
    }
    if let Some(rule) = &ev.rrule {
        lines.push(format!("RRULE:{}", rule.trim_start_matches("RRULE:")));
    }
    for ex in &ev.exdates {
        lines.push(time_prop("EXDATE", *ex, ev));
    }
    for m in &ev.reminders {
        lines.push("BEGIN:VALARM".into());
        lines.push("ACTION:DISPLAY".into());
        lines.push(format!("DESCRIPTION:{}", escape(&ev.title)));
        lines.push(format!("TRIGGER:-PT{m}M"));
        lines.push("END:VALARM".into());
    }
    lines.push("END:VEVENT".into());
    for l in lines {
        fold(&l, out);
    }
}

fn header(out: &mut String, name: Option<&str>) {
    fold("BEGIN:VCALENDAR", out);
    fold("VERSION:2.0", out);
    fold("PRODID:-//bhimio1//Material 3 Calendar//EN", out);
    if let Some(n) = name {
        fold(&format!("X-WR-CALNAME:{}", escape(n)), out);
    }
}

/// One CalDAV resource: a series with its edited instances.
pub fn event_resource(series: &[&Event]) -> String {
    let mut out = String::new();
    header(&mut out, None);
    for ev in series {
        write_event(ev, &mut out);
    }
    fold("END:VCALENDAR", &mut out);
    out
}

pub fn task_resource(t: &Task) -> String {
    let mut out = String::new();
    header(&mut out, None);
    let mut lines = vec![
        "BEGIN:VTODO".to_string(),
        format!("UID:{}", t.uid),
        format!("DTSTAMP:{}", fmt_utc(Utc::now().timestamp())),
        format!("SUMMARY:{}", escape(&t.title)),
    ];
    if !t.notes.is_empty() {
        lines.push(format!("DESCRIPTION:{}", escape(&t.notes)));
    }
    if let Some(d) = t.due {
        lines.push(format!("DUE;VALUE=DATE:{}", fmt_date(d)));
    }
    if t.completed {
        lines.push("STATUS:COMPLETED".into());
        lines.push(format!("COMPLETED:{}", fmt_utc(Utc::now().timestamp())));
    } else {
        lines.push("STATUS:NEEDS-ACTION".into());
    }
    lines.push("END:VTODO".into());
    for l in lines {
        fold(&l, &mut out);
    }
    fold("END:VCALENDAR", &mut out);
    out
}

/// A whole calendar, for export.
pub fn calendar_file(name: &str, events: &[Event]) -> String {
    let mut out = String::new();
    header(&mut out, Some(name));
    for ev in events {
        write_event(ev, &mut out);
    }
    fold("END:VCALENDAR", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-CALNAME:Team\r\nBEGIN:VEVENT\r\nUID:abc@example.com\r\nDTSTART;TZID=Europe/Berlin:20261019T090000\r\nDTEND;TZID=Europe/Berlin:20261019T093000\r\nSUMMARY:Standup\\, daily\r\nDESCRIPTION:Line one\\nLine two that is long enough to be folded by a careful\r\n  writer\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE\r\nEXDATE;TZID=Europe/Berlin:20261021T090000,20261026T090000\r\nBEGIN:VALARM\r\nTRIGGER:-PT10M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:abc@example.com\r\nRECURRENCE-ID;TZID=Europe/Berlin:20261028T090000\r\nDTSTART;TZID=Europe/Berlin:20261028T100000\r\nDURATION:PT45M\r\nSUMMARY:Standup (moved)\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:holiday\r\nDTSTART;VALUE=DATE:20261225\r\nSUMMARY:Christmas\r\nEND:VEVENT\r\nBEGIN:VTODO\r\nUID:t1\r\nSUMMARY:File taxes\r\nDUE;VALUE=DATE:20261031\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_events_tasks_and_alarms() {
        let p = parse(SAMPLE, "UTC");
        assert_eq!(p.name.as_deref(), Some("Team"));
        assert_eq!(p.events.len(), 3);
        let s = &p.events[0];
        assert_eq!(s.title, "Standup, daily");
        assert_eq!(s.description, "Line one\nLine two that is long enough to be folded by a careful writer");
        assert_eq!(s.tz, "Europe/Berlin");
        assert_eq!(s.end - s.start, 1800);
        assert_eq!(s.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO,WE"));
        assert_eq!(s.exdates.len(), 2);
        assert_eq!(s.reminders, vec![10]);
        // 09:00 CEST = 07:00Z
        assert_eq!(fmt_utc(s.start), "20261019T070000Z");
        let o = &p.events[1];
        assert_eq!(o.recurrence_id.map(fmt_utc).as_deref(), Some("20261028T080000Z"));
        assert_eq!(o.end - o.start, 45 * 60);
        let h = &p.events[2];
        assert!(h.all_day);
        assert_eq!(h.end - h.start, 86400);
        assert_eq!(p.tasks.len(), 1);
        assert_eq!(p.tasks[0].due.map(fmt_date).as_deref(), Some("20261031"));
        assert!(!p.tasks[0].completed);
    }

    #[test]
    fn round_trips() {
        let p = parse(SAMPLE, "UTC");
        let series: Vec<&Event> = p.events.iter().take(2).collect();
        let text = event_resource(&series);
        assert!(text.lines().all(|l| l.len() <= 76));
        let again = parse(&text, "UTC");
        assert_eq!(again.events.len(), 2);
        for (a, b) in again.events.iter().zip(&p.events) {
            assert_eq!(a.uid, b.uid);
            assert_eq!(a.title, b.title);
            assert_eq!(a.description, b.description);
            assert_eq!((a.start, a.end), (b.start, b.end));
            assert_eq!(a.rrule, b.rrule);
            assert_eq!(a.exdates, b.exdates);
            assert_eq!(a.recurrence_id, b.recurrence_id);
            assert_eq!(a.reminders, b.reminders);
            assert_eq!(a.tz, b.tz);
        }
        let task = parse(&task_resource(&p.tasks[0]), "UTC").tasks.remove(0);
        assert_eq!(task.title, "File taxes");
        assert_eq!(task.due, p.tasks[0].due);
        let mut done = p.tasks[0].clone();
        done.completed = true;
        assert!(parse(&task_resource(&done), "UTC").tasks[0].completed);
        let file = calendar_file("Mine; ours", &p.events);
        assert_eq!(parse(&file, "UTC").name.as_deref(), Some("Mine; ours"));
    }

    #[test]
    fn durations_and_quirks() {
        assert_eq!(parse_duration("-PT15M"), Some(-900));
        assert_eq!(parse_duration("-P1DT2H"), Some(-93600));
        assert_eq!(parse_duration("P1W"), Some(604800));
        assert_eq!(parse_duration("bogus"), None);
        let outlook = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nDTSTART;TZID=\"W. Europe Standard Time\":20260105T100000\nSUMMARY:Q\nEND:VEVENT\nEND:VCALENDAR\n";
        let p = parse(outlook, "UTC");
        assert_eq!(p.events[0].tz, "Europe/Berlin");
        assert_eq!(fmt_utc(p.events[0].start), "20260105T090000Z");
        // Floating times use the local zone.
        let floating = "BEGIN:VEVENT\nUID:y\nDTSTART:20260105T100000\nEND:VEVENT\n";
        let wrapped = format!("BEGIN:VCALENDAR\n{floating}END:VCALENDAR\n");
        assert_eq!(fmt_utc(parse(&wrapped, "America/New_York").events[0].start), "20260105T150000Z");
    }
}
