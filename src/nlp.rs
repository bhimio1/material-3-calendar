//! Quick add: turns "Lunch with Sam tomorrow 1pm at Café Luna for 90 min"
//! into an event. Understands today/tomorrow/tonight, weekday names (with
//! "next"), ISO and month-name dates, times (1pm, 13:30, noon, ranges like
//! 1-3pm or "from 9 to 11am"), "for 2h"/"for 30 min", "all day", and a
//! place after "at"/"@". Everything else is the title.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

#[derive(Clone, Debug, PartialEq)]
pub struct Quick {
    pub title: String,
    pub location: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub all_day: bool,
    /// From "every day", "weekly", "every monday", "every weekday", …
    pub rrule: Option<String>,
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn weekday(word: &str) -> Option<Weekday> {
    let w = word.trim_end_matches(['.', ',']);
    Some(match w {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn month(word: &str) -> Option<u32> {
    let w = word.trim_end_matches(['.', ',']);
    if w.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|m| w.starts_with(m) && (w.len() == 3 || full_month(w)))
        .map(|i| i as u32 + 1)
}

fn full_month(w: &str) -> bool {
    [
        "january", "february", "march", "april", "may", "june", "july", "august", "september", "sept",
        "october", "november", "december",
    ]
    .contains(&w)
}

fn day_number(word: &str) -> Option<u32> {
    let w = word
        .trim_end_matches(['.', ','])
        .trim_end_matches("st")
        .trim_end_matches("nd")
        .trim_end_matches("rd")
        .trim_end_matches("th");
    w.parse().ok().filter(|d| (1..=31).contains(d))
}

/// "1pm", "1:30pm", "13:00", "9am", "noon", "midnight", plus a bare hour
/// ("9") when `bare` is allowed. Returns the time and whether am/pm was given.
fn time(word: &str, bare: bool) -> Option<(NaiveTime, bool)> {
    let w = word.trim_end_matches([',', '.']);
    match w {
        "noon" | "midday" => return Some((NaiveTime::from_hms_opt(12, 0, 0)?, true)),
        "midnight" => return Some((NaiveTime::from_hms_opt(0, 0, 0)?, true)),
        _ => {}
    }
    let (num, suffix) = if let Some(n) = w.strip_suffix("am").or_else(|| w.strip_suffix("a")) {
        (n, Some(false))
    } else if let Some(n) = w.strip_suffix("pm").or_else(|| w.strip_suffix("p")) {
        (n, Some(true))
    } else {
        (w, None)
    };
    let (h, m) = match num.split_once([':', '.']) {
        // "1:1" isn't a time; minutes always have two digits.
        Some((h, m)) if m.len() == 2 => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        Some(_) => return None,
        None if suffix.is_some() || bare => (num.parse::<u32>().ok()?, 0),
        None => return None,
    };
    if num.is_empty() || m > 59 {
        return None;
    }
    let h = match suffix {
        Some(pm) if (1..=12).contains(&h) => (h % 12) + if pm { 12 } else { 0 },
        Some(_) => return None,
        None if h < 24 => h,
        None => return None,
    };
    Some((NaiveTime::from_hms_opt(h, m, 0)?, suffix.is_some()))
}

/// "2h", "90min", "1.5 hours" (two words handled by the caller).
fn duration(num: &str, unit: &str) -> Option<Duration> {
    let n: f64 = num.parse().ok()?;
    let mins = match unit.trim_end_matches(['.', ',']) {
        "h" | "hr" | "hrs" | "hour" | "hours" => n * 60.0,
        "m" | "min" | "mins" | "minute" | "minutes" => n,
        "d" | "day" | "days" => n * 1440.0,
        _ => return None,
    };
    Some(Duration::minutes(mins.round() as i64))
}

fn split_unit(w: &str) -> Option<(&str, &str)> {
    let i = w.find(|c: char| c.is_ascii_alphabetic())?;
    (i > 0).then(|| w.split_at(i))
}

pub fn parse(input: &str, now: NaiveDateTime) -> Quick {
    let words: Vec<&str> = input.split_whitespace().collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let mut used = vec![false; words.len()];
    let today = now.date();
    let mut date: Option<NaiveDate> = None;
    let mut start_t: Option<NaiveTime> = None;
    let mut end_t: Option<NaiveTime> = None;
    let mut dur: Option<Duration> = None;
    let mut all_day = false;
    let mut location: Vec<&str> = Vec::new();
    let mut rrule: Option<String> = None;

    let is_marker = |i: usize, lower: &[String]| -> bool {
        let w = lower[i].as_str();
        matches!(w, "today" | "tomorrow" | "tonight" | "for" | "from" | "on" | "next")
            || weekday(w).is_some()
            || time(w, false).is_some()
            || (w == "all" && lower.get(i + 1).is_some_and(|n| n.starts_with("day")))
    };

    let mut i = 0;
    while i < words.len() {
        let w = lower[i].as_str();
        let next = lower.get(i + 1).map(String::as_str);
        // Repeats.
        let simple = match w {
            "daily" => Some("FREQ=DAILY"),
            "weekly" => Some("FREQ=WEEKLY"),
            "monthly" => Some("FREQ=MONTHLY"),
            "yearly" | "annually" => Some("FREQ=YEARLY"),
            _ => None,
        };
        if let Some(r) = simple {
            rrule = Some(r.into());
            used[i] = true;
            i += 1;
            continue;
        }
        if w == "every" {
            if let Some(n) = next {
                let rule = match n.trim_end_matches(',') {
                    "day" => Some("FREQ=DAILY".to_string()),
                    "week" => Some("FREQ=WEEKLY".to_string()),
                    "month" => Some("FREQ=MONTHLY".to_string()),
                    "year" => Some("FREQ=YEARLY".to_string()),
                    "weekday" | "weekdays" => Some("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".to_string()),
                    other => weekday(other.trim_end_matches('s')).or_else(|| weekday(other)).map(|wd| {
                        // The first one is the next such day.
                        let mut d = today;
                        while d.weekday() != wd {
                            d += Duration::days(1);
                        }
                        date.get_or_insert(d);
                        format!("FREQ=WEEKLY;BYDAY={}", &format!("{wd:?}").to_uppercase()[..2])
                    }),
                };
                if let Some(r) = rule {
                    if r.contains("BYDAY=MO,TU") && date.is_none() {
                        let mut d = today;
                        while d.weekday().num_days_from_monday() > 4 {
                            d += Duration::days(1);
                        }
                        date = Some(d);
                    }
                    rrule = Some(r);
                    used[i] = true;
                    used[i + 1] = true;
                    i += 2;
                    continue;
                }
            }
        }
        // Dates.
        if w == "today" {
            date = Some(today);
            used[i] = true;
        } else if w == "tomorrow" || w == "tmrw" {
            date = Some(today + Duration::days(1));
            used[i] = true;
        } else if w == "tonight" {
            date = Some(today);
            start_t.get_or_insert(NaiveTime::from_hms_opt(19, 0, 0).unwrap());
            used[i] = true;
        } else if let (Some(wd), true) = (weekday(w), true) {
            let mut d = today + Duration::days(1);
            while d.weekday() != wd {
                d += Duration::days(1);
            }
            let next_word = i > 0 && lower[i - 1] == "next";
            if next_word {
                used[i - 1] = true;
                // "next friday" means the one in the coming week.
                if (d - today).num_days() < 7 && today.weekday().num_days_from_monday() < wd.num_days_from_monday() {
                    d += Duration::days(7);
                }
            }
            if i > 0 && lower[i - 1] == "on" {
                used[i - 1] = true;
            }
            date = Some(d);
            used[i] = true;
        } else if let Ok(d) = NaiveDate::parse_from_str(w, "%Y-%m-%d") {
            date = Some(d);
            used[i] = true;
        } else if let Some(m) = month(w) {
            // "oct 21" or "21 oct"
            if let Some(d) = next.and_then(day_number) {
                date = month_day(today, m, d);
                used[i] = true;
                used[i + 1] = true;
                i += 1;
            } else if i > 0 && !used[i - 1] {
                if let Some(d) = day_number(&lower[i - 1]) {
                    date = month_day(today, m, d);
                    used[i - 1] = true;
                    used[i] = true;
                }
            }
        } else if let Some((m, d)) = w.split_once('/').and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.trim_end_matches(',').parse::<u32>().ok()?))) {
            if let Some(dt) = month_day(today, m, d) {
                date = Some(dt);
                used[i] = true;
            }
        }
        // All day.
        else if w == "all" && next.is_some_and(|n| n.starts_with("day")) {
            all_day = true;
            used[i] = true;
            used[i + 1] = true;
            i += 1;
        }
        // Durations: "for 2h", "for 2 hours".
        else if w == "for" {
            if let Some(n) = next {
                if let Some((num, unit)) = split_unit(n) {
                    if let Some(d) = duration(num, unit) {
                        dur = Some(d);
                        used[i] = true;
                        used[i + 1] = true;
                        i += 1;
                    }
                } else if let Some(unit) = lower.get(i + 2) {
                    if let Some(d) = duration(n, unit) {
                        dur = Some(d);
                        used[i..=i + 2].fill(true);
                        i += 2;
                    }
                }
            }
        }
        // Ranges: "1-3pm", "1pm-3pm", "9:00-10:30".
        else if let Some((a, b)) = w.split_once(['-', '–']).filter(|(a, b)| !a.is_empty() && !b.is_empty()) {
            if let Some((e, end_has_suffix)) = time(b, true) {
                if let Some((mut s, has)) = time(a, true) {
                    // "1-3pm": the start borrows the end's am/pm.
                    if !has && end_has_suffix && s.format("%H").to_string().parse::<u32>().unwrap_or(0) < 12 && e.format("%H").to_string().parse::<u32>().unwrap_or(0) >= 12 {
                        let shifted = s + Duration::hours(12);
                        if shifted < e {
                            s = shifted;
                        }
                    }
                    start_t = Some(s);
                    end_t = Some(e);
                    used[i] = true;
                    if i > 0 && lower[i - 1] == "at" {
                        used[i - 1] = true;
                    }
                }
            }
        }
        // "from 9 to 11am" / "9am to 11am".
        else if (w == "to" || w == "until" || w == "till") && start_t.is_some() && end_t.is_none() {
            if let Some((e, _)) = next.and_then(|n| time(n, true)) {
                end_t = Some(e);
                used[i] = true;
                used[i + 1] = true;
                i += 1;
            }
        } else if w == "from" && next.and_then(|n| time(n, true)).is_some() {
            used[i] = true;
        } else if let Some((t, _)) = time(w, i > 0 && matches!(lower[i - 1].as_str(), "at" | "from" | "@")) {
            if start_t.is_none() {
                start_t = Some(t);
            } else if end_t.is_none() {
                end_t = Some(t);
            }
            used[i] = true;
            if i > 0 && matches!(lower[i - 1].as_str(), "at" | "@") {
                used[i - 1] = true;
            }
        }
        // Places: "at the office", "@ home".
        else if (w == "at" || w == "@") && next.is_some_and(|_| !is_marker(i + 1, &lower)) {
            used[i] = true;
            let mut j = i + 1;
            while j < words.len() && !is_marker(j, &lower) && !(lower[j] == "on" && j + 1 < words.len() && is_marker(j + 1, &lower)) {
                location.push(words[j]);
                used[j] = true;
                j += 1;
            }
            i = j;
            continue;
        } else if let Some(place) = words[i].strip_prefix('@').filter(|p| !p.is_empty()) {
            location.push(place);
            used[i] = true;
        }
        i += 1;
    }

    let title = words
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(w, _)| *w)
        .collect::<Vec<_>>()
        .join(" ");
    let title = title.trim_matches(|c: char| c == ',' || c.is_whitespace()).to_string();
    let day = date.unwrap_or(today);
    let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
    let (start, end, all_day) = match start_t {
        Some(s) if !all_day => {
            let start = day.and_time(s);
            let end = match (end_t, dur) {
                (Some(e), _) => {
                    let e = day.and_time(e);
                    if e <= start { e + Duration::days(1) } else { e }
                }
                (None, Some(d)) => start + d,
                (None, None) => start + Duration::hours(1),
            };
            (start, end, false)
        }
        _ => {
            let days = dur.map(|d| d.num_days().max(1)).unwrap_or(1);
            (day.and_time(midnight), (day + Duration::days(days)).and_time(midnight), true)
        }
    };
    Quick {
        title: if title.is_empty() { "New event".into() } else { title },
        location: location.join(" ").trim_end_matches([',', '.']).to_string(),
        start,
        end,
        all_day,
        rrule,
    }
}

/// The next date with this month and day (this year, or next if passed).
fn month_day(today: NaiveDate, m: u32, d: u32) -> Option<NaiveDate> {
    let this = NaiveDate::from_ymd_opt(today.year(), m, d)?;
    if this < today {
        NaiveDate::from_ymd_opt(today.year() + 1, m, d)
    } else {
        Some(this)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Wednesday, 21 Oct 2026, 10:00.
    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 21).unwrap().and_hms_opt(10, 0, 0).unwrap()
    }

    fn dt(m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, m, d).unwrap().and_hms_opt(h, min, 0).unwrap()
    }

    #[test]
    fn lunch_tomorrow() {
        let q = parse("Lunch tomorrow 1pm", now());
        assert_eq!(q.title, "Lunch");
        assert_eq!(q.start, dt(10, 22, 13, 0));
        assert_eq!(q.end, dt(10, 22, 14, 0));
        assert!(!q.all_day);
    }

    #[test]
    fn place_and_duration() {
        let q = parse("Lunch with Sam tomorrow at 1:30pm at Café Luna for 90 min", now());
        assert_eq!(q.title, "Lunch with Sam");
        assert_eq!(q.location, "Café Luna");
        assert_eq!(q.start, dt(10, 22, 13, 30));
        assert_eq!(q.end, dt(10, 22, 15, 0));
    }

    #[test]
    fn weekdays_and_ranges() {
        let q = parse("Dentist friday 9-10:30am", now());
        assert_eq!(q.title, "Dentist");
        assert_eq!((q.start, q.end), (dt(10, 23, 9, 0), dt(10, 23, 10, 30)));
        let q = parse("Review next monday 1-3pm @office", now());
        assert_eq!(q.title, "Review");
        assert_eq!(q.location, "office");
        assert_eq!((q.start, q.end), (dt(10, 26, 13, 0), dt(10, 26, 15, 0)));
        let q = parse("Gym on wed from 6pm to 7:15pm", now());
        assert_eq!(q.title, "Gym");
        assert_eq!((q.start, q.end), (dt(10, 28, 18, 0), dt(10, 28, 19, 15)));
    }

    #[test]
    fn dates_and_all_day() {
        let q = parse("Mum's birthday oct 30", now());
        assert_eq!(q.title, "Mum's birthday");
        assert!(q.all_day);
        assert_eq!((q.start, q.end), (dt(10, 30, 0, 0), dt(10, 31, 0, 0)));
        let q = parse("Trip 2026-12-24 for 3 days", now());
        assert_eq!((q.start, q.end), (dt(12, 24, 0, 0), dt(12, 27, 0, 0)));
        let q = parse("Conference 5th march", now());
        assert_eq!(q.start.date(), NaiveDate::from_ymd_opt(2027, 3, 5).unwrap());
        let q = parse("Party tonight at Alex's place", now());
        assert_eq!(q.location, "Alex's place");
        assert_eq!(q.start, dt(10, 21, 19, 0));
        let q = parse("Call 12/1 at noon", now());
        assert_eq!(q.start, dt(12, 1, 12, 0));
        assert_eq!(parse("", now()).title, "New event");
        assert_eq!(parse("Standup 9:15", now()).start, dt(10, 21, 9, 15));
        assert_eq!(parse("1:1 with Alex friday 2pm", now()).title, "1:1 with Alex");
    }

    #[test]
    fn repeats() {
        let q = parse("Standup every weekday 9:30-9:45am", now());
        assert_eq!(q.title, "Standup");
        assert_eq!(q.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"));
        assert_eq!(q.start, dt(10, 21, 9, 30));
        let q = parse("Piano every thursday 5pm", now());
        assert_eq!(q.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=TH"));
        assert_eq!(q.start, dt(10, 22, 17, 0));
        assert_eq!(parse("Rent monthly", now()).rrule.as_deref(), Some("FREQ=MONTHLY"));
    }
}
