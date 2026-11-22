//! The Roles page of server settings: the roles in rank order on the left, to pick, drag
//! into a new order or add to; the picked role's display, permissions and members on the
//! right. Changes wait for Save in the unsaved-changes bar, like Discord's.

use eframe::egui::{
    self, Color32, ComboBox, CursorIcon, Label, Layout, Sense, Stroke, Ui, Vec2, WidgetInfo,
    accesskit::Role as A11y, color_picker,
};

use crate::{
    home::face,
    net::Admin,
    server::{Directory, Id, Member, Role, Server},
    ui,
};

const LIST_WIDTH: f32 = 200.0;
const LIST_ROW: f32 = 34.0;
const MEMBER_ROW: f32 = 40.0;
/// The owner's rank; nothing else reaches it.
const OWNER_RANK: i64 = 1000;
/// Where new roles go: above Member (0), below Moderator (50), when yours is higher.
const NEW_ROLE_RANK: i64 = 25;
/// People offered at once when adding members.
const MATCHES: usize = 8;
/// How long the unsaved-changes bar stays red after leaving was refused.
const NUDGE_SECONDS: f64 = 0.8;

#[derive(Clone, Copy, PartialEq, Default)]
pub enum RoleTab {
    #[default]
    Display,
    Permissions,
    Members,
}

/// The Roles page between frames. Only changes not saved yet are kept here, so what the
/// server sends shows through everywhere else.
#[derive(Default)]
pub struct RolesPage {
    selected: Option<String>,
    tab: RoleTab,
    /// Every role ID, top first, after a drag; `None` until moved.
    order: Option<Vec<String>>,
    name: Option<String>,
    color: Option<Option<Color32>>,
    separate: Option<bool>,
    permissions: Option<Vec<String>>,
    /// People given (`true`) or losing (`false`) the open role.
    members: Vec<(Id, bool)>,
    search: String,
    /// Save was clicked; the next result ends it, keeping the changes if it failed.
    saving: bool,
    /// The role IDs there were when Create role was clicked, to open the new one.
    creating: Option<Vec<String>>,
    /// The delete confirmation is open.
    deleting: bool,
    /// When leaving with unsaved changes was refused, to turn the bar red.
    nudged: f64,
    /// The role being dragged in the list.
    dragging: Option<String>,
}

impl RolesPage {
    /// A change went through: what was saved now comes from the server.
    pub fn done(&mut self) {
        if self.saving {
            self.reset();
        }
        self.saving = false;
        self.deleting = false;
    }

    pub fn failed(&mut self) {
        self.saving = false;
        self.creating = None;
    }

    fn reset(&mut self) {
        self.order = None;
        self.name = None;
        self.color = None;
        self.separate = None;
        self.permissions = None;
        self.members.clear();
    }

    fn dirty(&self, dir: &Directory) -> bool {
        let Some(role) = self.role(dir) else {
            return self.order.is_some();
        };
        self.order.is_some()
            || self
                .name
                .as_ref()
                .is_some_and(|name| name.trim() != role.name)
            || self.color.is_some_and(|color| color != role.color)
            || self.separate.is_some_and(|on| on != role.separate)
            || self
                .permissions
                .as_ref()
                .is_some_and(|chosen| !same(chosen, &role.permissions))
            || !self.members.is_empty()
    }

    fn role<'a>(&self, dir: &'a Directory) -> Option<&'a Role> {
        let id = self.selected.as_ref()?;
        dir.roles.iter().find(|role| role.id == *id)
    }

    /// Ask to leave the open role; refused (and the bar flashes) while changes wait.
    fn may_leave(&mut self, ui: &Ui, dir: &Directory) -> bool {
        if self.dirty(dir) {
            self.nudged = ui.input(|input| input.time);
            return false;
        }
        true
    }
}

/// The same permissions, in any order.
fn same(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|p| b.contains(p))
}

/// Custom roles below yours can move; the rest keep their place.
fn movable(role: &Role, mine: i64) -> bool {
    role.custom && role.rank < mine
}

/// Ranks for roles listed top first, after a drag: the ones that moved get ranks between
/// the fixed roles around them, below `mine`. `None` when they don't fit there.
fn ranks(order: &[&Role], mine: i64) -> Option<Vec<i64>> {
    let mut out: Vec<i64> = order.iter().map(|role| role.rank).collect();
    let mut start = 0;
    while start < order.len() {
        if !movable(order[start], mine) {
            start += 1;
            continue;
        }
        let end = (start..order.len())
            .find(|&at| !movable(order[at], mine))
            .unwrap_or(order.len());
        let upper = start.checked_sub(1).map_or(OWNER_RANK, |at| order[at].rank);
        let lower = order.get(end).map_or(0, |role| role.rank);
        let high = (upper - 1).min(mine - 1);
        let low = (lower + 1).max(1);
        let run = &order[start..end];
        let count = run.len() as i64;
        if high - low + 1 < count {
            return None;
        }
        // Keep ranks that already fit, so a small move changes few roles.
        let fits = run.windows(2).all(|pair| pair[0].rank > pair[1].rank)
            && run[0].rank <= high
            && run[run.len() - 1].rank >= low;
        if !fits {
            let gap = (high - low + 2) as f64 / (count + 1) as f64;
            for at in 0..run.len() {
                out[start + at] = (high as f64 + 1.0 - gap * (at + 1) as f64).round() as i64;
            }
        }
        start = end;
    }
    Some(out)
}

/// The roles top first, as moved but not saved yet.
fn ordered<'a>(dir: &'a Directory, order: Option<&Vec<String>>) -> Vec<&'a Role> {
    let mut roles: Vec<&Role> = dir.roles.iter().collect();
    if let Some(order) = order {
        roles.sort_by_key(|role| order.iter().position(|id| *id == role.id));
    }
    roles
}

/// The highest rank among someone's roles; they can be changed only by people above it.
fn member_rank(dir: &Directory, member: &Member) -> i64 {
    (dir.roles.iter())
        .filter(|role| member.roles.contains(&role.id))
        .map(|role| role.rank)
        .max()
        .unwrap_or(-1)
}

/// A permission's API name, label, line of help and whether it needs care.
type Permission = (&'static str, &'static str, &'static str, bool);

/// The permissions in groups, with the group titles.
fn groups(events: bool) -> Vec<(&'static str, Vec<Permission>)> {
    let mut groups = vec![
        (
            t!("admin.group_general"),
            vec![
                ("invite", t!("perm.invite"), t!("perm.invite_help"), false),
                (
                    crate::server::MENTION_EVERYONE,
                    t!("perm.mention_everyone"),
                    t!("perm.mention_everyone_help"),
                    false,
                ),
                (
                    "manage_emoji",
                    t!("perm.manage_emoji"),
                    t!("perm.manage_emoji_help"),
                    false,
                ),
            ],
        ),
        (
            t!("admin.group_members"),
            vec![
                ("kick", t!("perm.kick"), t!("perm.kick_help"), false),
                (
                    "manage_roles",
                    t!("perm.manage_roles"),
                    t!("perm.manage_roles_help"),
                    true,
                ),
            ],
        ),
        (
            t!("admin.group_moderation"),
            vec![
                ("ban", t!("perm.ban"), t!("perm.ban_help"), true),
                (
                    "delete_messages",
                    t!("perm.delete_messages"),
                    t!("perm.delete_messages_help"),
                    false,
                ),
                (
                    "view_audit_log",
                    t!("perm.view_audit_log"),
                    t!("perm.view_audit_log_help"),
                    false,
                ),
                (
                    "mute_members",
                    t!("perm.mute_members"),
                    t!("perm.mute_members_help"),
                    false,
                ),
                (
                    "move_members",
                    t!("perm.move_members"),
                    t!("perm.move_members_help"),
                    false,
                ),
            ],
        ),
        (
            t!("admin.group_advanced"),
            vec![
                (
                    "manage_channels",
                    t!("perm.manage_channels"),
                    t!("perm.manage_channels_help"),
                    false,
                ),
                (
                    "manage_server",
                    t!("perm.manage_server"),
                    t!("perm.manage_server_help"),
                    true,
                ),
            ],
        ),
    ];
    // Older servers refuse permissions they don't know.
    if events {
        groups.push((
            t!("events.title"),
            vec![
                (
                    "suggest_events",
                    t!("perm.suggest_events"),
                    t!("perm.suggest_events_help"),
                    false,
                ),
                (
                    "create_events",
                    t!("perm.create_events"),
                    t!("perm.create_events_help"),
                    false,
                ),
                (
                    "manage_events",
                    t!("perm.manage_events"),
                    t!("perm.manage_events_help"),
                    false,
                ),
            ],
        ));
    }
    groups
}

/// Whether you hold a permission yourself; only those can be given or taken away.
fn holds(dir: &Directory, permission: &str) -> bool {
    (dir.roles.iter())
        .filter(|role| {
            (dir.members.iter()).any(|m| m.person == dir.me && m.roles.contains(&role.id))
        })
        .any(|role| role.permissions.iter().any(|p| p == permission))
}

/// The Roles page. Returns the requests to send, in order.
pub fn tab(
    ui: &mut Ui,
    server: &Server,
    page: &mut RolesPage,
    message: &mut Option<(String, bool)>,
) -> Vec<Admin> {
    let dir = &server.dir;
    let mut requests = Vec::new();
    default_role(ui, dir, &mut requests);
    ui::page_section(ui, t!("admin.roles"));

    // Open a role Create role just made, or keep one open.
    if let Some(before) = &page.creating
        && let Some(made) =
            (dir.roles.iter()).find(|role| role.custom && !before.contains(&role.id))
    {
        page.selected = Some(made.id.clone());
        page.tab = RoleTab::Display;
        page.creating = None;
        page.reset();
    }
    if page.role(dir).is_none() {
        page.reset();
        page.selected = (dir.roles.iter())
            .find(|role| role.rank < dir.perms.rank)
            .or(dir.roles.first())
            .map(|role| role.id.clone());
    }
    if (page.order.as_ref()).is_some_and(|order| {
        order.len() != dir.roles.len() || dir.roles.iter().any(|role| !order.contains(&role.id))
    }) {
        // Roles came or went meanwhile; the drag no longer applies.
        page.order = None;
    }

    let top = ui.cursor().top();
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(LIST_WIDTH, 0.0),
            Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(LIST_WIDTH);
                role_list(ui, dir, page, message, &mut requests);
            },
        );
        ui.add_space(ui::SPACE_M);
        let x = ui.cursor().left();
        ui.add_space(ui::SPACE_L);
        let editor = ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), 0.0),
            Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(ui.available_width());
                editor(ui, server, page);
            },
        );
        let bottom = editor.response.rect.bottom().max(ui.min_rect().bottom());
        ui.painter()
            .vline(x, top..=bottom, Stroke::new(1.0, ui::colors(ui).border));
    });

    if page.dirty(dir) {
        ui.add_space(ui::UNSAVED_BAR + ui::SPACE_S);
        let now = ui.input(|input| input.time);
        let nudge = (1.0 - (now - page.nudged) / NUDGE_SECONDS).clamp(0.0, 1.0) as f32;
        if nudge > 0.0 {
            ui.ctx().request_repaint();
        }
        let name_ok = page
            .name
            .as_ref()
            .is_none_or(|name| !name.trim().is_empty());
        let (reset, save) = ui::unsaved_bar(ui, nudge, name_ok && !page.saving);
        if reset {
            page.reset();
        }
        if save {
            requests.extend(save_requests(dir, page));
            page.saving = true;
            *message = None;
        }
    }
    requests
}

/// "New members get", at the top.
fn default_role(ui: &mut Ui, dir: &Directory, requests: &mut Vec<Admin>) {
    let default = (dir.roles.iter())
        .find(|role| role.id == dir.default_role)
        .map_or("", |role| role.name.as_str());
    ui::page_section(ui, t!("admin.new_members"));
    ui::setting_row(
        ui,
        t!("admin.default_role"),
        t!("admin.default_role_help"),
        |ui| {
            let combo = ComboBox::from_id_salt("default role")
                .selected_text(default)
                .show_ui(ui, |ui| {
                    for role in dir.roles.iter().filter(|r| r.id != "owner") {
                        if ui
                            .selectable_label(role.id == dir.default_role, &role.name)
                            .clicked()
                            && role.id != dir.default_role
                        {
                            requests.push(Admin::DefaultRole(role.id.clone()));
                        }
                    }
                });
            combo.response.widget_info(|| {
                WidgetInfo::labeled(A11y::ComboBox, true, t!("admin.default_role"))
            });
        },
    );
}

/// Create role, then the roles top first with their colour and member count. Custom roles
/// below yours can be dragged to a new place.
fn role_list(
    ui: &mut Ui,
    dir: &Directory,
    page: &mut RolesPage,
    message: &mut Option<(String, bool)>,
    requests: &mut Vec<Admin>,
) {
    let mine = dir.perms.rank;
    let can_create = mine > 1 && page.creating.is_none();
    let create = ui
        .add_enabled_ui(can_create, |ui| {
            ui.add_sized(
                Vec2::new(ui.available_width(), ui::CONTROL_HEIGHT),
                egui::Button::new(t!("admin.create_role")),
            )
        })
        .inner;
    if create.clicked() && page.may_leave(ui, dir) {
        requests.push(Admin::CreateRole {
            name: t!("role.new").into(),
            rank: NEW_ROLE_RANK.min(mine - 1),
        });
        page.creating = Some(dir.roles.iter().map(|role| role.id.clone()).collect());
        *message = None;
    }
    ui::section(ui, &t!("admin.roles_count", count = dir.roles.len()));
    ui.add_space(ui::SPACE_XS);
    ui.spacing_mut().item_spacing.y = 2.0;

    let roles = ordered(dir, page.order.as_ref());
    let mut rows = Vec::with_capacity(roles.len());
    let mut picked = None;
    for role in &roles {
        let count = (dir.members.iter())
            .filter(|member| member.roles.contains(&role.id))
            .count();
        let selected = page.selected.as_ref() == Some(&role.id);
        let dragged = page.dragging.as_ref() == Some(&role.id);
        let row = role_row(ui, role, count, selected, dragged, movable(role, mine));
        if row.clicked() && !selected {
            picked = Some(role.id.clone());
        }
        if row.drag_started() {
            page.dragging = Some(role.id.clone());
        }
        rows.push((role.id.clone(), row));
    }
    if let Some(id) = picked
        && page.may_leave(ui, dir)
    {
        page.selected = Some(id);
        page.reset();
    }

    let Some(dragged) = page.dragging.clone() else {
        return;
    };
    let pointer = ui.ctx().pointer_interact_pos();
    let others: Vec<&(String, egui::Response)> =
        rows.iter().filter(|(id, _)| *id != dragged).collect();
    // Where it lands: after every other row whose middle is above the pointer.
    let slot = pointer.map_or(0, |at| {
        others
            .iter()
            .filter(|(_, row)| row.rect.center().y < at.y)
            .count()
    });
    let line_y = match slot {
        0 => others.first().map(|(_, row)| row.rect.top() - 1.0),
        n => others.get(n - 1).map(|(_, row)| row.rect.bottom() + 1.0),
    };
    let mut order: Vec<String> = others.iter().map(|(id, _)| id.clone()).collect();
    order.insert(slot.min(order.len()), dragged.clone());
    let moved: Vec<&Role> = (order.iter())
        .filter_map(|id| dir.roles.iter().find(|role| role.id == *id))
        .collect();
    let fits = ranks(&moved, mine).is_some();
    let released = rows
        .iter()
        .find(|(id, _)| *id == dragged)
        .is_none_or(|(_, row)| row.drag_stopped());
    if released {
        page.dragging = None;
        if !fits {
            *message = Some((t!("admin.role_move_refused").into(), true));
        } else if order
            == dir
                .roles
                .iter()
                .map(|role| role.id.clone())
                .collect::<Vec<_>>()
        {
            page.order = None;
        } else if page.order.as_ref() != Some(&order) {
            page.order = Some(order);
        }
        return;
    }
    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    if let Some(y) = line_y {
        let color = if fits {
            ui::accent(ui).base
        } else {
            ui::colors(ui).danger
        };
        ui.painter()
            .hline(ui.max_rect().x_range(), y, Stroke::new(2.0, color));
    }
    if let (Some(at), Some(role)) = (pointer, dir.roles.iter().find(|role| role.id == dragged)) {
        ui::drag_preview(
            ui.ctx(),
            ui.scope_id().with("dragged role"),
            at + Vec2::new(8.0, -LIST_ROW / 2.0),
            LIST_WIDTH - 24.0,
            &role.name,
        );
    }
}

/// One role in the list: its colour, name and member count; open ones are filled.
fn role_row(
    ui: &mut Ui,
    role: &Role,
    count: usize,
    selected: bool,
    dragged: bool,
    draggable: bool,
) -> egui::Response {
    let sense = if draggable {
        Sense::click_and_drag()
    } else {
        Sense::click()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), LIST_ROW), sense);
    let c = ui::colors(ui);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        ui::HOVER_SECONDS,
    );
    let fill = if selected {
        c.selected
    } else {
        Color32::TRANSPARENT.lerp_to_gamma(c.hover_soft, hover)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, ui::RADIUS_ROW, fill);
    let opacity = if dragged { 0.4 } else { 1.0 };
    let dot = egui::pos2(rect.left() + 14.0, rect.center().y);
    painter.circle_filled(
        dot,
        5.0,
        role.color.unwrap_or(c.muted).gamma_multiply(opacity),
    );
    let count_text = count.to_string();
    let count_galley = painter.layout_no_wrap(
        count_text,
        egui::FontId::proportional(ui::TEXT_SMALL),
        c.muted,
    );
    let count_at = egui::pos2(
        rect.right() - 10.0 - count_galley.size().x,
        rect.center().y - count_galley.size().y / 2.0,
    );
    let name_width = (count_at.x - rect.left() - 28.0 - ui::SPACE_S).max(0.0);
    let text = if selected { c.strong } else { c.text };
    let mut job = egui::text::LayoutJob::simple_singleline(
        role.name.clone(),
        egui::FontId::new(ui::TEXT_BODY, ui::bold_family()),
        text.gamma_multiply(opacity),
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(name_width);
    let name = painter.layout_job(job);
    painter.galley(
        egui::pos2(rect.left() + 28.0, rect.center().y - name.size().y / 2.0),
        name,
        text,
    );
    painter.galley(count_at, count_galley, c.muted);
    ui::focus_ring(ui, &response, ui::RADIUS_ROW);
    let label = t!("admin.role_row", count = count, name = role.name);
    response.widget_info(|| WidgetInfo::selected(A11y::Tab, true, selected, &label));
    let cursor = if draggable {
        CursorIcon::Grab
    } else {
        CursorIcon::PointingHand
    };
    response.on_hover_cursor(cursor)
}

/// The open role: its name, then the Display, Permissions and Members tabs.
fn editor(ui: &mut Ui, server: &Server, page: &mut RolesPage) {
    let dir = &server.dir;
    let Some(role) = page.role(dir).cloned() else {
        return;
    };
    let mine = dir.perms.rank;
    // The Owner role's name and colour are the owner's to change.
    let editable = role.rank < mine || (dir.perms.owner && role.id == "owner");
    let name = page.name.clone().unwrap_or_else(|| role.name.clone());
    ui.horizontal(|ui| {
        ui::color_dot(ui, page.color.unwrap_or(role.color));
        ui.add(Label::new(ui::title(t!("admin.edit_role_named", name = name.trim()))).truncate());
    });
    if !editable {
        ui.label(ui::caption(t!("admin.role_above_you")));
    }
    ui.add_space(ui::SPACE_S);
    let before = page.tab;
    let mut tab = before;
    ui::tabs(
        ui,
        &mut tab,
        &[
            (RoleTab::Display, t!("admin.role_display")),
            (RoleTab::Permissions, t!("admin.permissions")),
            (RoleTab::Members, t!("admin.members")),
        ],
    );
    page.tab = tab;
    ui.add_enabled_ui(editable, |ui| match page.tab {
        RoleTab::Display => display_tab(ui, &role, page),
        RoleTab::Permissions => permissions_tab(ui, dir, &role, page),
        RoleTab::Members => members_tab(ui, server, &role, page),
    });
}

fn display_tab(ui: &mut Ui, role: &Role, page: &mut RolesPage) {
    let label = ui.label(ui::bold(t!("admin.role_name")));
    let name = page.name.get_or_insert_with(|| role.name.clone());
    ui.add(
        ui::field(name)
            .char_limit(100)
            .desired_width(ui.available_width()),
    )
    .labelled_by(label.id);
    if name.trim().is_empty() {
        ui::error_text(ui, Some(t!("admin.role_name_needed")));
    }
    ui.add_space(ui::SPACE_L);
    ui.label(ui::bold(t!("admin.role_color")));
    ui.label(ui::caption(t!("admin.role_color_help")));
    ui.add_space(ui::SPACE_XS);
    let mut color = page.color.unwrap_or(role.color);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if ui::color_swatch(ui, None, color.is_none(), t!("admin.no_color")).clicked() {
            color = None;
        }
        for choice in ui::ROLE_COLORS {
            let label = format!("#{:02x}{:02x}{:02x}", choice.r(), choice.g(), choice.b());
            if ui::color_swatch(ui, Some(choice), color == Some(choice), &label).clicked() {
                color = Some(choice);
            }
        }
        ui.add_space(ui::SPACE_S);
        // Anything else, from a picker.
        let mut custom = color.unwrap_or(ui::colors(ui).muted);
        ui.spacing_mut().interact_size = Vec2::new(30.0, 22.0);
        let picker =
            color_picker::color_edit_button_srgba(ui, &mut custom, color_picker::Alpha::Opaque);
        picker.widget_info(|| WidgetInfo::labeled(A11y::ColorWell, true, t!("admin.custom_color")));
        if picker.on_hover_text(t!("admin.custom_color")).changed() {
            color = Some(custom);
        }
    });
    if color != page.color.unwrap_or(role.color) {
        page.color = Some(color);
    }
    ui.add_space(ui::SPACE_M);
    let mut separate = page.separate.unwrap_or(role.separate);
    if ui::toggle_row(
        ui,
        &mut separate,
        t!("admin.role_separate"),
        t!("admin.role_separate_help"),
    )
    .changed()
    {
        page.separate = Some(separate);
    }
    if role.custom {
        // Not `ui::danger_zone`: that adds a page section, and this is part of one.
        ui.add_space(ui::SPACE_L);
        ui.separator();
        ui::setting_row(
            ui,
            t!("admin.delete_role"),
            t!("admin.delete_role_help"),
            |ui| {
                if ui::danger(ui, t!("admin.delete_role")).clicked() {
                    page.deleting = true;
                }
            },
        );
    }
}

fn permissions_tab(ui: &mut Ui, dir: &Directory, role: &Role, page: &mut RolesPage) {
    if !role.custom {
        ui.label(ui::caption(t!("admin.built_in_permissions")));
    }
    let chosen = page
        .permissions
        .get_or_insert_with(|| role.permissions.clone());
    for (at, (group, permissions)) in groups(dir.calendar.is_some()).into_iter().enumerate() {
        if at > 0 {
            ui.add_space(ui::SPACE_S);
        }
        ui::section(ui, group);
        for (permission, label, help, care) in permissions {
            let mut on = chosen.iter().any(|p| p == permission);
            // Only permissions you hold can be given or taken away.
            let yours = holds(dir, permission);
            let tag = care.then_some(t!("admin.needs_care"));
            ui.push_id(permission, |ui| {
                ui::tagged_row(ui, label, tag, help, |ui| {
                    let toggle = ui
                        .add_enabled_ui(role.custom && yours, |ui| ui::toggle(ui, &mut on, label))
                        .inner;
                    if !yours && role.custom {
                        toggle.on_disabled_hover_text(t!("admin.not_yours"));
                    } else if toggle.changed() {
                        chosen.retain(|p| p != permission);
                        if on {
                            chosen.push(permission.to_owned());
                        }
                    }
                });
            });
        }
    }
}

fn members_tab(ui: &mut Ui, server: &Server, role: &Role, page: &mut RolesPage) {
    let dir = &server.dir;
    if role.id == "owner" {
        ui.label(ui::caption(t!("admin.owner_members")));
    }
    let has = |member: &Member| match page.members.iter().find(|(p, _)| *p == member.person) {
        Some((_, add)) => *add,
        None => member.roles.contains(&role.id),
    };
    // You can change people ranked below you, and only for roles below yours.
    let changeable = |member: &Member| {
        role.id != "owner"
            && role.rank < dir.perms.rank
            && member_rank(dir, member) < dir.perms.rank
    };
    let mut flip = None;
    if role.id != "owner" {
        let search = ui.add(
            ui::field(&mut page.search)
                .hint_text(t!("admin.add_members"))
                .desired_width(ui.available_width()),
        );
        search
            .widget_info(|| WidgetInfo::labeled(A11y::SearchInput, true, t!("admin.add_members")));
        let query = page.search.trim().to_lowercase();
        if !query.is_empty() {
            let found: Vec<&Member> = (dir.members.iter())
                .filter(|member| !has(member) && changeable(member))
                .filter(|member| member.display_name().to_lowercase().contains(&query))
                .take(MATCHES)
                .collect();
            if found.is_empty() {
                ui.weak(t!("admin.nobody_found"));
            }
            for member in found {
                let name = member.display_name();
                let row =
                    ui::row_button(ui, MEMBER_ROW, &t!("admin.add_member", name = name), |ui| {
                        face(ui, dir, &member.person, &name, 28.0, false);
                        ui.label(ui::bold(&name));
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(ui::caption(t!("admin.add")));
                        });
                    });
                if row.clicked() {
                    flip = Some((member.person, true));
                }
            }
            ui.add_space(ui::SPACE_S);
        }
    }
    let members: Vec<&Member> = dir.members.iter().filter(|m| has(m)).collect();
    ui::section(ui, &t!("admin.role_members_count", count = members.len()));
    if members.is_empty() {
        ui.weak(t!("admin.no_role_members"));
    }
    for member in members {
        let name = member.display_name();
        ui.push_id(member.person, |ui| {
            ui::right_row(ui, MEMBER_ROW, |ui| {
                let allowed = changeable(member);
                let remove = ui
                    .add_enabled_ui(allowed, |ui| {
                        ui::small_close(ui, &t!("admin.remove_member", name = name))
                    })
                    .inner;
                if remove.clicked() {
                    flip = Some((member.person, false));
                }
                ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| {
                    face(ui, dir, &member.person, &name, 28.0, false);
                    ui.add(Label::new(ui::bold(&name)).truncate());
                });
            });
        });
    }
    if let Some((person, add)) = flip {
        let before = dir
            .members
            .iter()
            .any(|m| m.person == person && m.roles.contains(&role.id));
        page.members.retain(|(p, _)| *p != person);
        if add != before {
            page.members.push((person, add));
        }
        if add {
            page.search.clear();
        }
    }
}

/// What Save sends: the open role's look, its permissions and every rank that moved,
/// then each member who gains or loses it.
fn save_requests(dir: &Directory, page: &RolesPage) -> Vec<Admin> {
    let mut out = Vec::new();
    let role = page.role(dir);
    if let Some(role) = role {
        let name = (page.name.as_deref())
            .map(str::trim)
            .filter(|name| *name != role.name);
        let color = page.color.unwrap_or(role.color);
        let separate = page.separate.unwrap_or(role.separate);
        if name.is_some() || color != role.color || separate != role.separate {
            out.push(Admin::StyleRole {
                id: role.id.clone(),
                name: name.map(str::to_owned),
                color,
                separate,
            });
        }
    }
    let moved = page.order.as_ref().and_then(|order| {
        let roles = ordered(dir, Some(order));
        let ranks = ranks(&roles, dir.perms.rank)?;
        Some(roles.into_iter().zip(ranks).collect::<Vec<_>>())
    });
    let mut edits: Vec<(&Role, Option<i64>)> = (moved.into_iter().flatten())
        .filter(|(role, rank)| role.rank != *rank)
        .map(|(role, rank)| (role, Some(rank)))
        .collect();
    if let Some(role) = role
        && role.custom
        && (page.permissions.as_ref()).is_some_and(|chosen| !same(chosen, &role.permissions))
        && !edits.iter().any(|(r, _)| r.id == role.id)
    {
        edits.push((role, None));
    }
    for (edited, rank) in edits {
        let permissions = match (role, &page.permissions) {
            (Some(open), Some(chosen)) if open.id == edited.id => chosen.clone(),
            _ => edited.permissions.clone(),
        };
        out.push(Admin::EditRole {
            id: edited.id.clone(),
            permissions,
            rank,
        });
    }
    if let Some(role) = role {
        for (person, add) in &page.members {
            let Some(member) = dir.members.iter().find(|m| m.person == *person) else {
                continue;
            };
            // The Owner role comes with owning the server, not from this list.
            let mut roles: Vec<String> = (member.roles.iter())
                .filter(|id| *id != "owner" && **id != role.id)
                .cloned()
                .collect();
            if *add {
                roles.push(role.id.clone());
            }
            out.push(Admin::SetRoles(*person, roles));
        }
    }
    out
}

/// Delete asks first. A role still in use can't go: say what to do instead.
pub fn delete_dialog(
    ctx: &egui::Context,
    server: &Server,
    page: &mut RolesPage,
    message: &Option<(String, bool)>,
) -> Option<Admin> {
    if !page.deleting {
        return None;
    }
    let dir = &server.dir;
    let role = page.role(dir)?.clone();
    let in_use = dir.members.iter().any(|m| m.roles.contains(&role.id));
    let default = role.id == dir.default_role;
    let mut request = None;
    let mut cancel = false;
    let title = t!("admin.delete_role_title", name = role.name);
    let ((), dismissed) = ui::dialog(ctx, "delete-role", &title, |ui| {
        ui.label(if default {
            t!("admin.delete_role_default")
        } else if in_use {
            t!("admin.delete_role_in_use")
        } else {
            t!("admin.delete_role_confirm")
        });
        if let Some((text, true)) = message {
            ui::error_text(ui, Some(text));
        }
        ui::footer(ui, |ui| {
            if ui
                .add_enabled_ui(!default && !in_use, |ui| {
                    ui::danger(ui, t!("admin.delete_role"))
                })
                .inner
                .clicked()
            {
                request = Some(Admin::DeleteRole(role.id.clone()));
            }
            cancel = ui::secondary(ui, t!("common.cancel")).clicked();
        });
    });
    if cancel || dismissed {
        page.deleting = false;
    }
    if request.is_some() {
        page.reset();
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: &str, rank: i64, custom: bool) -> Role {
        Role {
            id: id.into(),
            name: id.into(),
            name_known: true,
            rank,
            permissions: Vec::new(),
            custom,
            color: None,
            separate: false,
        }
    }

    #[test]
    fn moved_roles_get_ranks_between_their_fixed_neighbours() {
        let owner = role("owner", 1000, false);
        let admin = role("admin", 100, false);
        let moderator = role("moderator", 50, false);
        let member = role("member", 0, false);
        let (a, b) = (role("a", 25, true), role("b", 25, true));
        // Two roles made at the same rank, b put above a: they spread out under Moderator.
        let order = [&owner, &admin, &moderator, &b, &a, &member];
        assert_eq!(ranks(&order, 1000), Some(vec![1000, 100, 50, 33, 17, 0]));
        // Roles that already fit keep their rank.
        let (c, d) = (role("c", 40, true), role("d", 10, true));
        let order = [&owner, &admin, &moderator, &c, &d, &member];
        assert_eq!(ranks(&order, 1000), Some(vec![1000, 100, 50, 40, 10, 0]));
        // Above Moderator, for an admin.
        let order = [&owner, &admin, &c, &moderator, &d, &member];
        assert_eq!(ranks(&order, 100), Some(vec![1000, 100, 75, 50, 10, 0]));
        // Not above your own rank, and never under Member.
        let order = [&owner, &admin, &c, &moderator, &d, &member];
        assert_eq!(ranks(&order, 50), None);
        let order = [&owner, &admin, &moderator, &member, &d];
        assert_eq!(ranks(&order, 1000), None);
    }

    #[test]
    fn the_unsaved_bar_shows_only_real_changes() {
        let mut role = role("a", 10, true);
        role.permissions = vec!["kick".into(), "ban".into()];
        let dir = Directory {
            roles: vec![role],
            ..Directory::default()
        };
        let mut page = RolesPage {
            selected: Some("a".into()),
            ..RolesPage::default()
        };
        page.name = Some(" a ".into());
        page.permissions = Some(vec!["ban".into(), "kick".into()]);
        page.color = Some(None);
        assert!(!page.dirty(&dir), "the same values in another order");
        page.color = Some(Some(ui::ROLE_COLORS[0]));
        assert!(page.dirty(&dir));
        let requests = save_requests(&dir, &page);
        assert!(matches!(
            requests.as_slice(),
            [Admin::StyleRole {
                name: None,
                color: Some(_),
                separate: false,
                ..
            }]
        ));
        page.saving = true;
        page.done();
        assert!(!page.dirty(&dir));
    }
}
