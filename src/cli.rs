//! The command line. Reads go to the daemon over D-Bus when it runs and to
//! the cache directly otherwise; writes are queued for the next sync either
//! way.

use crate::config::Settings;
use crate::dbus::Client;
use crate::model::Occurrence;
use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use chrono::{Duration, Local, NaiveDate, TimeZone};

pub const HELP: &str = "Material 3 Calendar

Usage:
  m3-calendar [--view month|week|day|agenda] [--date YYYY-MM-DD] [--page accounts|preferences]
  m3-calendar --open EVENT_ID [--at UNIX_TIME]
  m3-calendar events [--from DATE] [--to DATE] [--json]
  m3-calendar agenda [--days N] [--json]
  m3-calendar add \"Lunch with Sam tomorrow 1pm at Café Luna\" [--calendar ID] [--json]
  m3-calendar tasks [--json]
  m3-calendar calendars [--json]
  m3-calendar sync
  m3-calendar import FILE.ics
  m3-calendar export CALENDAR_ID FILE.ics
  m3-calendar daemon            (also --daemon)

DATE is YYYY-MM-DD, today, tomorrow or yesterday. Times are local.
";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

pub fn parse_date(s: &str) -> Result<NaiveDate> {
    let today = Local::now().date_naive();
    Ok(match s {
        "today" => today,
        "tomorrow" => today + Duration::days(1),
        "yesterday" => today - Duration::days(1),
        _ => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| anyhow!("{s}: expected YYYY-MM-DD"))?,
    })
}

pub fn local_midnight(d: NaiveDate) -> i64 {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|t| t.timestamp())
        .unwrap_or_else(|| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

fn events(from: i64, to: i64) -> Result<Vec<Occurrence>> {
    if let Some(c) = Client::connect() {
        let json: String = c.call("GetEvents", &(from, to))?;
        return Ok(serde_json::from_str(&json)?);
    }
    Store::open_default()?.occurrences(from, to, true)
}

fn print_occurrences(list: &[Occurrence], json: bool) {
    if json {
        println!("{}", crate::ops::occurrences_json(list));
        return;
    }
    if list.is_empty() {
        println!("Nothing scheduled.");
        return;
    }
    let mut day = None;
    for o in list {
        let local = |t: i64| Local.timestamp_opt(t, 0).single().unwrap_or_default();
        let d = if o.all_day {
            chrono::DateTime::from_timestamp(o.start, 0).unwrap_or_default().date_naive()
        } else {
            local(o.start).date_naive()
        };
        if day != Some(d) {
            day = Some(d);
            println!("\n{}", d.format("%A, %e %B %Y"));
        }
        let when = if o.all_day {
            "all day      ".to_string()
        } else {
            format!("{} – {}", local(o.start).format("%H:%M"), local(o.end).format("%H:%M"))
        };
        let place = if o.location.is_empty() { String::new() } else { format!("  @ {}", o.location) };
        println!("  {when}  {}{place}  [{}]", o.title, o.calendar);
    }
}

/// Handles a subcommand. `Ok(false)` means "not a subcommand, start the app".
pub fn run(args: &[String]) -> Result<bool> {
    let Some(cmd) = args.first().map(String::as_str) else { return Ok(false) };
    let json = has(args, "--json");
    match cmd {
        "help" | "--help" | "-h" => print!("{HELP}"),
        "--version" | "-V" => println!("m3-calendar {}", env!("CARGO_PKG_VERSION")),
        "daemon" | "--daemon" => crate::daemon::run()?,
        "events" => {
            let today = Local::now().date_naive();
            let from = flag(args, "--from").map(|s| parse_date(&s)).transpose()?.unwrap_or(today);
            let to = flag(args, "--to").map(|s| parse_date(&s)).transpose()?.unwrap_or(from + Duration::days(6));
            if to < from {
                bail!("--to is before --from");
            }
            // --to is inclusive.
            print_occurrences(&events(local_midnight(from), local_midnight(to + Duration::days(1)))?, json);
        }
        "agenda" => {
            let days: i64 = flag(args, "--days").map(|d| d.parse()).transpose()?.unwrap_or(7).clamp(1, 366);
            let today = Local::now().date_naive();
            let now = Local::now().timestamp();
            let list: Vec<Occurrence> = events(local_midnight(today), local_midnight(today + Duration::days(days)))?
                .into_iter()
                .filter(|o| o.end > now || o.all_day)
                .collect();
            print_occurrences(&list, json);
        }
        "add" => {
            let text: Vec<&str> = args[1..]
                .iter()
                .enumerate()
                .filter(|(i, a)| !a.starts_with("--") && !(*i > 0 && args[*i] == "--calendar"))
                .map(|(_, a)| a.as_str())
                .collect();
            let text = text.join(" ");
            if text.trim().is_empty() {
                bail!("usage: m3-calendar add \"Lunch tomorrow 1pm\"");
            }
            let calendar = flag(args, "--calendar");
            let ev = match (Client::connect(), &calendar) {
                (Some(c), None) => {
                    let json: String = c.call("QuickAdd", &(text.as_str(),))?;
                    serde_json::from_str(&json)?
                }
                _ => {
                    let store = Store::open_default()?;
                    crate::sync::ensure_local_calendar(&store)?;
                    crate::ops::quick_add(&store, &Settings::load(), &text, calendar.as_deref())?
                }
            };
            if json {
                println!("{}", serde_json::to_string(&ev)?);
            } else {
                let when = if ev.all_day {
                    chrono::DateTime::from_timestamp(ev.start, 0).unwrap_or_default().format("%a %e %b, all day").to_string()
                } else {
                    Local.timestamp_opt(ev.start, 0).single().unwrap_or_default().format("%a %e %b %H:%M").to_string()
                };
                println!("Added “{}” — {when}{}", ev.title, if ev.location.is_empty() { String::new() } else { format!(" @ {}", ev.location) });
            }
        }
        "tasks" => {
            let text = match Client::connect() {
                Some(c) => c.call::<_, String>("GetTasks", &())?,
                None => crate::ops::tasks_json(&Store::open_default()?)?,
            };
            if json {
                println!("{text}");
            } else {
                let list: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                if list.is_empty() {
                    println!("No open tasks.");
                }
                for t in list {
                    let due = t["due_iso"].as_str().map(|d| format!("  (due {d})")).unwrap_or_default();
                    println!("  ☐ {}{due}  [{}]", t["title"].as_str().unwrap_or_default(), t["list"].as_str().unwrap_or_default());
                }
            }
        }
        "calendars" => {
            let text = match Client::connect() {
                Some(c) => c.call::<_, String>("ListCalendars", &())?,
                None => crate::ops::calendars_json(&Store::open_default()?)?,
            };
            if json {
                println!("{text}");
            } else {
                for c in serde_json::from_str::<Vec<serde_json::Value>>(&text)? {
                    println!(
                        "  {} {}  ({}){}  id: {}",
                        if c["visible"].as_bool() == Some(true) { "●" } else { "○" },
                        c["name"].as_str().unwrap_or_default(),
                        c["account"].as_str().unwrap_or_default(),
                        if c["kind"] == "tasks" { " tasks" } else { "" },
                        c["id"].as_str().unwrap_or_default()
                    );
                }
            }
        }
        "sync" => match Client::connect() {
            Some(c) => {
                c.call::<_, ()>("SyncNow", &())?;
                println!("Sync started in the background.");
            }
            None => {
                let store = Store::open_default()?;
                let errors = crate::sync::sync_all(&store, &Settings::load(), std::sync::Arc::new(material_3_ui::secrets::Keyring::new(crate::config::KEYRING_SERVICE)));
                for e in &errors {
                    eprintln!("{e}");
                }
                if !errors.is_empty() {
                    bail!("some accounts didn't sync");
                }
                println!("Synced.");
            }
        },
        "import" => {
            let path = args.get(1).ok_or_else(|| anyhow!("usage: m3-calendar import FILE.ics"))?;
            let (cal, n) = crate::sync::import_ics(&Store::open_default()?, std::path::Path::new(path))?;
            println!("Imported {n} events into “{}”.", cal.name);
        }
        "export" => {
            let (Some(id), Some(path)) = (args.get(1), args.get(2)) else { bail!("usage: m3-calendar export CALENDAR_ID FILE.ics") };
            let n = crate::sync::export_ics(&Store::open_default()?, id, std::path::Path::new(path))?;
            println!("Exported {n} events to {path}.");
        }
        _ if cmd.starts_with("--") => return Ok(false),
        other => bail!("unknown command “{other}”; see m3-calendar --help"),
    }
    Ok(true)
}

/// Options for the app window.
#[derive(Default, Clone, Debug)]
pub struct AppArgs {
    pub view: Option<crate::config::ViewKind>,
    pub date: Option<NaiveDate>,
    pub page: Option<String>,
    pub open: Option<(String, Option<i64>)>,
}

pub fn app_args(args: &[String]) -> Result<AppArgs> {
    Ok(AppArgs {
        view: flag(args, "--view").map(|v| crate::config::ViewKind::parse(&v).ok_or_else(|| anyhow!("unknown view {v}"))).transpose()?,
        date: flag(args, "--date").map(|d| parse_date(&d)).transpose()?,
        page: flag(args, "--page"),
        open: flag(args, "--open").map(|id| (id, flag(args, "--at").and_then(|a| a.parse().ok()))),
    })
}
