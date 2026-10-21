//! The app window: sidebar (mini month, calendars), the month / week / day
//! / agenda views, the tasks pane, the event editor, Accounts and
//! Preferences.

mod accounts;
mod editor;
mod prefs;
mod sidebar;
mod tasks;
mod time;
mod views;

use crate::cli::AppArgs;
use crate::config::{Accounts, Settings, ViewKind};
use crate::model::{Calendar, Occurrence, Task};
use crate::secrets::{Keyring, Secrets};
use crate::store::Store;
use chrono::{Datelike, Duration, Local, NaiveDate};
use material_3_ui::egui::{self, Color32, Key};
use material_3_ui::icons::icons::*;
use material_3_ui::sys::task::Task as Job;
use material_3_ui::theme::ThemeManager;
use material_3_ui::toasts::{Toast, ToastKind, Toasts};
use material_3_ui::widgets::{self, ButtonKind};
use std::sync::Arc;
use std::time::Instant;

pub use time::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Calendar,
    Accounts,
    Preferences,
}

/// What a toast button does.
#[derive(Clone, Debug)]
pub enum Action {
    UndoDelete(Box<crate::model::Event>),
    UndoTask(String),
}

pub struct App {
    pub settings: Settings,
    pub theme: ThemeManager,
    pub toasts: Toasts<Action>,
    pub store: Store,
    pub secrets: Arc<dyn Secrets>,
    pub accounts: Accounts,
    pub calendars: Vec<Calendar>,
    pub tasks: Vec<Task>,
    pub page: Page,
    pub view: ViewKind,
    /// The focused day.
    pub cursor: NaiveDate,
    /// The month the mini calendar shows.
    pub mini_month: NaiveDate,
    pub search: String,
    /// The search field moves to its own row when the bar is narrow.
    pub search_open: bool,
    pub quick_add: String,
    pub new_task: String,
    pub editor: Option<editor::Editor>,
    pub add_account: Option<accounts::AddAccount>,
    pub sync_job: Job<Vec<String>>,
    pub signin_job: Job<Result<String, String>>,
    pub daemon: bool,
    rev: i64,
    last_poll: Instant,
    occ_range: (i64, i64),
    occ: Vec<Occurrence>,
    /// Days of the mini month that have events.
    pub busy_days: std::collections::HashSet<NaiveDate>,
    busy_month: Option<NaiveDate>,
    sync_after: Option<Instant>,
    pub scroll_to_hour: bool,
    open_request: Option<(String, Option<i64>)>,
}

pub fn run(opts: AppArgs) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Calendar")
            .with_app_id("material_3_calendar")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Calendar",
        options,
        Box::new(move |cc| {
            let app = App::new(&cc.egui_ctx, opts).map_err(|e| format!("{e:#}"))?;
            Ok(Box::new(app))
        }),
    )
}

impl App {
    fn new(ctx: &egui::Context, opts: AppArgs) -> anyhow::Result<Self> {
        let settings = Settings::load();
        let theme = ThemeManager::new(ctx, &settings.appearance);
        let store = Store::open_default()?;
        crate::sync::ensure_local_calendar(&store)?;
        let today = Local::now().date_naive();
        let cursor = opts.date.unwrap_or(today);
        let mut app = Self {
            view: opts.view.unwrap_or(settings.view),
            settings,
            theme,
            toasts: Toasts::default(),
            rev: -1,
            store,
            secrets: Arc::new(Keyring),
            accounts: Accounts::load(),
            calendars: vec![],
            tasks: vec![],
            page: match opts.page.as_deref() {
                Some("accounts") => Page::Accounts,
                Some("preferences") | Some("settings") => Page::Preferences,
                _ => Page::Calendar,
            },
            cursor,
            mini_month: cursor.with_day(1).unwrap_or(cursor),
            search: String::new(),
            search_open: false,
            quick_add: String::new(),
            new_task: String::new(),
            editor: None,
            add_account: None,
            sync_job: Job::default(),
            signin_job: Job::default(),
            daemon: crate::dbus::Client::connect().is_some(),
            last_poll: Instant::now(),
            occ_range: (0, 0),
            occ: vec![],
            busy_days: Default::default(),
            busy_month: None,
            sync_after: None,
            scroll_to_hour: true,
            open_request: opts.open,
        };
        app.reload();
        // Catch up with the servers when nothing does it in the background.
        if !app.daemon && app.accounts.list.iter().any(|a| !a.is_local()) {
            app.start_sync(ctx);
        }
        Ok(app)
    }

    /// Re-reads calendars and tasks from the cache.
    pub fn reload(&mut self) {
        self.rev = self.store.revision();
        self.calendars = self.store.calendars().unwrap_or_default();
        self.tasks = self.store.tasks(false).unwrap_or_default();
        self.accounts = Accounts::load();
        self.occ_range = (0, 0);
        self.busy_month = None;
    }

    /// Occurrences overlapping [from, to), cached until the cache changes.
    pub fn occurrences(&mut self, from: i64, to: i64) -> Vec<Occurrence> {
        if self.occ_range != (from, to) {
            self.occ = self.store.occurrences(from, to, true).unwrap_or_default();
            self.occ_range = (from, to);
        }
        self.occ.clone()
    }

    pub fn ensure_busy_days(&mut self) {
        if self.busy_month == Some(self.mini_month) {
            return;
        }
        self.busy_month = Some(self.mini_month);
        let first = self.mini_month;
        let from = local_midnight(first - Duration::days(7));
        let to = local_midnight(first + Duration::days(45));
        self.busy_days.clear();
        for o in self.store.occurrences(from, to, true).unwrap_or_default() {
            for d in occurrence_days(&o) {
                self.busy_days.insert(d);
            }
        }
    }

    pub fn calendar(&self, id: &str) -> Option<&Calendar> {
        self.calendars.iter().find(|c| c.id == id)
    }

    /// After a local edit: sync soon (the daemon notices on its own).
    pub fn changed(&mut self) {
        self.reload();
        if !self.daemon && self.store.has_pending() {
            self.sync_after = Some(Instant::now() + std::time::Duration::from_secs(2));
        }
    }

    pub fn start_sync(&mut self, ctx: &egui::Context) {
        if let Some(c) = crate::dbus::Client::connect() {
            self.daemon = true;
            if let Err(e) = c.call::<_, ()>("SyncNow", &()) {
                self.toasts.error(format!("Couldn't reach the sync service: {e:#}"));
            }
            return;
        }
        if self.sync_job.running() {
            return;
        }
        let settings = self.settings.clone();
        let secrets = self.secrets.clone();
        self.sync_job.spawn(ctx, move || match Store::open_default() {
            Ok(store) => crate::sync::sync_all(&store, &settings, secrets),
            Err(e) => vec![format!("{e:#}")],
        });
    }

    pub fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.toasts.error(format!("Couldn't save preferences: {e:#}"));
        }
    }

    pub fn go(&mut self, delta: i64) {
        self.cursor = match self.view {
            ViewKind::Month => add_months(self.cursor, delta as i32),
            ViewKind::Week => self.cursor + Duration::days(7 * delta),
            ViewKind::Day => self.cursor + Duration::days(delta),
            ViewKind::Agenda => self.cursor + Duration::days(14 * delta),
        };
        self.mini_month = self.cursor.with_day(1).unwrap();
    }

    pub fn set_view(&mut self, view: ViewKind) {
        self.view = view;
        self.page = Page::Calendar;
        self.scroll_to_hour = true;
    }

    pub fn today(&mut self) {
        self.cursor = Local::now().date_naive();
        self.mini_month = self.cursor.with_day(1).unwrap();
        self.page = Page::Calendar;
        self.scroll_to_hour = true;
    }

    pub fn quick_add_now(&mut self) {
        let text = std::mem::take(&mut self.quick_add);
        if text.trim().is_empty() {
            return;
        }
        match crate::ops::quick_add(&self.store, &self.settings, &text, None) {
            Ok(ev) => {
                self.cursor = event_date(ev.start, ev.all_day);
                self.mini_month = self.cursor.with_day(1).unwrap();
                self.toasts.success(format!("Added “{}” on {}", ev.title, self.cursor.format("%a %e %b")));
                self.changed();
            }
            Err(e) => self.toasts.error(format!("{e:#}")),
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.editor.is_some() || self.add_account.is_some() {
            if self.editor.is_some() && ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.editor = None;
            }
            return;
        }
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k) && i.modifiers.is_none());
        if pressed(Key::T) {
            self.today();
        }
        if pressed(Key::ArrowLeft) {
            self.go(-1);
        }
        if pressed(Key::ArrowRight) {
            self.go(1);
        }
        if pressed(Key::N) {
            self.new_event_at(self.cursor, Some(9 * 60));
        }
        for (k, v) in [(Key::Num1, ViewKind::Month), (Key::Num2, ViewKind::Week), (Key::Num3, ViewKind::Day), (Key::Num4, ViewKind::Agenda)] {
            if pressed(k) {
                self.set_view(v);
            }
        }
    }

    /// Opens the editor on a new event; `minute` of the day, or all-day.
    pub fn new_event_at(&mut self, day: NaiveDate, minute: Option<i64>) {
        let ev = match minute {
            Some(m) => crate::ops::new_event(&self.settings, local_midnight(day) + m * 60, false),
            None => crate::ops::new_event(&self.settings, utc_midnight(day), true),
        };
        let cal = crate::ops::default_calendar(&self.store, &self.settings).unwrap_or_default();
        self.editor = Some(editor::Editor::new(crate::model::Event { calendar_id: cal, ..ev }, None, true, &self.settings));
    }

    pub fn open_occurrence(&mut self, o: &Occurrence) {
        if let Ok(Some(ev)) = self.store.event(&o.event_id) {
            self.editor = Some(editor::Editor::new(ev, Some(o.start), false, &self.settings));
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.theme.palette().clone();
        ui.horizontal(|ui| {
            ui.set_height(44.0);
            ui.spacing_mut().item_spacing.x = 6.0;
            if widgets::button(ui, ButtonKind::Outlined, None, "Today").on_hover_text("T").clicked() {
                self.today();
            }
            if widgets::icon_button(ui, ICON_CHEVRON_LEFT, "Previous (←)", true).clicked() {
                self.go(-1);
            }
            if widgets::icon_button(ui, ICON_CHEVRON_RIGHT, "Next (→)", true).clicked() {
                self.go(1);
            }
            ui.add_space(6.0);
            // The title gets what's left after the controls on the right
            // (about 330 px), truncated if need be.
            let room = (ui.available_width() - 290.0).max(80.0);
            let title = widgets::truncated_galley(ui, &self.title(), widgets::title_font(22.0), p.on_surface, room);
            ui.label(title).on_hover_text(self.title());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = self.sync_job.running();
                if widgets::icon_button(ui, ICON_SYNC, if busy { "Syncing…" } else { "Sync now" }, !busy).clicked() {
                    self.start_sync(ui.ctx());
                }
                if widgets::icon_button_sized(ui, ICON_TASK_ALT, "Tasks", true, 36.0, self.settings.show_tasks).clicked() {
                    self.settings.show_tasks = !self.settings.show_tasks;
                    self.save_settings();
                }
                let mut view = self.view;
                let narrow = ui.available_width() < 700.0;
                let label = |s: &'static str| if narrow { "" } else { s };
                if widgets::segmented(
                    ui,
                    &mut view,
                    &[
                        (ViewKind::Agenda, ICON_VIEW_AGENDA, label("Agenda")),
                        (ViewKind::Day, ICON_CALENDAR_VIEW_DAY, label("Day")),
                        (ViewKind::Week, ICON_CALENDAR_VIEW_WEEK, label("Week")),
                        (ViewKind::Month, ICON_CALENDAR_VIEW_MONTH, label("Month")),
                    ],
                ) {
                    self.set_view(view);
                }
                let w = (ui.available_width() - 16.0).min(260.0);
                if w >= 160.0 {
                    self.search_open = false;
                    material_3_ui::common::search_field(ui, &mut self.search, "Search events", w);
                } else if widgets::icon_button_sized(ui, ICON_SEARCH, "Search", true, 36.0, self.search_open || !self.search.is_empty()).clicked() {
                    self.search_open = !self.search_open;
                    if !self.search_open {
                        self.search.clear();
                    }
                }
            });
        });
        if self.search_open {
            ui.add_space(4.0);
            let r = material_3_ui::common::search_field(ui, &mut self.search, "Search events", ui.available_width());
            if r.gained_focus() || self.search.is_empty() {
                r.request_focus();
            }
        }
    }

    fn title(&self) -> String {
        if !self.search.trim().is_empty() {
            return format!("Results for “{}”", self.search.trim());
        }
        match self.view {
            ViewKind::Month | ViewKind::Agenda => self.cursor.format("%B %Y").to_string(),
            ViewKind::Day => self.cursor.format("%A, %e %B %Y").to_string(),
            ViewKind::Week => {
                let (a, b) = week_range(self.cursor, self.settings.week_start);
                let b = b - Duration::days(1);
                // The year only when it isn't this one.
                let this_year = a.year() == Local::now().year() && b.year() == a.year();
                let year = if this_year { String::new() } else { format!(" {}", a.year()) };
                if a.month() == b.month() {
                    format!("{} – {} {}{year}", a.day(), b.day(), a.format("%b"))
                } else if a.year() == b.year() {
                    format!("{} – {}{year}", a.format("%e %b").to_string().trim(), b.format("%e %b"))
                } else {
                    format!("{} – {}", a.format("%e %b %Y"), b.format("%e %b %Y"))
                }
            }
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if self.last_poll.elapsed().as_secs_f32() >= 1.0 {
            self.last_poll = Instant::now();
            if self.store.revision() != self.rev {
                self.reload();
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        if let Some(errors) = self.sync_job.take() {
            self.reload();
            for e in errors {
                self.toasts.error(e);
            }
        }
        if self.sync_after.is_some_and(|t| Instant::now() >= t) {
            self.sync_after = None;
            self.start_sync(ctx);
        }
        if let Some((id, at)) = self.open_request.take() {
            if let Ok(Some(ev)) = self.store.event(&id) {
                let start = at.unwrap_or(ev.start);
                self.cursor = event_date(start, ev.all_day);
                self.mini_month = self.cursor.with_day(1).unwrap();
                self.editor = Some(editor::Editor::new(ev, at, false, &self.settings));
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.theme.poll(&self.settings.appearance);
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.handle_keys(&ctx);
        let p = self.theme.palette().clone();
        let surface = egui::Frame::new().fill(p.surface);

        egui::Panel::left("sidebar")
            .exact_size(256.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(surface.inner_margin(egui::Margin::same(12)))
            .show(ui, |ui| sidebar::show(self, ui));

        if self.page == Page::Calendar && self.settings.show_tasks {
            egui::Panel::right("tasks")
                .exact_size(264.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(surface.inner_margin(egui::Margin { left: 0, right: 12, top: 12, bottom: 12 }))
                .show(ui, |ui| tasks::show(self, ui));
        }

        egui::CentralPanel::default()
            .frame(surface.inner_margin(egui::Margin { left: 0, right: 12, top: 12, bottom: 12 }))
            .show(ui, |ui| {
                egui::Frame::new()
                    .fill(p.surface_container_low)
                    .corner_radius(egui::CornerRadius::same(24))
                    .inner_margin(egui::Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        match self.page {
                            Page::Calendar => {
                                self.top_bar(ui);
                                ui.add_space(6.0);
                                if !self.search.trim().is_empty() {
                                    views::search(self, ui);
                                } else {
                                    match self.view {
                                        ViewKind::Month => views::month(self, ui),
                                        ViewKind::Week => views::time_grid(self, ui, 7),
                                        ViewKind::Day => views::time_grid(self, ui, 1),
                                        ViewKind::Agenda => views::agenda(self, ui),
                                    }
                                }
                            }
                            Page::Accounts => accounts::page(self, ui),
                            Page::Preferences => prefs::page(self, ui),
                        }
                    });
            });

        editor::show(self, &ctx);
        accounts::dialogs(self, &ctx);

        for action in self.toasts.show(&ctx, &p, 24.0) {
            match action {
                Action::UndoDelete(ev) => {
                    if let Err(e) = crate::ops::save_event(&self.store, &self.settings, crate::model::Event { id: String::new(), uid: String::new(), etag: None, href: None, ..*ev }) {
                        self.toasts.error(format!("{e:#}"));
                    }
                    self.changed();
                }
                Action::UndoTask(id) => {
                    let _ = crate::ops::set_task_completed(&self.store, &id, false);
                    self.changed();
                }
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.theme.palette().surface.to_normalized_gamma_f32()
    }
}

pub fn toast_with(text: impl Into<String>, label: &str, action: Action) -> Toast<Action> {
    let mut t = Toast::new(text, ToastKind::Info);
    t.action = Some((label.into(), action));
    t
}
