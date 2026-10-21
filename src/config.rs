//! Preferences (`~/.config/m3-calendar/config.toml`) and account metadata
//! (`~/.config/m3-calendar/accounts.toml`). Neither holds a secret.

use crate::model::AccountKind;
use anyhow::{Context, Result};
use material_3_ui::theme::ThemeSettings;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    material_3_ui::sys::paths::config_home().join("m3-calendar")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WeekStart {
    #[default]
    Monday,
    Sunday,
    Saturday,
}

impl WeekStart {
    pub fn weekday(self) -> chrono::Weekday {
        match self {
            Self::Monday => chrono::Weekday::Mon,
            Self::Sunday => chrono::Weekday::Sun,
            Self::Saturday => chrono::Weekday::Sat,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewKind {
    #[default]
    Month,
    Week,
    Day,
    Agenda,
}

impl ViewKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "month" => Self::Month,
            "week" => Self::Week,
            "day" => Self::Day,
            "agenda" => Self::Agenda,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: ThemeSettings,
    pub week_start: WeekStart,
    pub view: ViewKind,
    /// Minutes before; `None` for no default reminder.
    pub default_reminder: Option<i64>,
    pub default_duration: i64,
    /// Calendar new events go to.
    pub default_calendar: String,
    pub show_tasks: bool,
    pub show_week_numbers: bool,
    pub time_24h: bool,
    pub sync_minutes: u64,
    /// OAuth "Desktop app" client ids the user registered (see README).
    pub google_client_id: String,
    pub microsoft_client_id: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: ThemeSettings::default(),
            week_start: WeekStart::Monday,
            view: ViewKind::Month,
            default_reminder: Some(10),
            default_duration: 60,
            default_calendar: String::new(),
            show_tasks: true,
            show_week_numbers: false,
            time_24h: true,
            sync_minutes: 15,
            google_client_id: String::new(),
            microsoft_client_id: String::new(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        write(&Self::path(), &toml::to_string_pretty(self)?)
    }

    /// The Google client id: environment first, then preferences.
    pub fn google_client(&self) -> Option<String> {
        std::env::var("M3_CALENDAR_GOOGLE_CLIENT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| Some(self.google_client_id.trim().to_string()).filter(|s| !s.is_empty()))
    }

    pub fn microsoft_client(&self) -> Option<String> {
        std::env::var("M3_CALENDAR_MS_CLIENT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| Some(self.microsoft_client_id.trim().to_string()).filter(|s| !s.is_empty()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub kind: AccountKind,
    /// What the user sees: an email address, or a name for local calendars.
    pub name: String,
    /// CalDAV server URL (iCloud and CalDAV).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    /// The OAuth client id used to sign in, so refreshing keeps working if
    /// the preference changes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id: String,
}

/// The keyring entries' `service` attribute (with `key` from
/// [`Account::secret_key`]).
pub const KEYRING_SERVICE: &str = "m3-calendar";

impl Account {
    /// Keyring key for this account's refresh token or password.
    pub fn secret_key(&self) -> String {
        format!("account:{}", self.id)
    }

    pub fn is_local(&self) -> bool {
        self.kind == AccountKind::Local
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Accounts {
    #[serde(default, rename = "account")]
    pub list: Vec<Account>,
}

impl Accounts {
    pub fn path() -> PathBuf {
        config_dir().join("accounts.toml")
    }

    pub fn load() -> Self {
        let mut a: Self = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default();
        a.ensure_local();
        a
    }

    pub fn save(&self) -> Result<()> {
        write(&Self::path(), &toml::to_string_pretty(self)?)
    }

    /// There's always an "On this computer" account to hold local calendars.
    pub fn ensure_local(&mut self) {
        if !self.list.iter().any(|a| a.kind == AccountKind::Local) {
            self.list.insert(
                0,
                Account { id: "local".into(), kind: AccountKind::Local, name: "On this computer".into(), ..Default::default() },
            );
        }
    }

    pub fn get(&self, id: &str) -> Option<&Account> {
        self.list.iter().find(|a| a.id == id)
    }

    /// Whether edits to this account's calendars must be sent somewhere.
    pub fn is_remote(&self, account_id: &str) -> bool {
        self.get(account_id).is_some_and(|a| !a.is_local())
    }
}

fn write(path: &std::path::Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_round_trip_without_secrets() {
        let mut a = Accounts::default();
        a.ensure_local();
        a.list.push(Account {
            id: "g1".into(),
            kind: AccountKind::Google,
            name: "me@example.com".into(),
            client_id: "123.apps.googleusercontent.com".into(),
            ..Default::default()
        });
        let text = toml::to_string_pretty(&a).unwrap();
        assert!(text.contains("kind = \"google\""));
        let back: Accounts = toml::from_str(&text).unwrap();
        assert_eq!(back, a);
        assert!(back.is_remote("g1"));
        assert!(!back.is_remote("local"));
        let s: Settings = toml::from_str("week_start = \"sunday\"").unwrap();
        assert_eq!(s.week_start, WeekStart::Sunday);
        assert_eq!(s.sync_minutes, 15);
    }
}
