//! The Members page of server settings: search by name, filter by role and a table
//! you can sort. Names are decrypted here, so the search runs on this device.

use std::cmp::Reverse;

use eframe::egui::{
    self, Align, FontId, Label, Layout, Rect, Sense, Shape, Stroke, Ui, UiBuilder, Vec2,
    accesskit::Role as Access, pos2, vec2,
};

use crate::{
    home::{
        person::{Person, Pick, interact, status_text},
        presence_color,
    },
    server::{Directory, Id, Member, Role},
    ui::{self, Item, Look},
};

const ROW: f32 = 52.0;
const HEADER: f32 = 28.0;
const FACE: f32 = 32.0;
const STATUS_WIDTH: f32 = 96.0;
const ROLE_FILTER_WIDTH: f32 = 180.0;

#[derive(Clone, Copy, Default, PartialEq, Debug)]
enum Sort {
    #[default]
    Name,
    /// Highest role first.
    Role,
}

/// What the table shows: the search, the role filter and the sort order.
#[derive(Default)]
pub struct View {
    query: String,
    /// A role id.
    role: Option<String>,
    sort: Sort,
    descending: bool,
}

impl View {
    fn sort_by(&mut self, sort: Sort) {
        if self.sort == sort {
            self.descending = !self.descending;
        } else {
            *self = Self {
                sort,
                query: std::mem::take(&mut self.query),
                role: self.role.take(),
                descending: false,
            };
        }
    }
}

fn top_rank(dir: &Directory, member: &Member) -> i64 {
    (dir.roles.iter())
        .filter(|role| member.roles.contains(&role.id))
        .map(|role| role.rank)
        .max()
        .unwrap_or(i64::MIN)
}

/// Everyone matching the search and role filter, in the chosen order. Members who
/// hide while offline are included: admins manage everyone.
fn visible<'a>(dir: &'a Directory, view: &View) -> Vec<&'a Member> {
    let query = view.query.trim().to_lowercase();
    let mut list: Vec<&Member> = (dir.members.iter())
        .filter(|m| view.role.as_ref().is_none_or(|role| m.roles.contains(role)))
        .filter(|m| query.is_empty() || m.display_name().to_lowercase().contains(&query))
        .collect();
    match view.sort {
        Sort::Name => list.sort_by_cached_key(|m| m.display_name().to_lowercase()),
        Sort::Role => list
            .sort_by_cached_key(|m| (Reverse(top_rank(dir, m)), m.display_name().to_lowercase())),
    }
    if view.descending {
        list.reverse();
    }
    list
}

/// The table's columns inside `row`: member, roles, status and the actions button.
fn columns(row: Rect) -> [Rect; 4] {
    let inner = row.shrink2(vec2(ui::SPACE_S, 0.0));
    let actions = Rect::from_min_max(
        pos2(inner.right() - ui::ICON_BUTTON, inner.top()),
        inner.max,
    );
    let status = Rect::from_min_max(
        pos2(actions.left() - ui::SPACE_M - STATUS_WIDTH, inner.top()),
        pos2(actions.left() - ui::SPACE_M, inner.bottom()),
    );
    let rest = status.left() - ui::SPACE_M - inner.left();
    let roles_width = (rest * 0.4).max(0.0);
    let roles = Rect::from_min_max(
        pos2(status.left() - ui::SPACE_M - roles_width, inner.top()),
        pos2(status.left() - ui::SPACE_M, inner.bottom()),
    );
    let member = Rect::from_min_max(inner.min, pos2(roles.left() - ui::SPACE_M, inner.bottom()));
    [member, roles, status, actions]
}

/// A child of `ui` filling `rect`, clipped to it, laid out left to right.
fn cell(ui: &mut Ui, rect: Rect) -> Ui {
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child
}

/// The search, the role filter, the count and the table. Returns what was picked
/// from someone's menu.
pub fn show<'a>(
    ui: &mut Ui,
    dir: &Directory,
    view: &mut View,
    target: impl Fn(Id) -> Person<'a>,
) -> Option<(Id, Pick)> {
    // A role deleted meanwhile no longer filters.
    if (view.role.as_ref()).is_some_and(|id| !dir.roles.iter().any(|role| role.id == *id)) {
        view.role = None;
    }
    ui::right_row(ui, ui::CONTROL_HEIGHT, |ui| {
        role_filter(ui, dir, view);
        let search = ui.add(
            ui::field(&mut view.query)
                .hint_text(t!("admin.members_search"))
                .desired_width(ui.available_width()),
        );
        search.widget_info(|| {
            egui::WidgetInfo::labeled(Access::SearchInput, true, t!("admin.members_search"))
        });
    });
    let list = visible(dir, view);
    let total = dir.members.len();
    ui.add_space(ui::SPACE_S);
    let count = if list.len() == total {
        t!("admin.members_total", count = total)
    } else {
        t!("admin.members_shown", count = list.len(), total = total)
    };
    ui.label(ui::caption(count));
    ui.add_space(ui::SPACE_S);
    header(ui, view);
    if list.is_empty() {
        ui.add_space(ui::SPACE_M);
        ui.weak(t!("admin.members_none"));
        return None;
    }
    // Only the rows in view are drawn, so thousands of members stay smooth.
    let top = ui.cursor().top();
    let clip = ui.clip_rect();
    let first = (((clip.top() - top) / ROW).floor().max(0.0) as usize).min(list.len());
    let last = ((((clip.bottom() - top) / ROW).ceil().max(0.0) as usize) + 1).min(list.len());
    let mut pick = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.add_space(first as f32 * ROW);
    for member in &list[first..last] {
        if let Some(picked) = row(ui, dir, member, &target(member.person)) {
            pick = Some((member.person, picked));
        }
    }
    ui.add_space((list.len() - last) as f32 * ROW);
    pick
}

fn role_filter(ui: &mut Ui, dir: &Directory, view: &mut View) {
    let c = ui::colors(ui);
    let shown = (view.role.as_ref())
        .and_then(|id| dir.roles.iter().find(|role| role.id == *id))
        .map_or(t!("admin.all_roles"), |role| role.name.as_str());
    let button = ui.add_sized(
        vec2(ROLE_FILTER_WIDTH, ui::CONTROL_HEIGHT),
        egui::Button::new(shown).truncate(),
    );
    button.widget_info(|| egui::WidgetInfo::labeled(Access::ComboBox, true, t!("admin.roles")));
    ui::menu_below(&button, |m| {
        if m.add(Item::new("", t!("admin.all_roles")).picked(view.role.is_none()))
            .clicked()
        {
            view.role = None;
        }
        for role in &dir.roles {
            let item = Item::dot(role.color.unwrap_or(c.muted), &role.name)
                .picked(view.role.as_ref() == Some(&role.id));
            if m.add(item).clicked() {
                view.role = Some(role.id.clone());
            }
        }
    });
}

/// Column titles; Member and Roles sort when clicked.
fn header(ui: &mut Ui, view: &mut View) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER), Sense::hover());
    let [member, roles, status, _] = columns(rect);
    let sortable = |ui: &mut Ui, view: &mut View, area: Rect, sort, label: &str| {
        let response = ui.interact(area, ui.scope_id().with(("sort", label)), Sense::click());
        let on = view.sort == sort;
        let c = ui::colors(ui);
        let color = if on || response.hovered() {
            c.strong
        } else {
            c.muted
        };
        let font = FontId::new(ui::TEXT_CAPTION, ui::bold_family());
        let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
        let width = galley.size().x;
        let at = pos2(area.left(), area.center().y - galley.size().y / 2.0);
        ui.painter().galley(at, galley, color);
        if on {
            // Up when A to Z or highest role first, down when reversed.
            let x = area.left() + width + ui::SPACE_S;
            let y = area.center().y;
            let tip = if view.descending { 3.0 } else { -3.0 };
            ui.painter().add(Shape::convex_polygon(
                vec![
                    pos2(x - 4.0, y - tip),
                    pos2(x + 4.0, y - tip),
                    pos2(x, y + tip),
                ],
                color,
                Stroke::NONE,
            ));
        }
        response.widget_info(|| {
            egui::WidgetInfo::labeled(Access::Button, true, t!("admin.sort_by", column = label))
        });
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            view.sort_by(sort);
        }
    };
    sortable(ui, view, member, Sort::Name, t!("admin.col_member"));
    sortable(ui, view, roles, Sort::Role, t!("admin.roles"));
    let c = ui::colors(ui);
    let font = FontId::new(ui::TEXT_CAPTION, ui::bold_family());
    ui.painter().text(
        pos2(status.left(), status.center().y),
        egui::Align2::LEFT_CENTER,
        t!("admin.col_status"),
        font,
        c.muted,
    );
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, c.border));
}

fn row(ui: &mut Ui, dir: &Directory, member: &Member, person: &Person) -> Option<Pick> {
    let name = member.display_name();
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::hover());
    // Before the contents, so the actions button on top of it takes its own clicks.
    let row = ui.interact(
        rect,
        ui.scope_id().with(("member", member.person)),
        Sense::click(),
    );
    row.widget_info(|| egui::WidgetInfo::labeled(Access::Button, true, &name));
    let hover =
        ui.ctx()
            .animate_bool_with_time(row.id.with("hover"), row.hovered(), ui::HOVER_SECONDS);
    let c = ui::colors(ui);
    let fill = egui::Color32::TRANSPARENT.lerp_to_gamma(c.hover, hover);
    ui.painter()
        .rect_filled(rect.shrink2(vec2(0.0, 1.0)), ui::RADIUS_ROW, fill);
    let [member_area, roles_area, status_area, actions_area] = columns(rect);

    let child = &mut cell(ui, member_area);
    child.spacing_mut().item_spacing.x = ui::SPACE_M;
    crate::home::face(child, dir, &member.person, &name, FACE, false);
    let mut text = ui::bold(&name);
    if let Some(color) = dir.name_color(&member.roles) {
        text = text.color(color);
    }
    child.add(Label::new(text).truncate().selectable(false));

    let child = &mut cell(ui, roles_area);
    child.spacing_mut().item_spacing.x = ui::SPACE_XS;
    role_chips(child, dir, member);

    let child = &mut cell(ui, status_area);
    child.spacing_mut().item_spacing.x = ui::SPACE_S;
    ui::dot(child, presence_color(c, member.presence));
    child
        .add(Label::new(ui::caption(status_text(member.presence)).color(c.text)).selectable(false));

    let child = &mut cell(ui, actions_area);
    let more = ui::icon_button(
        child,
        "…",
        &t!("admin.member_actions", name = name),
        Look::Ghost,
        Vec2::splat(ui::ICON_BUTTON),
    );
    let menu = ui::menu(&more, |m| person.menu(m)).flatten();
    interact(ui, &row, person, false).or(menu)
}

fn role_chips(ui: &mut Ui, dir: &Directory, member: &Member) {
    let roles: Vec<&Role> = (dir.roles.iter())
        .filter(|role| member.roles.contains(&role.id))
        .collect();
    if roles.is_empty() {
        ui.add(Label::new(ui::caption(t!("admin.no_role"))).selectable(false));
        return;
    }
    let c = ui::colors(ui);
    for (at, role) in roles.iter().enumerate() {
        // Keep room for a chip; the rest are counted.
        if at > 0 && ui.available_width() < 72.0 {
            let more = roles.len() - at;
            ui.add(Label::new(ui::caption(format!("+{more}"))).selectable(false));
            break;
        }
        // Painted by hand: a framed row would stretch to the height of the table row.
        let font = FontId::new(ui::TEXT_CAPTION, egui::FontFamily::Proportional);
        let galley = ui.painter().layout_no_wrap(role.name.clone(), font, c.text);
        let pad = vec2(6.0, 3.0);
        let dot = 10.0;
        let size = vec2(
            pad.x * 2.0 + dot + ui::SPACE_XS + galley.size().x,
            galley.size().y + pad.y * 2.0,
        );
        let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, ui::RADIUS_S, c.field);
        let dot_center = pos2(rect.left() + pad.x + dot / 2.0, rect.center().y);
        painter.circle_filled(dot_center, 5.0, role.color.unwrap_or(c.muted));
        let text = pos2(dot_center.x + dot / 2.0 + ui::SPACE_XS, rect.center().y);
        painter.galley(text - vec2(0.0, galley.size().y / 2.0), galley, c.text);
        response.widget_info(|| egui::WidgetInfo::labeled(Access::Label, true, &role.name));
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};
    use rusttalk_core::presence::Presence;

    use super::*;
    use crate::home::Home;

    fn role(id: &str, rank: i64) -> Role {
        Role {
            id: id.into(),
            name: id.into(),
            name_known: true,
            rank,
            permissions: Vec::new(),
            custom: false,
            color: None,
            separate: false,
        }
    }

    fn member(byte: u8, name: &str, roles: &[&str]) -> Member {
        Member {
            person: [byte; 32],
            name: Some(name.into()),
            devices: Vec::new(),
            roles: roles.iter().map(|id| id.to_string()).collect(),
            avatar: None,
            presence: Presence::Offline,
            hide_offline: false,
            bio: None,
            voice_muted: false,
        }
    }

    fn directory() -> Directory {
        Directory {
            roles: vec![role("owner", 1000), role("mod", 50), role("member", 0)],
            members: vec![
                member(1, "carla", &["member"]),
                member(2, "Bob", &["mod", "member"]),
                member(3, "alice", &["owner", "member"]),
                member(4, "Dan", &[]),
            ],
            ..Directory::default()
        }
    }

    fn names(list: Vec<&Member>) -> Vec<String> {
        list.iter().map(|m| m.display_name()).collect()
    }

    #[test]
    fn search_ignores_case_and_matches_part_of_a_name() {
        let dir = directory();
        let view = |query: &str| View {
            query: query.into(),
            ..View::default()
        };
        assert_eq!(names(visible(&dir, &view("AL"))), ["alice"]);
        assert_eq!(names(visible(&dir, &view(" ar "))), ["carla"]);
        assert!(visible(&dir, &view("zzz")).is_empty());
        assert_eq!(visible(&dir, &view("")).len(), 4);
    }

    #[test]
    fn the_role_filter_keeps_people_with_that_role_and_combines_with_search() {
        let dir = directory();
        let mut view = View {
            role: Some("member".into()),
            ..View::default()
        };
        assert_eq!(names(visible(&dir, &view)), ["alice", "Bob", "carla"]);
        view.query = "b".into();
        assert_eq!(names(visible(&dir, &view)), ["Bob"]);
        view.role = Some("owner".into());
        assert!(visible(&dir, &view).is_empty());
    }

    #[test]
    fn names_sort_without_regard_to_case_and_can_be_reversed() {
        let dir = directory();
        let mut view = View::default();
        assert_eq!(
            names(visible(&dir, &view)),
            ["alice", "Bob", "carla", "Dan"]
        );
        view.sort_by(Sort::Name);
        assert_eq!(
            names(visible(&dir, &view)),
            ["Dan", "carla", "Bob", "alice"]
        );
    }

    #[test]
    fn roles_sort_by_highest_role_then_name_and_keep_the_filter_when_switching() {
        let dir = directory();
        let mut view = View {
            query: "a".into(),
            ..View::default()
        };
        view.sort_by(Sort::Role);
        // The search survives a new sort; the direction starts over.
        assert_eq!(view.query, "a");
        assert!(!view.descending);
        assert_eq!(names(visible(&dir, &view)), ["alice", "carla", "Dan"]);
        view.query.clear();
        assert_eq!(
            names(visible(&dir, &view)),
            ["alice", "Bob", "carla", "Dan"]
        );
        view.sort_by(Sort::Role);
        assert_eq!(
            names(visible(&dir, &view)),
            ["Dan", "carla", "Bob", "alice"]
        );
    }

    #[test]
    fn people_hiding_while_offline_are_still_listed() {
        let mut dir = directory();
        dir.members[0].hide_offline = true;
        assert_eq!(visible(&dir, &View::default()).len(), 4);
    }

    fn placeholder_home() -> Home {
        let ctx = egui::Context::default();
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut home = Home::new(identity, &ctx, data.path().to_owned()).unwrap();
        let known = rusttalk_core::auth::KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [2; 32],
            tls_fingerprint: None,
        };
        let (commands, _) = tokio::sync::mpsc::unbounded_channel();
        home.servers.push(crate::server::Server {
            address: known.address.clone(),
            known,
            label: "Friends".into(),
            status: crate::server::Status::Online,
            compatibility: Default::default(),
            order_unavailable: false,
            dir: Directory::default(),
            chats: Default::default(),
            open: None,
            events_open: false,
            events: Default::default(),
            voice: Default::default(),
            names: Default::default(),
            commands,
        });
        home
    }

    /// The table on a server where you hold `perms`, with `extra` more people.
    fn table(perms: crate::server::Perms, extra: usize) -> Harness<'static, (Home, View)> {
        let mut harness = Harness::builder()
            .with_size(Vec2::new(700.0, 500.0))
            .build_ui_state(
                |ui, (home, view): &mut (Home, View)| {
                    // Fonts come after the first frame, which has no members yet.
                    if home.servers[0].dir.members.is_empty() {
                        return;
                    }
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let server = &home.servers[0];
                        let target =
                            |person| Person::with(&home.friends, &home.voice, server, person);
                        show(ui, &server.dir, view, target);
                    });
                },
                (placeholder_home(), View::default()),
            );
        ui::install(&harness.ctx);
        let mut dir = directory();
        dir.perms = perms;
        dir.me = [1; 32];
        for at in 0..extra {
            let mut crowd = member(10, &format!("Person {at}"), &["member"]);
            crowd.person[1] = (at / 200) as u8;
            crowd.person[2] = (at % 200) as u8;
            dir.members.push(crowd);
        }
        harness.state_mut().0.servers[0].dir = dir;
        harness.run_steps(3);
        harness
    }

    #[test]
    fn a_thousand_members_draw_only_the_rows_in_view() {
        let harness = table(crate::server::Perms::default(), 1000);
        let drawn = harness.query_all_by_label_contains("Actions for").count();
        assert!(drawn > 3 && drawn < 20, "{drawn}");
    }

    #[test]
    fn the_actions_button_opens_the_shared_person_menu_with_rank_gated_items() {
        let perms = crate::server::Perms {
            kick: true,
            ban: true,
            manage_roles: true,
            rank: 900,
            ..Default::default()
        };
        let mut harness = table(perms, 0);
        harness.get_by_label("Actions for Bob").click();
        harness.run_steps(4);
        for label in [
            t!("home.message"),
            t!("admin.change_roles"),
            t!("admin.kick"),
        ] {
            assert!(harness.query_by_label(label).is_some(), "{label}");
        }
        // The owner outranks you: no moderation in their menu.
        harness.key_press(egui::Key::Escape);
        harness.run_steps(4);
        harness.get_by_label("Actions for alice").click();
        harness.run_steps(4);
        assert!(harness.query_by_label(t!("home.message")).is_some());
        assert!(harness.query_by_label(t!("admin.kick")).is_none());
    }

    #[test]
    fn searching_narrows_the_table_and_the_count() {
        let mut harness = table(crate::server::Perms::default(), 0);
        harness.state_mut().1.query = "ali".into();
        harness.run_steps(3);
        assert!(harness.query_all_by_label("alice").count() > 0);
        assert_eq!(harness.query_all_by_label("Bob").count(), 0);
        assert!(
            harness
                .query_by_label(&t!("admin.members_shown", count = 1, total = 4))
                .is_some()
        );
    }
}
