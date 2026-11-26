use super::*;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

const CROWD: usize = 300;

fn role(id: &str, name: &str, color: Option<Color32>, separate: bool) -> crate::server::Role {
    crate::server::Role {
        id: id.into(),
        name: name.into(),
        name_known: true,
        rank: 0,
        permissions: Vec::new(),
        custom: true,
        color,
        separate,
    }
}

/// `CROWD` people besides this one: a few admins and helpers, some offline, some hidden.
fn crowd(dir: &mut Directory) {
    dir.me = ME;
    dir.roles = vec![
        role("admin", "Admins", Some(ui::ACCENTS[0].base), true),
        role("helper", "Helpers", None, true),
        role("vip", "VIP", Some(ui::ACCENTS[2].base), false),
        role("member", "Members", None, false),
    ];
    let member = |n: usize| {
        let mut roles = vec!["member".to_owned()];
        match n % 25 {
            0 => roles.push("admin".into()),
            1 | 2 => roles.push("helper".into()),
            3 => roles.extend(["vip".into(), "helper".into()]),
            _ => {}
        }
        let mut person = [0; 32];
        person[..8].copy_from_slice(&(n as u64 + 100).to_le_bytes());
        Member {
            person,
            name: (n % 50 != 7).then(|| format!("Member {n:03}")),
            devices: Vec::new(),
            roles,
            avatar: None,
            presence: match n {
                _ if n.is_multiple_of(3) => Presence::Offline,
                _ if n % 7 == 1 => Presence::Away,
                _ if n % 4 == 2 => Presence::Dnd,
                _ => Presence::Online,
            },
            hide_offline: n.is_multiple_of(9),
            bio: None,
            voice_muted: false,
            timeout_until: None,
        }
    };
    let mut me = member(0);
    me.person = ME;
    me.name = Some("Me".into());
    me.hide_offline = true;
    me.roles = vec!["member".into()];
    dir.members = std::iter::once(me).chain((1..=CROWD).map(member)).collect();
}

/// The grouping as `member_list` built it every frame before it was cached, plus the
/// status-then-name order inside each group.
fn old_grouping(dir: &Directory) -> Vec<(Option<String>, Vec<Id>)> {
    let listed: Vec<&Member> = (dir.members.iter())
        .filter(|m| m.listed() || m.person == dir.me)
        .collect();
    let mut groups: Vec<(Option<&str>, Vec<&Member>)> = (dir.roles.iter())
        .filter(|role| role.separate)
        .map(|role| (Some(role.id.as_str()), Vec::new()))
        .collect();
    groups.push((None, Vec::new()));
    for member in listed {
        let heading = dir.heading_role(&member.roles).map(|role| role.id.as_str());
        if let Some((_, list)) = groups.iter_mut().find(|(id, _)| *id == heading) {
            list.push(member);
        }
    }
    let status = |m: &Member| {
        [
            Presence::Online,
            Presence::Dnd,
            Presence::Away,
            Presence::Offline,
        ]
        .iter()
        .position(|p| *p == m.presence)
    };
    for (_, list) in &mut groups {
        list.sort_by_key(|m| (status(m), m.display_name().to_lowercase()));
    }
    (groups.into_iter())
        .map(|(id, list)| {
            (
                id.map(str::to_owned),
                list.iter().map(|m| m.person).collect(),
            )
        })
        .collect()
}

fn new_grouping(dir: &Directory) -> Vec<(Option<String>, Vec<Id>)> {
    let groups = MemberGroups::of(dir);
    (groups.groups.iter())
        .map(|(id, list)| {
            let people = list.iter().map(|&at| dir.members[at].person).collect();
            (id.clone(), people)
        })
        .collect()
}

#[test]
fn member_groups_match_the_old_grouping() {
    let mut dir = Directory::default();
    crowd(&mut dir);
    assert_eq!(new_grouping(&dir), old_grouping(&dir));
    assert_eq!(
        MemberGroups::of(&dir).listed,
        old_grouping(&dir)
            .iter()
            .map(|(_, l)| l.len())
            .sum::<usize>()
    );
    // No separate roles: one group without headings.
    for role in &mut dir.roles {
        role.separate = false;
    }
    assert_eq!(new_grouping(&dir), old_grouping(&dir));
    assert_eq!(MemberGroups::of(&dir).groups.len(), 1);
    // A lower role listed separately, and an empty list.
    dir.roles[2].separate = true;
    assert_eq!(new_grouping(&dir), old_grouping(&dir));
    dir.members = crate::server::Members::default();
    assert_eq!(new_grouping(&dir), old_grouping(&dir));
}

#[test]
fn member_groups_list_online_then_dnd_then_away_then_offline_by_name() {
    let mut dir = Directory {
        me: ME,
        roles: vec![role("admin", "Admins", None, true)],
        ..Default::default()
    };
    let member = |n: u8, name: &str, presence: Presence, admin: bool| Member {
        person: [n; 32],
        name: Some(name.into()),
        devices: Vec::new(),
        roles: if admin {
            vec!["admin".into()]
        } else {
            Vec::new()
        },
        avatar: None,
        presence,
        hide_offline: false,
        bio: None,
        voice_muted: false,
        timeout_until: None,
    };
    dir.members = vec![
        member(1, "zoe", Presence::Offline, false),
        member(2, "Yara", Presence::Away, false),
        member(3, "xavi", Presence::Dnd, false),
        member(4, "Will", Presence::Online, false),
        member(5, "anna", Presence::Offline, false),
        member(6, "Bob", Presence::Away, false),
        member(7, "carl", Presence::Dnd, false),
        member(8, "Dana", Presence::Online, false),
        member(9, "Ola", Presence::Offline, true),
        member(10, "Pim", Presence::Online, true),
    ]
    .into();
    let names = |dir: &Directory| -> Vec<Vec<String>> {
        (MemberGroups::of(dir).groups.iter())
            .map(|(_, list)| {
                list.iter()
                    .map(|&at| dir.members[at].display_name())
                    .collect()
            })
            .collect()
    };
    assert_eq!(
        names(&dir),
        [
            vec!["Pim", "Ola"],
            vec!["Dana", "Will", "carl", "xavi", "Bob", "Yara", "anna", "zoe"],
        ]
    );
    // Presence changes re-sort: anna comes online, Dana goes offline.
    let ctx = egui::Context::default();
    let before = MemberGroups::cached(&ctx, &dir);
    dir.members.edit()[4].presence = Presence::Online;
    dir.members.edit()[7].presence = Presence::Offline;
    let after = MemberGroups::cached(&ctx, &dir);
    assert!(!Arc::ptr_eq(&before, &after));
    assert_eq!(
        names(&dir)[1],
        ["anna", "Will", "carl", "xavi", "Bob", "Yara", "Dana", "zoe"]
    );
}

#[test]
fn member_groups_regroup_only_when_the_grouping_can_change() {
    let ctx = egui::Context::default();
    let mut dir = Directory::default();
    crowd(&mut dir);
    let first = MemberGroups::cached(&ctx, &dir);
    let same = |dir: &Directory, before: &Arc<MemberGroups>| {
        let now = MemberGroups::cached(&ctx, dir);
        assert_eq!(new_grouping(dir), old_grouping(dir));
        (Arc::ptr_eq(before, &now), now)
    };
    let (kept, groups) = same(&dir, &first);
    assert!(kept, "nothing changed");

    // Captions and colours are read when drawn, so renaming a role keeps the cache.
    dir.roles[0].name = "Owners".into();
    dir.roles[0].color = None;
    let (kept, groups) = same(&dir, &groups);
    assert!(kept);

    // Someone hidden while offline comes online: they are listed again.
    let hidden = (dir.members.iter())
        .position(|m| !m.listed() && m.person != ME)
        .unwrap();
    dir.members.edit()[hidden].presence = Presence::Online;
    let (kept, groups) = same(&dir, &groups);
    assert!(!kept, "presence changed");
    assert_eq!(groups.listed, first.listed + 1);

    // A role change moves someone under another heading.
    dir.members.edit()[5].roles.push("admin".into());
    let (kept, groups) = same(&dir, &groups);
    assert!(!kept, "member roles changed");

    // A role stops being listed separately.
    dir.roles[1].separate = false;
    let (kept, groups) = same(&dir, &groups);
    assert!(!kept, "separate roles changed");
    assert_eq!(groups.groups.len(), 2);

    // A fresh directory from the server.
    dir.members = dir.members.iter().cloned().collect();
    let (kept, groups) = same(&dir, &groups);
    assert!(!kept, "members replaced");

    // Another person signed in on the same list.
    dir.me = ANN;
    let (kept, _) = same(&dir, &groups);
    assert!(!kept, "me changed");
}

fn harness(width: f32, height: f32, dark: bool) -> Harness<'static, Home> {
    let (mut fixture, _sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    crowd(&mut home.servers[0].dir);
    let theme = if dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    let mut h = Harness::builder()
        .with_size(Vec2::new(width, height))
        .with_context_setup(ui::install)
        .with_theme(theme)
        .build_ui_state(
            |ui, home: &mut Home| {
                let colors = ui::colors(ui);
                let size = if ui::is_phone(ui.ctx()) {
                    ui.available_width()
                } else {
                    home.layout.members
                };
                Panel::right("members")
                    .resizable(false)
                    .exact_size(size)
                    .frame(panel_frame(colors.side))
                    .show(ui, |ui| member_list(ui, home, 0));
                CentralPanel::default()
                    .frame(panel_frame(colors.main))
                    .show(ui, |_| {});
            },
            home,
        );
    h.run_steps(4);
    h
}

/// The member rows (40 points tall buttons) the list built this frame, top down.
fn rows(h: &Harness<'_, Home>) -> Vec<(String, Rect)> {
    (h.query_all(by().role(Role::Button)))
        .filter(|node| (node.rect().height() - 40.0).abs() < 0.5)
        .filter_map(|node| Some((node.accesskit_node().label()?, node.rect())))
        .collect()
}

fn names(rows: &[(String, Rect)]) -> Vec<&str> {
    rows.iter().map(|(name, _)| name.as_str()).collect()
}

#[test]
fn member_list_builds_only_the_rows_in_view() {
    for ((width, height), dark) in [(390.0, 844.0), (1100.0, 700.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let mut h = harness(width, height, dark);
        if let Some(dir) = std::env::var_os("RUSTTALK_MEMBER_LIST_SCREENSHOTS") {
            let theme = if dark { "dark" } else { "light" };
            h.render()
                .unwrap()
                .save(PathBuf::from(dir).join(format!("{theme}-{width}x{height}.png")))
                .unwrap();
        }
        let dir = &h.state().servers[0].dir;
        let listed = MemberGroups::of(dir).listed;
        assert!(listed > 250, "{listed} listed");
        let screenful = (height / 42.0).ceil() as usize + 1;
        let top = rows(&h);
        assert!(
            !top.is_empty() && top.len() <= screenful,
            "{} rows built at {width}x{height}",
            top.len()
        );
        // The first heading's people come first, in list order: an admin who is online.
        let admins = &new_grouping(dir)[0].1;
        let first = dir.members.get(&admins[0]).unwrap().display_name();
        assert_eq!(top[0].0, first);
        // Every row in view is built: they fill the panel down to its bottom.
        assert!(
            top.last().unwrap().1.bottom() >= height - 42.0,
            "{:?}",
            names(&top)
        );

        // Scrolled to the end, the last people are built and the first are not.
        h.hover_at(top[0].1.center());
        for _ in 0..30 {
            h.input_mut().events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Page,
                delta: Vec2::new(0.0, -10.0),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                modifiers: Modifiers::NONE,
            });
            h.run_steps(2);
        }
        h.run_steps(10);
        let dir = &h.state().servers[0].dir;
        let grouping = new_grouping(dir);
        let everyone = &grouping.last().unwrap().1;
        let last = dir
            .members
            .get(everyone.last().unwrap())
            .unwrap()
            .display_name();
        let bottom = rows(&h);
        assert!(
            bottom.len() <= screenful,
            "{} rows at the end",
            bottom.len()
        );
        assert!(
            names(&bottom).contains(&last.as_str()),
            "{last} not in {:?}",
            names(&bottom)
        );
        assert!(!names(&bottom).contains(&first.as_str()));
    }
}
