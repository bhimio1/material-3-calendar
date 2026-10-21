//! The calendar data model shared by the cache, the providers, the daemon and
//! the UI. Times are UTC seconds; an event's `tz` is the IANA zone its
//! recurrence is expanded in. All-day events start at 00:00 UTC of their
//! first day and end at 00:00 UTC of the day after their last (exclusive),
//! like iCalendar's `VALUE=DATE`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Google,
    Microsoft,
    Icloud,
    Caldav,
    #[default]
    Local,
}

impl AccountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Microsoft => "microsoft",
            Self::Icloud => "icloud",
            Self::Caldav => "caldav",
            Self::Local => "local",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Google => "Google",
            Self::Microsoft => "Microsoft",
            Self::Icloud => "iCloud",
            Self::Caldav => "CalDAV",
            Self::Local => "On this computer",
        }
    }

}

/// What a remote collection holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarKind {
    #[default]
    Events,
    Tasks,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    /// Local id, `"{account_id}/{remote_id}"`.
    pub id: String,
    pub account_id: String,
    /// The provider's id (Google calendar id, Graph id, CalDAV href).
    pub remote_id: String,
    pub name: String,
    /// `#rrggbb`.
    pub color: String,
    pub visible: bool,
    pub read_only: bool,
    pub kind: CalendarKind,
    /// Incremental sync state (Google syncToken, Graph deltaLink, CalDAV
    /// sync-token). `None` means the next sync is a full one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token: Option<String>,
    /// CalDAV collection tag, to skip unchanged collections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctag: Option<String>,
}

impl Calendar {
    pub fn make_id(account_id: &str, remote_id: &str) -> String {
        format!("{account_id}/{remote_id}")
    }
}

/// Local edits not yet sent to the provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dirty {
    #[default]
    Clean,
    Created,
    Updated,
    Deleted,
}

impl Dirty {
    pub fn as_i64(self) -> i64 {
        match self {
            Self::Clean => 0,
            Self::Created => 1,
            Self::Updated => 2,
            Self::Deleted => 3,
        }
    }

    pub fn from_i64(v: i64) -> Self {
        match v {
            1 => Self::Created,
            2 => Self::Updated,
            3 => Self::Deleted,
            _ => Self::Clean,
        }
    }

    /// The state after another local edit.
    pub fn after_edit(self) -> Self {
        match self {
            Self::Created => Self::Created,
            _ => Self::Updated,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    /// Local id, unique across calendars.
    pub id: String,
    pub calendar_id: String,
    /// iCalendar UID / provider event id.
    pub uid: String,
    pub title: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub description: String,
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub all_day: bool,
    /// IANA zone, e.g. "Europe/Berlin". Empty means UTC.
    #[serde(default)]
    pub tz: String,
    /// `FREQ=WEEKLY;BYDAY=MO` (no `RRULE:` prefix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rrule: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exdates: Vec<i64>,
    /// For an edited instance of a recurring event: the original start of
    /// the instance it replaces. Its `uid` is the series' uid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence_id: Option<i64>,
    /// Minutes before the start.
    #[serde(default)]
    pub reminders: Vec<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// CalDAV resource href.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default)]
    pub dirty: Dirty,
}

impl Event {
    pub fn duration(&self) -> i64 {
        (self.end - self.start).max(0)
    }
}

/// One concrete occurrence of an event, what views and the shell show.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Occurrence {
    /// `"{event_id}@{start}"`.
    pub key: String,
    pub event_id: String,
    pub calendar_id: String,
    pub calendar: String,
    pub color: String,
    pub title: String,
    pub location: String,
    pub description: String,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub recurring: bool,
    pub reminders: Vec<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub calendar_id: String,
    pub uid: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    /// Due day, 00:00 UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<i64>,
    #[serde(default)]
    pub completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default)]
    pub dirty: Dirty,
}

/// A fresh random id (hex), for local ids and new iCalendar UIDs.
pub fn new_id() -> String {
    format!("{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

pub const CALENDAR_COLORS: &[&str] = &[
    "#4285F4", "#0B8043", "#D50000", "#F4511E", "#8E24AA", "#039BE5", "#E67C73", "#33B679",
    "#F6BF26", "#616161",
];
