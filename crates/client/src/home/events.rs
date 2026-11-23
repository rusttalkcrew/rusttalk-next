//! The Events page: a month calendar with the events of a day beside it, or below it when
//! narrow, and the form to suggest or add one.

use chrono::{DateTime, Datelike, Days, Local, NaiveDate, NaiveTime, TimeZone};
use eframe::egui::{
    self, Align, Align2, CursorIcon, FontId, Label, Layout, Rect, Response, RichText, ScrollArea,
    Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, accesskit::Role, text::LayoutJob,
};
use rusttalk_core::calendar::{
    Answer, Details, HORIZON, MAX_DESCRIPTION, MAX_PLACE, MAX_TITLE, Place, REMIND_EVERY,
};

use super::Home;
use crate::{
    net::{Change, Command},
    notify::unix_now,
    server::{Calendar, Event, Id, Server},
    ui::{self, Look},
};

const DAYS_A_WEEK: usize = 7;
/// Narrower than this, the day's events go below the month.
const SIDE_BY_SIDE: f32 = 760.0;
const AGENDA_WIDTH: f32 = 300.0;
/// Narrower days show dots instead of titles.
const TITLES_FROM: f32 = 64.0;
const DAY_HEIGHT: f32 = 76.0;
const SMALL_DAY_HEIGHT: f32 = 44.0;
const PICKER_DAY_HEIGHT: f32 = 30.0;
const DAY_GAP: f32 = 4.0;
const CHIP_HEIGHT: f32 = 18.0;
const TIME_WIDTH: f32 = 72.0;
/// A new event starts at this hour and lasts this long.
const START_HOUR: u32 = 20;
const LASTS: i64 = 2 * 60 * 60;

/// The page between frames.
#[derive(Default)]
pub struct View {
    /// The first day of the month shown; this month until changed.
    month: Option<NaiveDate>,
    /// The picked day; without one, what comes next is listed.
    day: Option<NaiveDate>,
    form: Option<Form>,
    confirm: Option<Confirm>,
    /// The event whose answers are open in a window.
    people: Option<Id>,
    error: Option<String>,
}

struct Confirm {
    id: Id,
    question: &'static str,
    action: &'static str,
}

impl View {
    /// The server answered a change.
    pub fn changed(&mut self, result: Result<(), String>) {
        match (&mut self.form, result) {
            (Some(form), result) if form.sending => match result {
                Ok(()) => self.form = None,
                Err(error) => {
                    form.sending = false;
                    form.error = Some(error);
                }
            },
            (_, result) => self.error = result.err(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Start,
    End,
}

/// A day and the time typed for it.
struct When {
    date: NaiveDate,
    time: String,
}

impl When {
    fn new(at: i64) -> Self {
        let at = local(at);
        Self {
            date: at.date_naive(),
            time: at.format("%H:%M").to_string(),
        }
    }

    fn unix(&self) -> Option<i64> {
        // A time the clocks skip has none; one they repeat means the first.
        Local
            .from_local_datetime(&self.date.and_time(parse_time(&self.time)?))
            .earliest()
            .map(|at| at.timestamp())
    }
}

/// Where the form says the event happens.
#[derive(Clone, Copy, PartialEq)]
enum Where {
    Nowhere,
    Irl,
    Channel,
}

struct Form {
    title: String,
    description: String,
    start: When,
    end: When,
    place: Where,
    address: String,
    channel: Option<Id>,
    /// The date being picked, and the month shown for it.
    picking: Option<(Side, NaiveDate)>,
    sending: bool,
    error: Option<String>,
}

impl Form {
    fn new(day: Option<NaiveDate>, now: i64) -> Self {
        let next_hour = now - now.rem_euclid(3600) + 3600;
        let start = day
            .and_then(|day| day.and_hms_opt(START_HOUR, 0, 0))
            .and_then(|at| Local.from_local_datetime(&at).earliest())
            .map(|at| at.timestamp())
            .filter(|&at| at > now)
            .unwrap_or(next_hour);
        Self {
            title: String::new(),
            description: String::new(),
            start: When::new(start),
            end: When::new(start + LASTS),
            place: Where::Nowhere,
            address: String::new(),
            channel: None,
            picking: None,
            sending: false,
            error: None,
        }
    }

    fn pick(&mut self, side: Side, date: NaiveDate) {
        match side {
            // The end keeps its distance from the start.
            Side::Start => {
                self.end.date += date - self.start.date;
                self.start.date = date;
            }
            Side::End => self.end.date = date,
        }
    }

    fn details(&self, now: i64) -> Result<Details, &'static str> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err(t!("events.need_title"));
        }
        let (Some(start), Some(end)) = (self.start.unix(), self.end.unix()) else {
            return Err(t!("events.bad_time"));
        };
        if end <= start {
            return Err(t!("events.end_first"));
        }
        if end <= now || end > now + HORIZON {
            return Err(t!("events.bad_range"));
        }
        let place = match self.place {
            Where::Nowhere => Place::Nowhere,
            Where::Irl if self.address.trim().is_empty() => return Err(t!("events.need_address")),
            Where::Irl => Place::Irl(self.address.trim().to_owned()),
            Where::Channel => Place::Channel(self.channel.ok_or(t!("events.need_channel"))?),
        };
        Ok(Details {
            title: title.to_owned(),
            description: self.description.trim().to_owned(),
            start,
            end,
            place,
        })
    }
}

/// `20`, `20:15` or `2015`.
fn parse_time(text: &str) -> Option<NaiveTime> {
    let text = text.trim();
    let (hours, minutes) = match text.split_once(':') {
        Some((hours, minutes)) if minutes.len() == 2 => (hours, minutes),
        Some(_) => return None,
        None if text.len() <= 2 => (text, "00"),
        None if text.len() <= 4 => text.split_at_checked(text.len() - 2)?,
        None => return None,
    };
    let number = |part: &str| {
        (part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    NaiveTime::from_hms_opt(number(hours)?, number(minutes)?, 0)
}

fn local(at: i64) -> DateTime<Local> {
    DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
}

fn first_of_month(day: NaiveDate) -> NaiveDate {
    day.with_day(1).unwrap_or(day)
}

fn next_month(first: NaiveDate) -> NaiveDate {
    first_of_month(first + Days::new(32))
}

fn previous_month(first: NaiveDate) -> NaiveDate {
    first_of_month(first - Days::new(1))
}

/// Every day drawn for a month: whole weeks from Monday.
fn month_days(first: NaiveDate) -> Vec<NaiveDate> {
    let start = first - Days::new(first.weekday().num_days_from_monday().into());
    let last = next_month(first) - Days::new(1);
    let end = last + Days::new((6 - last.weekday().num_days_from_monday()).into());
    start.iter_days().take_while(|day| *day <= end).collect()
}

fn happens_on(details: &Details, day: NaiveDate) -> bool {
    // An event that ends at midnight is not on the next day.
    let (first, last) = (local(details.start), local(details.end - 1));
    (first.date_naive()..=last.date_naive()).contains(&day)
}

/// The events to show, earliest first. Ones this device can't read go last.
fn current(calendar: &Calendar, now: i64) -> Vec<&Event> {
    let mut events: Vec<&Event> = (calendar.events.iter())
        .filter(|event| event.record.remove_at > now)
        .collect();
    events.sort_by_key(|event| {
        let start = event.details.as_ref().map(|details| details.start);
        (start.is_none(), start, event.record.id)
    });
    events
}

/// How many suggestions wait for this person.
pub fn waiting(server: &Server) -> usize {
    let Some(calendar) = &server.dir.calendar else {
        return 0;
    };
    if !server.dir.perms.manage_events {
        return 0;
    }
    (current(calendar, unix_now()).iter())
        .filter(|event| !event.record.approved)
        .count()
}

pub fn enabled(server: &Server) -> bool {
    (server.dir.calendar.as_ref()).is_some_and(|calendar| calendar.enabled)
}

/// The entry above the channels. True when clicked.
pub fn nav_row(ui: &mut Ui, server: &Server, selected: bool) -> bool {
    let label = t!("events.title");
    let waiting = waiting(server);
    let spoken = if waiting > 0 {
        format!("{label}, {}", t!("events.waiting_count", count = waiting))
    } else {
        label.to_owned()
    };
    let trailing = if waiting > 0 { 28.0 } else { 0.0 };
    let row = ui::nav_row_trailing(ui, "📅", label, selected, Role::Button, &spoken, trailing);
    if waiting > 0 {
        let at = egui::pos2(row.rect.right() - 16.0, row.rect.center().y);
        ui::mention_badge(ui, at, waiting as u32, ui::colors(ui).side);
    }
    row.clicked()
}

/// What was clicked on the page.
#[derive(Default)]
struct Asked {
    add: bool,
    /// A channel to open: where an event happens.
    open: Option<Id>,
    /// Approve, answer or remind.
    change: Option<Change>,
    /// Show who comes to this event.
    people: Option<Id>,
    confirm: Option<Confirm>,
}

pub fn pane(ui: &mut Ui, home: &mut Home, index: usize) {
    let title = t!("events.title");
    super::pane_title(
        ui,
        "events-title",
        title,
        Some(&mut home.layout),
        false,
        None,
    );

    let now = unix_now();
    let today = local(now).date_naive();
    // Taken out, so the page can read the server while it changes.
    let mut view = std::mem::take(&mut home.servers[index].events);
    let server = &home.servers[index];
    let mut asked = Asked::default();
    if let Some(calendar) = &server.dir.calendar {
        let events = current(calendar, now);
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(ui::SPACE_S);
                if ui.available_width() >= SIDE_BY_SIDE {
                    ui.horizontal_top(|ui| {
                        let gap = ui::SPACE_XL + ui.spacing().item_spacing.x * 2.0;
                        let width = ui.available_width() - AGENDA_WIDTH - gap;
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            month(ui, &mut view, &events, today);
                        });
                        ui.add_space(ui::SPACE_XL);
                        ui.vertical(|ui| {
                            ui.set_width(AGENDA_WIDTH);
                            agenda(ui, server, &mut view, &events, now, &mut asked);
                        });
                    });
                } else {
                    month(ui, &mut view, &events, today);
                    ui.add_space(ui::SPACE_L);
                    agenda(ui, server, &mut view, &events, now, &mut asked);
                }
            });
    }
    if let Some(change) = asked.change {
        view.error = None;
        server.send(Command::Calendar(change));
    }
    if asked.people.is_some() {
        view.people = asked.people;
    }
    if asked.confirm.is_some() {
        view.error = None;
        view.confirm = asked.confirm;
    }
    if asked.add {
        view.error = None;
        view.form = Some(Form::new(view.day, now));
    }
    if let Some(change) = form_dialog(ui.ctx(), server, &mut view.form, now)
        .or_else(|| confirm_dialog(ui.ctx(), &mut view.confirm))
        .or_else(|| people_dialog(ui.ctx(), server, &mut view.people, now))
    {
        server.send(Command::Calendar(change));
    }
    home.servers[index].events = view;
    if let Some(channel) = asked.open {
        home.servers[index].open = Some(channel);
        home.layout.list = false;
    }
}

/// `name` from a `|` separated list in the current language.
fn listed(list: &'static str, at: usize) -> &'static str {
    list.split('|').nth(at).unwrap_or_default()
}

fn month_title(first: NaiveDate) -> String {
    let name = listed(t!("events.months"), first.month0() as usize);
    t!("events.month_year", month = name, year = first.year())
}

fn date_text(day: NaiveDate) -> String {
    day.format(t!("events.date")).to_string()
}

/// The month's name with its arrows. True when "Today" was clicked.
fn month_header(ui: &mut Ui, month: &mut NaiveDate, small: bool) -> bool {
    ui.horizontal(|ui| {
        let size = Vec2::splat(ui::ICON_BUTTON);
        if ui::icon_button(ui, "⏴", t!("events.previous"), Look::Ghost, size).clicked() {
            *month = previous_month(*month);
        }
        if ui::icon_button(ui, "⏵", t!("events.next"), Look::Ghost, size).clicked() {
            *month = next_month(*month);
        }
        let name = month_title(*month);
        ui.add(
            Label::new(if small {
                ui::bold(name)
            } else {
                ui::title(name)
            })
            .truncate(),
        );
        !small
            && ui
                .with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui::secondary(ui, t!("events.today")).clicked()
                })
                .inner
    })
    .inner
}

fn weekdays(ui: &mut Ui, width: f32) {
    let size = Vec2::new(ui.available_width(), ui::TEXT_CAPTION + ui::SPACE_S);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    for at in 0..DAYS_A_WEEK {
        let center = egui::pos2(
            rect.left() + at as f32 * (width + DAY_GAP) + width / 2.0,
            rect.center().y,
        );
        ui.painter().text(
            center,
            Align2::CENTER_CENTER,
            listed(t!("events.weekdays"), at),
            FontId::new(ui::TEXT_CAPTION, ui::bold_family()),
            ui::colors(ui).muted,
        );
    }
}

/// A month's days in weeks of seven. `draw` fills each day; returns the one clicked.
fn days(
    ui: &mut Ui,
    first: NaiveDate,
    height: impl Fn(f32) -> f32,
    mut draw: impl FnMut(&mut Ui, Rect, &Response, NaiveDate),
) -> Option<NaiveDate> {
    let gaps = DAY_GAP * (DAYS_A_WEEK - 1) as f32;
    let width = ((ui.available_width() - gaps) / DAYS_A_WEEK as f32).max(1.0);
    let height = height(width);
    weekdays(ui, width);
    let days = month_days(first);
    let rows = (days.len() / DAYS_A_WEEK) as f32;
    let size = Vec2::new(ui.available_width(), rows * height + (rows - 1.0) * DAY_GAP);
    let (grid, response) = ui.allocate_exact_size(size, Sense::hover());
    let mut picked = None;
    for (at, day) in days.into_iter().enumerate() {
        let corner = Vec2::new(
            (at % DAYS_A_WEEK) as f32 * (width + DAY_GAP),
            (at / DAYS_A_WEEK) as f32 * (height + DAY_GAP),
        );
        let rect = Rect::from_min_size(grid.min + corner, Vec2::new(width, height));
        let cell = ui.interact(rect, response.id.with(day), Sense::click());
        draw(ui, rect, &cell, day);
        ui::focus_ring(ui, &cell, ui::RADIUS_ROW);
        if cell.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            picked = Some(day);
        }
    }
    picked
}

/// A day's fill and number.
fn day_base(
    ui: &Ui,
    rect: Rect,
    cell: &Response,
    (day, month, today): (NaiveDate, NaiveDate, NaiveDate),
    selected: bool,
    number: Align2,
) {
    let c = ui::colors(ui);
    let fill = if selected {
        c.selected
    } else if cell.hovered() {
        c.hover
    } else {
        c.field
    };
    let painter = ui.painter();
    painter.rect_filled(rect, ui::RADIUS_ROW, fill);
    if day == today {
        let stroke = Stroke::new(1.5, ui::accent(ui).text(c.dark));
        painter.rect_stroke(rect, ui::RADIUS_ROW, stroke, StrokeKind::Inside);
    }
    let color = if day.month() == month.month() {
        c.strong
    } else {
        c.muted
    };
    painter.text(
        number.pos_in_rect(&rect.shrink(6.0)),
        number,
        day.day(),
        FontId::new(ui::TEXT_SMALL, ui::bold_family()),
        color,
    );
}

fn month(ui: &mut Ui, view: &mut View, events: &[&Event], today: NaiveDate) {
    let mut shown = view.month.unwrap_or_else(|| first_of_month(today));
    if month_header(ui, &mut shown, false) {
        shown = first_of_month(today);
        view.day = Some(today);
    }
    ui.add_space(ui::SPACE_S);
    let height = |width| {
        if width >= TITLES_FROM {
            DAY_HEIGHT
        } else {
            SMALL_DAY_HEIGHT
        }
    };
    let selected = view.day;
    let picked = days(ui, shown, height, |ui, rect, cell, day| {
        let on_day: Vec<(&Event, &Details)> = (events.iter())
            .filter_map(|event| Some((*event, event.details.as_ref()?)))
            .filter(|(_, details)| happens_on(details, day))
            .collect();
        let chosen = selected == Some(day);
        cell.widget_info(|| {
            let mut label = date_text(day);
            if !on_day.is_empty() {
                label += ", ";
                label += &t!("events.day_count", count = on_day.len());
            }
            WidgetInfo::selected(Role::Button, true, chosen, label)
        });
        let wide = rect.width() >= TITLES_FROM;
        let number = if wide {
            Align2::LEFT_TOP
        } else {
            Align2::CENTER_TOP
        };
        day_base(ui, rect, cell, (day, shown, today), chosen, number);
        if wide {
            chips(ui, rect, &on_day);
        } else if !on_day.is_empty() {
            let at = egui::pos2(rect.center().x, rect.bottom() - 9.0);
            let approved = on_day.iter().any(|(event, _)| event.record.approved);
            let fill = if approved {
                ui::accent(ui).text(ui::colors(ui).dark)
            } else {
                ui::colors(ui).muted
            };
            ui.painter().circle_filled(at, 3.0, fill);
        }
    });
    if let Some(day) = picked {
        // A second click shows what comes next again.
        view.day = (view.day != Some(day)).then_some(day);
        shown = first_of_month(day);
    }
    view.month = Some(shown);
}

/// Up to two titles in a day, or one and how many more.
fn chips(ui: &Ui, rect: Rect, events: &[(&Event, &Details)]) {
    let c = ui::colors(ui);
    let line = |at: usize| {
        Rect::from_min_size(
            egui::pos2(
                rect.left() + 4.0,
                rect.top() + 26.0 + at as f32 * (CHIP_HEIGHT + 3.0),
            ),
            Vec2::new(rect.width() - 8.0, CHIP_HEIGHT),
        )
    };
    let shown = if events.len() > 2 { 1 } else { events.len() };
    for (at, (event, details)) in events.iter().take(shown).enumerate() {
        let (fill, color) = if event.record.approved {
            (ui::accent(ui).base, ui::ON_FILL)
        } else {
            (egui::Color32::TRANSPARENT, c.text)
        };
        let rect = line(at);
        ui.painter().rect_filled(rect, ui::RADIUS_S, fill);
        if !event.record.approved {
            // Outlined, as the button fill vanishes on a day in light themes.
            let stroke = Stroke::new(1.0, c.muted);
            ui.painter()
                .rect_stroke(rect, ui::RADIUS_S, stroke, StrokeKind::Inside);
        }
        let font = FontId::new(ui::TEXT_CAPTION, crate::emoji::family(false));
        let width = rect.width() - 8.0;
        let mut job = LayoutJob::simple(one_line(&details.title), font, color, width);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = ui.painter().layout_job(job);
        let at = egui::pos2(rect.left() + 4.0, rect.center().y - galley.size().y / 2.0);
        ui.painter().with_clip_rect(rect).galley(at, galley, color);
    }
    if events.len() > shown {
        ui.painter().text(
            line(shown).left_center() + Vec2::new(4.0, 0.0),
            Align2::LEFT_CENTER,
            t!("events.more", number = events.len() - shown),
            FontId::proportional(ui::TEXT_CAPTION),
            c.muted,
        );
    }
}

/// Titles come from other people's apps: keep them on one line.
fn one_line(text: &str) -> String {
    text.replace(char::is_control, " ")
}

fn when_text(details: &Details) -> String {
    let (start, end) = (local(details.start), local(details.end));
    let clock = |at| crate::i18n::time(at, t!("time.clock"));
    let (first, last) = (start.date_naive(), end.date_naive());
    if first == last {
        format!("{}, {} – {}", date_text(first), clock(start), clock(end))
    } else {
        format!(
            "{}, {} – {}, {}",
            date_text(first),
            clock(start),
            date_text(last),
            clock(end)
        )
    }
}

fn agenda(
    ui: &mut Ui,
    server: &Server,
    view: &mut View,
    events: &[&Event],
    now: i64,
    asked: &mut Asked,
) {
    let perms = server.dir.perms;
    if perms.create_events || perms.suggest_events {
        let label = if perms.create_events {
            t!("events.add")
        } else {
            t!("events.suggest")
        };
        asked.add = ui::primary(ui, label).clicked();
    }
    ui::error_text(ui, view.error.as_deref());
    // Suggestions come first for who decides on them, whatever day is picked.
    let (waiting, rest): (Vec<&Event>, Vec<&Event>) =
        (events.iter().copied()).partition(|event| perms.manage_events && !event.record.approved);
    if !waiting.is_empty() {
        ui::section(ui, t!("events.waiting"));
        for event in waiting {
            card(ui, server, event, now, asked);
        }
    }
    let listed: Vec<&Event> = match view.day {
        Some(day) => {
            ui::section(ui, &date_text(day));
            (rest.into_iter())
                .filter(|event| (event.details.as_ref()).is_some_and(|d| happens_on(d, day)))
                .collect()
        }
        None => {
            ui::section(ui, t!("events.upcoming"));
            rest
        }
    };
    if listed.is_empty() {
        ui.weak(match view.day {
            Some(_) => t!("events.none_that_day"),
            None => t!("events.none"),
        });
    }
    for event in listed {
        card(ui, server, event, now, asked);
    }
    if view.day.is_some() {
        ui.add_space(ui::SPACE_S);
        if ui::secondary(ui, t!("events.show_upcoming")).clicked() {
            view.day = None;
        }
    }
}

fn card(ui: &mut Ui, server: &Server, event: &Event, now: i64, asked: &mut Asked) {
    let (c, record) = (ui::colors(ui), &event.record);
    ui.add_space(ui::SPACE_XS);
    ui.push_id(record.id, |ui| {
        ui::card(ui, |ui| {
            match &event.details {
                Some(details) => {
                    ui.add(Label::new(ui::bold(one_line(&details.title))).wrap());
                    ui.add(Label::new(RichText::new(when_text(details)).color(c.muted)).wrap());
                    place_line(ui, server, &details.place, asked);
                    if !details.description.is_empty() {
                        let text =
                            RichText::new(&details.description).family(crate::emoji::family(false));
                        ui.add(Label::new(text).wrap());
                    }
                    if details.end <= now {
                        ui.label(ui::caption(t!("events.over")));
                    } else if details.start <= now && record.approved {
                        ui.colored_label(c.success, t!("events.running"));
                    }
                }
                None => {
                    ui.add(Label::new(RichText::new(t!("events.unreadable")).weak()).wrap());
                }
            }
            let name = server.member_name(&record.author);
            ui.label(ui::caption(t!("events.by", name = name)));
            if !record.approved {
                ui.colored_label(c.warning, t!("events.not_approved"));
            }
            answers(ui, server, event, now, asked);
            actions(ui, server, event, now, asked);
        });
    });
}

fn actions(ui: &mut Ui, server: &Server, event: &Event, now: i64, asked: &mut Asked) {
    let record = &event.record;
    let manages = server.dir.perms.manage_events;
    let own = record.author == server.dir.me;
    if !own && !manages {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        let approvable =
            !record.approved && (event.details.as_ref()).is_some_and(|details| details.end > now);
        if manages && approvable && ui::primary(ui, t!("events.approve")).clicked() {
            asked.change = Some(Change::Approve(Box::new(record.clone())));
        }
        let (action, question) = match (record.approved, own) {
            (true, _) => (t!("events.cancel"), t!("events.cancel_question")),
            (false, true) => (t!("events.withdraw"), t!("events.withdraw_question")),
            (false, false) => (t!("events.decline"), t!("events.decline_question")),
        };
        if ui::secondary(ui, action).clicked() {
            asked.confirm = Some(Confirm {
                id: record.id,
                question,
                action,
            });
        }
    });
}

/// Who is going, who isn't, who hasn't answered, and (while it runs in a voice channel)
/// who said yes but isn't there.
struct People {
    going: Vec<Id>,
    not_going: Vec<Id>,
    open: Vec<Id>,
    away: Vec<Id>,
}

fn people(server: &Server, event: &Event, now: i64) -> People {
    let answer = |person: &Id| {
        (event.answers.iter())
            .find(|(who, _)| who == person)
            .map(|(_, answer)| *answer)
    };
    let mut people = People {
        going: Vec::new(),
        not_going: Vec::new(),
        open: Vec::new(),
        away: Vec::new(),
    };
    for member in &server.dir.members {
        match answer(&member.person) {
            Some(Answer::Going) => people.going.push(member.person),
            Some(Answer::NotGoing) => people.not_going.push(member.person),
            None => people.open.push(member.person),
        }
    }
    if let Some(details) = &event.details
        && let Place::Channel(channel) = details.place
        && server
            .channel(&channel)
            .is_some_and(|channel| channel.voice)
        && (details.start..details.end).contains(&now)
    {
        let inside = server.voice.get(&channel).map_or(&[][..], Vec::as_slice);
        people.away = (people.going.iter())
            .filter(|person| !inside.iter().any(|peer| peer.person == **person))
            .copied()
            .collect();
    }
    people
}

/// Going and Not going, and how many answered, for an approved event still to come.
fn answers(ui: &mut Ui, server: &Server, event: &Event, now: i64, asked: &mut Asked) {
    let Some(details) = &event.details else {
        return;
    };
    if !event.record.approved || details.end <= now {
        return;
    }
    let mine = (event.answers.iter())
        .find(|(person, _)| *person == server.dir.me)
        .map(|(_, answer)| *answer);
    ui.add_space(ui::SPACE_XS);
    ui.horizontal_wrapped(|ui| {
        for (answer, label) in [
            (Answer::Going, t!("events.going")),
            (Answer::NotGoing, t!("events.not_going")),
        ] {
            let size = Vec2::new(0.0, ui::CONTROL_HEIGHT);
            let picked = mine == Some(answer);
            let button = ui::choice_button(ui, picked, label, size);
            if button.clicked() && !picked {
                asked.change = Some(Change::Answer(event.record.id, answer));
            }
        }
    });
    let count = people(server, event, now);
    let summary = t!(
        "events.answer_counts",
        going = count.going.len(),
        open = count.open.len()
    );
    if ui.link(summary).on_hover_text(t!("events.who")).clicked() {
        asked.people = Some(event.record.id);
    }
}

/// Reminders for unanswered events: event ID, reminder timestamp, removal time and popup text.
pub fn reminders(server: &Server, now: i64) -> Vec<(Id, i64, i64, String)> {
    let Some(calendar) = &server.dir.calendar else {
        return Vec::new();
    };
    let me = server.dir.me;
    (calendar.events.iter())
        .filter(|event| event.record.approved)
        .filter(|event| !event.answers.iter().any(|(person, _)| *person == me))
        .filter_map(|event| {
            let at = event.record.reminded_at?;
            let details = event.details.as_ref().filter(|d| d.end > now)?;
            let body = format!(
                "{} · {}\n{}",
                one_line(&details.title),
                when_text(details),
                t!("events.reminder_body")
            );
            Some((event.record.id, at, event.record.remove_at, body))
        })
        .collect()
}

fn people_dialog(
    ctx: &egui::Context,
    server: &Server,
    open: &mut Option<Id>,
    now: i64,
) -> Option<Change> {
    let id = (*open)?;
    let event = (server.dir.calendar.iter())
        .flat_map(|calendar| &calendar.events)
        .find(|event| event.record.id == id);
    let Some((event, details)) = event.and_then(|e| Some((e, e.details.as_ref()?))) else {
        *open = None;
        return None;
    };
    let lists = people(server, event, now);
    let record = &event.record;
    let may_remind = record.author == server.dir.me || server.dir.perms.manage_events;
    let next = record.reminded_at.map(|at| at + REMIND_EVERY);
    let mut change = None;
    let title = one_line(&details.title);
    let room = (ctx.content_rect().height() - 200.0).max(160.0);
    let (close, dismissed) = ui::dialog(ctx, "event-people", &title, |ui| {
        ui.label(RichText::new(when_text(details)).color(ui::colors(ui).muted));
        ScrollArea::vertical().max_height(room).show(ui, |ui| {
            let list = |ui: &mut Ui, label: &str, people: &[Id]| {
                ui::section(ui, &format!("{label} — {}", people.len()));
                if people.is_empty() {
                    ui.weak(t!("events.nobody"));
                }
                for person in people {
                    ui.label(server.member_name(person));
                }
            };
            if !lists.away.is_empty() {
                list(ui, t!("events.not_there"), &lists.away);
            }
            list(ui, t!("events.going"), &lists.going);
            list(ui, t!("events.missing"), &lists.open);
            list(ui, t!("events.not_going"), &lists.not_going);
        });
        let mut close = false;
        ui::footer(ui, |ui| {
            close = ui::secondary(ui, t!("common.close")).clicked();
            if may_remind && details.end > now {
                let ready = next.is_none_or(|next| next <= now) && !lists.open.is_empty();
                let remind = ui
                    .add_enabled_ui(ready, |ui| ui::primary(ui, t!("events.remind")))
                    .inner;
                let remind = match next.filter(|next| *next > now) {
                    Some(next) => remind.on_disabled_hover_text(t!(
                        "events.remind_again_at",
                        time = crate::i18n::time(local(next), t!("time.clock"))
                    )),
                    None => remind,
                };
                if remind.clicked() {
                    change = Some(Change::Remind(id));
                }
            }
        });
        close
    });
    if close || dismissed {
        *open = None;
    }
    change
}

fn confirm_dialog(ctx: &egui::Context, confirm: &mut Option<Confirm>) -> Option<Change> {
    let asked = confirm.as_ref()?;
    let ((yes, no), dismissed) = ui::dialog(ctx, "event-confirm", asked.action, |ui| {
        ui.add(Label::new(asked.question).wrap());
        let mut answer = (false, false);
        ui::footer(ui, |ui| {
            answer.0 = ui::danger(ui, asked.action).clicked();
            answer.1 = ui::secondary(ui, t!("common.back")).clicked();
        });
        answer
    });
    let change = yes.then_some(Change::Delete(asked.id));
    if yes || no || dismissed {
        *confirm = None;
    }
    change
}

/// Where an event happens: an address, or a channel that opens on a click.
fn place_line(ui: &mut Ui, server: &Server, place: &Place, asked: &mut Asked) {
    match place {
        Place::Nowhere => {}
        Place::Irl(address) => {
            let text = RichText::new(format!("📍 {address}")).family(crate::emoji::family(false));
            ui.add(Label::new(text).wrap());
        }
        Place::Channel(id) => match server.channel(id) {
            Some(channel) => {
                let label = format!("{}  {}", super::channel_icon(channel), channel.name);
                let link = ui.link(label).on_hover_text(t!("events.open_channel"));
                if link.clicked() {
                    asked.open = Some(*id);
                }
            }
            None => {
                ui.label(RichText::new(t!("events.channel_hidden")).weak());
            }
        },
    }
}

/// The form's place: none, an address, or one of this server's channels.
fn place_row(ui: &mut Ui, server: &Server, form: &mut Form) {
    ui.add_space(ui::SPACE_S);
    ui.label(ui::bold(t!("events.place")));
    ui::segmented(
        ui,
        &mut form.place,
        &[
            (Where::Nowhere, t!("events.place_none"), ""),
            (Where::Irl, t!("events.place_irl"), ""),
            (Where::Channel, t!("events.place_channel"), ""),
        ],
    );
    ui.add_space(ui::SPACE_XS);
    match form.place {
        Where::Nowhere => {}
        Where::Irl => {
            let field = ui.add(
                ui::field(&mut form.address)
                    .char_limit(MAX_PLACE)
                    .hint_text(t!("events.address_hint"))
                    .desired_width(f32::INFINITY),
            );
            field.widget_info(|| {
                WidgetInfo::labeled(Role::TextInput, true, t!("events.address_hint"))
            });
        }
        Where::Channel => {
            let channels: Vec<_> = (server.dir.categories.iter())
                .flat_map(|category| &category.channels)
                .collect();
            let picked = (form.channel)
                .and_then(|id| channels.iter().find(|channel| channel.id == id))
                .map_or_else(
                    || t!("events.pick_channel").to_owned(),
                    |channel| format!("{}  {}", super::channel_icon(channel), channel.name),
                );
            let combo = egui::ComboBox::from_id_salt("event-channel")
                .selected_text(picked)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for channel in &channels {
                        let label = format!("{}  {}", super::channel_icon(channel), channel.name);
                        ui.selectable_value(&mut form.channel, Some(channel.id), label);
                    }
                });
            combo.response.widget_info(|| {
                WidgetInfo::labeled(Role::ComboBox, true, t!("events.place_channel"))
            });
        }
    }
}

fn form_dialog(
    ctx: &egui::Context,
    server: &Server,
    slot: &mut Option<Form>,
    now: i64,
) -> Option<Change> {
    let form = slot.as_mut()?;
    let create = server.dir.perms.create_events;
    let title = if create {
        t!("events.add")
    } else {
        t!("events.suggest")
    };
    let room = (ctx.content_rect().height() - 180.0).max(160.0);
    let ((send, cancel), dismissed) = ui::dialog(ctx, "event-form", title, |ui| {
        ScrollArea::vertical().max_height(room).show(ui, |ui| {
            let label = ui.label(ui::bold(t!("events.name")));
            ui.add(
                ui::field(&mut form.title)
                    .char_limit(MAX_TITLE)
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label.id);
            ui.add_space(ui::SPACE_S);
            let label = ui.label(ui::bold(t!("events.description")));
            ui.add(
                ui::text_area(&mut form.description)
                    .char_limit(MAX_DESCRIPTION)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label.id);
            let typed = form.description.chars().count();
            ui.label(ui::caption(format!("{typed}/{MAX_DESCRIPTION}")));
            when_row(ui, form, Side::Start, now);
            when_row(ui, form, Side::End, now);
            place_row(ui, server, form);
            if !create {
                ui.add_space(ui::SPACE_S);
                ui.add(Label::new(ui::caption(t!("events.suggest_help"))).wrap());
            }
        });
        ui::error_text(ui, form.error.as_deref());
        let mut answer = (false, false);
        ui::footer(ui, |ui| {
            let label = if create {
                t!("common.create")
            } else {
                t!("events.send_suggestion")
            };
            answer.0 = ui
                .add_enabled_ui(!form.sending, |ui| ui::primary(ui, label))
                .inner
                .clicked();
            answer.1 = ui::secondary(ui, t!("common.cancel")).clicked();
        });
        answer
    });
    if cancel || dismissed {
        *slot = None;
        return None;
    }
    if !send {
        return None;
    }
    match form.details(now) {
        Ok(details) => {
            form.sending = true;
            form.error = None;
            Some(Change::Add(details))
        }
        Err(problem) => {
            form.error = Some(problem.into());
            None
        }
    }
}

fn when_row(ui: &mut Ui, form: &mut Form, side: Side, now: i64) {
    let (label, time_label) = match side {
        Side::Start => (t!("events.start"), t!("events.start_time")),
        Side::End => (t!("events.end"), t!("events.end_time")),
    };
    let when = match side {
        Side::Start => &mut form.start,
        Side::End => &mut form.end,
    };
    ui.add_space(ui::SPACE_S);
    ui.label(ui::bold(label));
    ui.horizontal(|ui| {
        let date = date_text(when.date);
        let button = ui::secondary(ui, &date);
        button.widget_info(|| WidgetInfo::labeled(Role::Button, true, format!("{label}: {date}")));
        if button.clicked() {
            form.picking = match form.picking {
                Some((open, _)) if open == side => None,
                _ => Some((side, first_of_month(when.date))),
            };
        }
        let time = ui.add(
            ui::field(&mut when.time)
                .char_limit(5)
                .hint_text("20:00")
                .desired_width(TIME_WIDTH),
        );
        time.widget_info(|| WidgetInfo::labeled(Role::TextInput, true, time_label));
    });
    let (date, today) = (when.date, local(now).date_naive());
    let Some((open, month)) = &mut form.picking else {
        return;
    };
    if *open != side {
        return;
    }
    month_header(ui, month, true);
    let shown = *month;
    let picked = days(
        ui,
        shown,
        |_| PICKER_DAY_HEIGHT,
        |ui, rect, cell, day| {
            let chosen = day == date;
            cell.widget_info(|| WidgetInfo::selected(Role::Button, true, chosen, date_text(day)));
            let number = Align2::CENTER_CENTER;
            day_base(ui, rect, cell, (day, shown, today), chosen, number);
        },
    );
    if let Some(day) = picked {
        form.pick(side, day);
        form.picking = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn a_month_is_whole_weeks_from_monday() {
        // February 2027 starts on a Monday and has four weeks exactly.
        assert_eq!(month_days(day(2027, 2, 1)).len(), 28);
        let november = month_days(day(2026, 11, 1));
        assert_eq!(november.first(), Some(&day(2026, 10, 26)));
        assert_eq!(november.last(), Some(&day(2026, 12, 6)));
        assert_eq!(next_month(day(2026, 12, 1)), day(2027, 1, 1));
        assert_eq!(previous_month(day(2027, 1, 1)), day(2026, 12, 1));
    }

    #[test]
    fn times_are_read_as_people_type_them() {
        let time = |hour, minute| NaiveTime::from_hms_opt(hour, minute, 0);
        assert_eq!(parse_time("20"), time(20, 0));
        assert_eq!(parse_time(" 20:15 "), time(20, 15));
        assert_eq!(parse_time("2015"), time(20, 15));
        assert_eq!(parse_time("915"), time(9, 15));
        for bad in ["", "24:00", "20:60", "20:5", "+9:15", "12345", "２０"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_event_is_on_every_day_it_touches() {
        let at = |date: NaiveDate, hour| When {
            date,
            time: format!("{hour}:00"),
        };
        let details = Details {
            start: at(day(2026, 11, 24), 22).unix().unwrap(),
            end: at(day(2026, 11, 26), 0).unix().unwrap(),
            ..Details::default()
        };
        let on = |d| happens_on(&details, day(2026, 11, d));
        assert_eq!([on(23), on(24), on(25), on(26)], [false, true, true, false]);
    }

    #[test]
    fn the_form_checks_times_and_moves_the_end_with_the_start() {
        let now = When {
            date: day(2026, 11, 22),
            time: "12:00".into(),
        }
        .unix()
        .unwrap();
        let mut form = Form::new(Some(day(2026, 11, 24)), now);
        assert!(form.details(now).is_err(), "a title is needed");
        form.title = " Quiz ".into();
        let details = form.details(now).unwrap();
        assert_eq!(details.title, "Quiz");
        assert_eq!(details.end - details.start, LASTS);

        form.pick(Side::Start, day(2026, 11, 28));
        assert_eq!(form.end.date, day(2026, 11, 28));
        form.end.time = "19".into();
        assert!(form.details(now).is_err(), "it ends before it starts");
    }

    #[test]
    fn a_chosen_place_needs_its_address_or_channel() {
        let now = 1_800_000_000;
        let mut form = Form::new(None, now);
        form.title = "Quiz".into();
        assert_eq!(form.details(now).unwrap().place, Place::Nowhere);
        form.place = Where::Irl;
        form.address = "  ".into();
        assert!(form.details(now).is_err());
        form.address = " Bahnhofstr. 1 ".into();
        assert_eq!(
            form.details(now).unwrap().place,
            Place::Irl("Bahnhofstr. 1".into())
        );
        form.place = Where::Channel;
        assert!(form.details(now).is_err());
        form.channel = Some([3; 32]);
        assert_eq!(form.details(now).unwrap().place, Place::Channel([3; 32]));
    }

    #[test]
    fn a_failed_change_keeps_what_was_typed() {
        let mut view = View {
            form: Some(Form::new(None, 1_800_000_000)),
            ..View::default()
        };
        view.form.as_mut().unwrap().sending = true;
        view.changed(Err("no".into()));
        let form = view.form.as_ref().unwrap();
        assert!(!form.sending && form.error.is_some());
        view.form.as_mut().unwrap().sending = true;
        view.changed(Ok(()));
        assert!(view.form.is_none());
    }

    #[test]
    fn lists_who_is_missing_and_reminds_only_those_who_havent_answered() {
        use crate::server::{Category, Channel, Member, Peer};
        const ME: Id = [1; 32];
        const ANN: Id = [2; 32];
        const BOB: Id = [3; 32];
        const CID: Id = [4; 32];
        const LOUNGE: Id = [9; 32];
        let (commands, _) = tokio::sync::mpsc::unbounded_channel();
        let known = rusttalk_core::auth::KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [5; 32],
            tls_fingerprint: None,
        };
        let mut server = crate::home::new_server(known, "Friends", commands);
        server.dir.me = ME;
        let member = |person| Member {
            person,
            name: None,
            devices: Vec::new(),
            roles: vec!["member".into()],
            avatar: None,
            presence: Default::default(),
            hide_offline: false,
            bio: None,
            voice_muted: false,
        };
        server.dir.members = vec![member(ME), member(ANN), member(BOB), member(CID)];
        server.dir.categories = vec![Category {
            id: None,
            name: String::new(),
            channels: vec![Channel {
                id: LOUNGE,
                name: "Lounge".into(),
                topic: String::new(),
                voice: true,
                page: false,
                audio_kbps: 64,
                flags: Vec::new(),
            }],
        }];
        let peer = |person| Peer {
            person,
            connecting: false,
            unknown: false,
            status: Default::default(),
            apps: Vec::new(),
            screens: Vec::new(),
            camera: None,
            terminal: None,
        };
        server.voice.insert(LOUNGE, vec![peer(ANN)]);

        let now = 1_800_000_000;
        let event = Event {
            details: Some(Details {
                title: "Quiz".into(),
                start: now - 600,
                end: now + 3600,
                place: Place::Channel(LOUNGE),
                ..Details::default()
            }),
            record: rusttalk_core::calendar::EventRecord {
                id: [7; 32],
                author: ANN,
                author_device: [0; 32],
                certificate: String::new(),
                approved: true,
                epoch: 1,
                remove_at: now + 3600 + rusttalk_core::calendar::GRACE,
                envelope: String::new(),
                answers: Vec::new(),
                reminded_at: Some(now - 60),
            },
            answers: vec![
                (ANN, Answer::Going),
                (BOB, Answer::Going),
                (CID, Answer::NotGoing),
            ],
        };
        let lists = people(&server, &event, now);
        assert_eq!(lists.going, [ANN, BOB]);
        assert_eq!(lists.not_going, [CID]);
        assert_eq!(lists.open, [ME]);
        assert_eq!(lists.away, [BOB], "Bob said yes but isn't in the lounge");
        // Before it starts, nobody is missing from the channel yet.
        assert!(people(&server, &event, now - 3600).away.is_empty());

        server.dir.calendar = Some(Calendar {
            enabled: true,
            events: vec![event],
        });
        assert_eq!(reminders(&server, now).len(), 1, "I haven't answered");
        let events = &mut server.dir.calendar.as_mut().unwrap().events;
        events[0].record.author = ME;
        assert_eq!(reminders(&server, now).len(), 1, "unanswered author");
        assert!(
            reminders(&server, now + 3600).is_empty(),
            "reconnected at event end"
        );
        // A manager can use the dialog to remind the unanswered author.
        server.dir.me = ANN;
        server.dir.perms.manage_events = true;
        let id = server.dir.calendar.as_ref().unwrap().events[0].record.id;
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            use egui_kittest::kittest::Queryable;
            let mut open = Some(id);
            let mut harness = egui_kittest::Harness::builder()
                .with_context_setup(move |ctx| {
                    ui::install(ctx);
                    ctx.set_theme(theme);
                })
                .build_ui_state(
                    |ui, sent: &mut bool| {
                        let change =
                            people_dialog(ui.ctx(), &server, &mut open, now + REMIND_EVERY - 60);
                        *sent |= matches!(change, Some(Change::Remind(target)) if target == id);
                    },
                    false,
                );
            harness.run_steps(4);
            harness.get_by_label(t!("events.remind")).click();
            harness.run_steps(2);
            assert!(*harness.state());
        }
        server.dir.me = ME;
        server.dir.calendar.as_mut().unwrap().events[0]
            .answers
            .push((ME, Answer::NotGoing));
        assert!(
            reminders(&server, now).is_empty(),
            "answered, so no reminder"
        );
    }
}
