//! Accounts: what's connected and how it's syncing, and adding Google,
//! Microsoft, iCloud, CalDAV or local calendars. Online accounts sign in
//! through the browser; nothing but metadata is written to disk.

use super::*;
use crate::config::Account;
use crate::model::AccountKind;
use crate::providers::{self, Endpoints};
use material_3_ui::common::{self, card, dialog_buttons, modal, row};
use material_3_ui::egui::RichText;

pub enum AddAccount {
    Google { client_id: String, client_secret: String },
    Microsoft { client_id: String },
    Icloud { user: String, password: String },
    Caldav { url: String, user: String, password: String },
    Local { name: String, color: String },
    Import { path: String },
}

fn kind_icon(kind: AccountKind) -> egui_material_icons::MaterialIcon {
    match kind {
        AccountKind::Google => ICON_MAIL,
        AccountKind::Microsoft => ICON_WORK,
        AccountKind::Icloud => ICON_CLOUD,
        AccountKind::Caldav => ICON_DNS,
        AccountKind::Local => ICON_COMPUTER,
    }
}

use material_3_ui::icons as egui_material_icons;

fn ago(t: i64) -> String {
    let d = chrono::Utc::now().timestamp() - t;
    match d {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{} min ago", d / 60),
        d if d < 86400 => format!("{} h ago", d / 3600),
        d => format!("{} days ago", d / 86400),
    }
}

pub fn page(app: &mut App, ui: &mut egui::Ui) {
    let p = widgets::palette(ui);
    let mut remove = None;
    let mut sync = false;
    common::page(ui, ICON_ACCOUNT_CIRCLE, "Accounts", "Sign in once and Calendar keeps your calendars and tasks in sync.", |ui| {
        card(ui, "Connected", |ui| {
            for a in app.accounts.list.clone() {
                let cals = app.calendars.iter().filter(|c| c.account_id == a.id).count();
                let status = app.store.account_status(&a.id);
                let sub = if a.is_local() {
                    format!("{cals} calendars on this computer")
                } else if let Some(e) = &status.error {
                    e.clone()
                } else {
                    format!(
                        "{} · {cals} calendars · {}",
                        a.kind.label(),
                        status.last_sync.map(|t| format!("synced {}", ago(t))).unwrap_or_else(|| "not synced yet".into())
                    )
                };
                common::list_row(ui, kind_icon(a.kind), status.error.is_none(), &a.name, &sub, |ui| {
                    if !a.is_local() {
                        if widgets::icon_button(ui, ICON_DELETE, "Remove account", true).clicked() {
                            remove = Some(a.id.clone());
                        }
                        if status.error.is_some() && widgets::button(ui, ButtonKind::Tonal, None, "Sign in again").clicked() {
                            {
                                app.add_account = Some(match a.kind {
                                    AccountKind::Google => AddAccount::Google { client_id: a.client_id.clone(), client_secret: String::new() },
                                    AccountKind::Microsoft => AddAccount::Microsoft { client_id: a.client_id.clone() },
                                    AccountKind::Icloud => AddAccount::Icloud { user: a.username.clone(), password: String::new() },
                                    _ => AddAccount::Caldav { url: a.url.clone(), user: a.username.clone(), password: String::new() },
                                });
                            }
                        }
                    }
                });
            }
            ui.add_space(4.0);
            row(ui, "Sync now", if app.daemon { "The background service syncs every few minutes and after each change." } else { "Background sync is off, so Calendar syncs while it's open." }, |ui| {
                if common::refresh_button(ui, app.sync_job.running()) {
                    sync = true;
                }
            });
        });
        card(ui, "Add an account", |ui| {
            let mut add = |ui: &mut egui::Ui, icon, title: &str, sub: &str, what: AddAccount| {
                let mut what = Some(what);
                let r = common::list_row(ui, icon, false, title, sub, |ui| {
                    if widgets::button(ui, ButtonKind::Tonal, Some(ICON_ADD), "Add").clicked() {
                        app.add_account = what.take();
                    }
                });
                if r.clicked() {
                    if let Some(w) = what.take() {
                        app.add_account = Some(w);
                    }
                }
            };
            add(ui, ICON_MAIL, "Google", "Google Calendar and Google Tasks", AddAccount::Google { client_id: app.settings.google_client().unwrap_or_default(), client_secret: String::new() });
            add(ui, ICON_WORK, "Microsoft", "Outlook.com, Microsoft 365 and To Do", AddAccount::Microsoft { client_id: app.settings.microsoft_client().unwrap_or_default() });
            add(ui, ICON_CLOUD, "iCloud", "Apple calendars and reminders", AddAccount::Icloud { user: String::new(), password: String::new() });
            add(ui, ICON_DNS, "CalDAV", "Nextcloud, Fastmail, mailbox.org, Radicale, …", AddAccount::Caldav { url: String::new(), user: String::new(), password: String::new() });
            ui.separator();
            add(ui, ICON_COMPUTER, "New local calendar", "Stays on this computer", AddAccount::Local { name: String::new(), color: crate::model::CALENDAR_COLORS[1].into() });
            let dl = dirs::download_dir().map(|d| material_3_ui::sys::paths::tilde(&d) + "/").unwrap_or_default();
            add(ui, ICON_UPLOAD_FILE, "Import an .ics file", "Into a new local calendar", AddAccount::Import { path: dl });
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Passwords and sign-in tokens are kept in your keyring (Secret Service), never in files.").size(12.0).color(p.on_surface_variant));
    });
    if sync {
        app.start_sync(ui.ctx());
    }
    if let Some(id) = remove {
        match crate::sync::remove_account(&app.store, app.secrets.as_ref(), &id) {
            Ok(()) => app.toasts.info("Account removed"),
            Err(e) => app.toasts.error(format!("{e:#}")),
        }
        app.reload();
    }
}

fn open_url(url: &str) -> anyhow::Result<()> {
    material_3_ui::sys::cmd::spawn_detached("xdg-open", &[url]).map_err(|e| anyhow::anyhow!("couldn't open the browser: {e}"))
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str, password: bool) {
    let p = widgets::palette(ui);
    ui.label(RichText::new(label).size(12.5).color(p.on_surface_variant));
    ui.add(egui::TextEdit::singleline(value).hint_text(hint).password(password).desired_width(f32::INFINITY).margin(egui::vec2(8.0, 6.0)));
    ui.add_space(6.0);
}

fn steps(ui: &mut egui::Ui, lines: &[&str]) {
    let p = widgets::palette(ui);
    egui::Frame::new()
        .fill(p.surface_container_highest)
        .corner_radius(egui::CornerRadius::same(14))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, l) in lines.iter().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{}.", i + 1)).color(p.primary));
                    ui.label(RichText::new(*l).size(12.5).color(p.on_surface));
                });
            }
        });
    ui.add_space(10.0);
}

pub fn dialogs(app: &mut App, ctx: &egui::Context) {
    if app.add_account.is_none() {
        return;
    }
    let p = app.theme.palette().clone();
    if let Some(result) = app.signin_job.take() {
        match result {
            Ok(name) => {
                app.toasts.success(format!("Added {name}"));
                app.add_account = None;
                app.reload();
                return;
            }
            Err(e) => app.toasts.error(e),
        }
    }
    let busy = app.signin_job.running();
    let mut close = false;
    let mut go = false;
    let (icon, title) = match app.add_account.as_ref().unwrap() {
        AddAccount::Google { .. } => (ICON_MAIL, "Add Google account"),
        AddAccount::Microsoft { .. } => (ICON_WORK, "Add Microsoft account"),
        AddAccount::Icloud { .. } => (ICON_CLOUD, "Add iCloud"),
        AddAccount::Caldav { .. } => (ICON_DNS, "Add CalDAV account"),
        AddAccount::Local { .. } => (ICON_COMPUTER, "New calendar"),
        AddAccount::Import { .. } => (ICON_UPLOAD_FILE, "Import calendar"),
    };
    let resp = modal(ctx, &p, "add-account", icon, title, |ui| {
        ui.set_width(480.0);
        if busy {
            common::loading(ui, "Finish signing in in your browser…");
            dialog_buttons(ui, |ui| {
                if widgets::button(ui, ButtonKind::Text, None, "Cancel").clicked() {
                    close = true;
                }
            });
            return;
        }
        let action = match app.add_account.as_mut().unwrap() {
            AddAccount::Google { client_id, client_secret } => {
                if app.settings.google_client().is_none() || client_id.is_empty() {
                    ui.label(RichText::new("Google needs an OAuth client of your own. It's free and takes a few minutes:").color(p.on_surface));
                    ui.add_space(6.0);
                    steps(ui, &[
                        "In Google Cloud Console, create a project.",
                        "Enable the Google Calendar API and the Google Tasks API.",
                        "Set up the OAuth consent screen (External) and add yourself as a test user.",
                        "Credentials → Create credentials → OAuth client ID → Desktop app.",
                        "Paste the client ID and secret here.",
                    ]);
                    if widgets::button(ui, ButtonKind::Text, Some(ICON_OPEN_IN_NEW), "Open Google Cloud Console").clicked() {
                        let _ = open_url("https://console.cloud.google.com/apis/credentials");
                    }
                    ui.add_space(6.0);
                }
                field(ui, "Client ID", client_id, "1234-abc.apps.googleusercontent.com", false);
                field(ui, "Client secret", client_secret, "Leave empty if you saved it before", true);
                "Sign in with Google"
            }
            AddAccount::Microsoft { client_id } => {
                if client_id.is_empty() {
                    ui.label(RichText::new("Microsoft needs an app registration of your own:").color(p.on_surface));
                    ui.add_space(6.0);
                    steps(ui, &[
                        "In the Azure portal, open App registrations → New registration.",
                        "Supported accounts: any organizational directory and personal Microsoft accounts.",
                        "Redirect URI: Public client/native, http://localhost",
                        "Copy the Application (client) ID here.",
                    ]);
                    if widgets::button(ui, ButtonKind::Text, Some(ICON_OPEN_IN_NEW), "Open App registrations").clicked() {
                        let _ = open_url("https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade");
                    }
                    ui.add_space(6.0);
                }
                field(ui, "Application (client) ID", client_id, "00000000-0000-0000-0000-000000000000", false);
                "Sign in with Microsoft"
            }
            AddAccount::Icloud { user, password } => {
                steps(ui, &[
                    "Sign in at account.apple.com → Sign-In and Security → App-Specific Passwords.",
                    "Create one called “Calendar” and paste it below.",
                ]);
                field(ui, "Apple Account", user, "you@icloud.com", false);
                field(ui, "App-specific password", password, "xxxx-xxxx-xxxx-xxxx", true);
                "Connect"
            }
            AddAccount::Caldav { url, user, password } => {
                field(ui, "Server", url, "https://cloud.example.com", false);
                field(ui, "User name", user, "", false);
                field(ui, "Password or app password", password, "", true);
                ui.label(RichText::new("Calendar finds your calendars from the server address.").size(12.0).color(p.on_surface_variant));
                "Connect"
            }
            AddAccount::Local { name, color } => {
                field(ui, "Name", name, "Personal", false);
                ui.horizontal(|ui| {
                    for c in crate::model::CALENDAR_COLORS {
                        let (r, resp) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::click());
                        ui.painter().circle_filled(r.center(), 12.0, hex(c));
                        if *color == *c {
                            widgets::paint_icon(ui, ICON_CHECK, r.center(), 16.0, text_on(hex(c)));
                        }
                        if resp.clicked() {
                            *color = c.to_string();
                        }
                    }
                });
                "Create"
            }
            AddAccount::Import { path } => {
                field(ui, "File", path, "~/Downloads/calendar.ics", false);
                if material_3_ui::sys::cmd::exists("zenity") && widgets::button(ui, ButtonKind::Text, Some(ICON_FOLDER_OPEN), "Choose…").clicked() {
                    if let Ok(chosen) = material_3_ui::sys::cmd::run("zenity", &["--file-selection", "--file-filter=Calendars | *.ics *.ICS"]) {
                        *path = chosen.trim().to_string();
                    }
                }
                "Import"
            }
        };
        dialog_buttons(ui, |ui| {
            if widgets::button(ui, ButtonKind::Filled, None, action).clicked() {
                go = true;
            }
            if widgets::button(ui, ButtonKind::Text, None, "Cancel").clicked() {
                close = true;
            }
        });
    });
    if (resp.should_close() && !busy) || close {
        app.add_account = None;
        return;
    }
    if go {
        start(app, ctx);
    }
}

fn start(app: &mut App, ctx: &egui::Context) {
    let secrets = app.secrets.clone();
    let settings = app.settings.clone();
    let ep = Endpoints::default();
    let Some(what) = app.add_account.as_ref() else { return };
    // Finishes adding an account: save it, then a first sync.
    let finish = move |account: Account, secret: String, settings: Settings, secrets: Arc<dyn Secrets>| -> Result<String, String> {
        let account = crate::sync::add_account(secrets.as_ref(), account, &secret).map_err(|e| format!("{e:#}"))?;
        let store = Store::open_default().map_err(|e| format!("{e:#}"))?;
        crate::sync::sync_account(&store, &account, &settings, secrets, &Endpoints::default())
            .map_err(|e| format!("Signed in, but the first sync failed: {e:#}"))?;
        Ok(account.name)
    };
    match what {
        AddAccount::Google { client_id, client_secret } => {
            let cid = client_id.trim().to_string();
            if cid.is_empty() {
                return app.toasts.error("Enter the client ID first");
            }
            if !client_secret.trim().is_empty() {
                if let Err(e) = secrets.set(&providers::google_secret_key(&cid), "Material 3 Calendar: Google client secret", client_secret.trim()) {
                    return app.toasts.error(format!("{e:#}"));
                }
            }
            app.settings.google_client_id = cid.clone();
            app.save_settings();
            app.signin_job.spawn(ctx, move || {
                let secret = secrets.get(&providers::google_secret_key(&cid)).ok().flatten();
                let cfg = providers::google_oauth(&ep, &cid, secret);
                let tokens = crate::oauth::sign_in(&cfg, open_url, std::time::Duration::from_secs(300)).map_err(|e| format!("{e:#}"))?;
                let refresh = tokens.refresh_token.ok_or("Google didn't send a refresh token; remove Calendar's access at myaccount.google.com and try again")?;
                let email = providers::google::account_email(&ep.google_api, &tokens.access_token).map_err(|e| format!("{e:#}"))?;
                finish(Account { kind: AccountKind::Google, name: email, client_id: cid, ..Default::default() }, refresh, settings, secrets)
            });
        }
        AddAccount::Microsoft { client_id } => {
            let cid = client_id.trim().to_string();
            if cid.is_empty() {
                return app.toasts.error("Enter the client ID first");
            }
            app.settings.microsoft_client_id = cid.clone();
            app.save_settings();
            app.signin_job.spawn(ctx, move || {
                let cfg = providers::microsoft_oauth(&ep, &cid);
                let tokens = crate::oauth::sign_in(&cfg, open_url, std::time::Duration::from_secs(300)).map_err(|e| format!("{e:#}"))?;
                let refresh = tokens.refresh_token.ok_or("Microsoft didn't send a refresh token")?;
                let email = providers::microsoft::account_email(&ep.graph, &tokens.access_token).map_err(|e| format!("{e:#}"))?;
                finish(Account { kind: AccountKind::Microsoft, name: email, client_id: cid, ..Default::default() }, refresh, settings, secrets)
            });
        }
        AddAccount::Icloud { user, password } | AddAccount::Caldav { user, password, .. } => {
            let (kind, url) = match what {
                AddAccount::Caldav { url, .. } => {
                    let u = url.trim();
                    (AccountKind::Caldav, if u.contains("://") { u.to_string() } else { format!("https://{u}") })
                }
                _ => (AccountKind::Icloud, providers::ICLOUD_URL.to_string()),
            };
            let (user, password) = (user.trim().to_string(), password.trim().to_string());
            if user.is_empty() || password.is_empty() || url.len() < 10 {
                return app.toasts.error("Fill in every field");
            }
            app.signin_job.spawn(ctx, move || {
                // Check the credentials before saving anything.
                let mut dav = providers::caldav::CalDav::new("check", &url, &user, &password);
                providers::Provider::calendars(&mut dav).map_err(|e| format!("{e:#}"))?;
                let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
                let name = if kind == AccountKind::Icloud || user.contains('@') { user.clone() } else { format!("{user}@{host}") };
                finish(Account { kind, name, url, username: user, ..Default::default() }, password, settings, secrets)
            });
        }
        AddAccount::Local { name, color } => {
            let name = if name.trim().is_empty() { "Calendar".to_string() } else { name.trim().to_string() };
            match crate::sync::add_local_calendar(&app.store, &name, color) {
                Ok(_) => app.toasts.success(format!("Created “{name}”")),
                Err(e) => app.toasts.error(format!("{e:#}")),
            }
            app.add_account = None;
            app.reload();
        }
        AddAccount::Import { path } => {
            let path = material_3_ui::sys::paths::expand(path.trim());
            match crate::sync::import_ics(&app.store, &path) {
                Ok((cal, n)) => {
                    app.toasts.success(format!("Imported {n} events into “{}”", cal.name));
                    app.add_account = None;
                }
                Err(e) => app.toasts.error(format!("{e:#}")),
            }
            app.reload();
        }
    }
}
