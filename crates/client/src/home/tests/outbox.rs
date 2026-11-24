use super::*;
use crate::net::Update;
use egui_kittest::kittest::Queryable;
use std::path::PathBuf;

/// A message shows at once while sending, stays with Try again when sending fails,
/// and is replaced by the server's record (from the answer and the live event) once.
#[test]
fn sent_message_shows_at_once_then_fails_retries_and_is_confirmed_once() {
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let (mut h, mut commands) = files_harness(dark);
            h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
            h.set_size(size);
            let home = h.state_mut().as_mut().unwrap();
            // Records of mine carry this device's person, as the server's do.
            let me = *home.identity.certificate.person_id();
            let server = &mut home.servers[0];
            server.dir.me = me;
            server.dir.members.edit()[0].person = me;
            let address = server.address.clone();
            let chat = server.chats.get_mut(&PROJECTS).unwrap();
            chat.at_start = true;
            chat.merge(
                vec![Msg {
                    backfilled: false,
                    id: [40; 32],
                    person: ANN,
                    at: 1_700_000_000,
                    reply_to: None,
                    quoted_author: None,
                    edit_of: None,
                    body: Body::Text("Who's coming tonight?".into()),
                    files: Vec::new(),
                    live: None,
                    sticker: None,
                    replicas: Vec::new(),
                    copy_files: Vec::new(),
                }],
                false,
            );
            chat.draft = "Count me in".into();
            h.run_steps(3);
            h.get_by_label(t!("home.send")).click_accesskit();
            h.run_steps(3);
            let mut next = || {
                std::iter::from_fn(|| commands.try_recv().ok()).find_map(|cmd| match cmd {
                    Command::Send(draft) => Some(draft),
                    _ => None,
                })
            };
            let draft = next().expect("sent");
            assert_eq!(draft.text, "Count me in");
            h.get_by_label(t!("home.sending"));
            assert!(h.query_all_by_value("Count me in").next().is_some());
            let capture = |h: &mut egui_kittest::Harness<'static, Option<Home>>, phase| {
                if let Some(dir) = std::env::var_os("RUSTTALK_OUTBOX_SCREENSHOTS") {
                    std::fs::create_dir_all(&dir).unwrap();
                    let theme = if dark { "dark" } else { "light" };
                    let name = format!("{phase}-{}x{}-{theme}.png", size.x, size.y);
                    h.render()
                        .unwrap()
                        .save(PathBuf::from(dir).join(name))
                        .unwrap();
                }
            };
            capture(&mut h, "sending");

            let ctx = h.ctx.clone();
            let home = h.state_mut().as_mut().unwrap();
            home.apply(&ctx, Update::Unsent(address.clone(), draft.clone()), 1.);
            h.run_steps(3);
            h.get_by_label(t!("home.not_sent"));
            assert!(h.query_by_label(t!("home.sending")).is_none());
            assert!(h.query_all_by_value("Count me in").next().is_some());
            let chat = &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS];
            assert!(
                chat.draft.is_empty(),
                "it stays in the chat, not the composer"
            );
            capture(&mut h, "failed");

            h.get_by_label(t!("common.try_again")).click_accesskit();
            h.run_steps(3);
            assert_eq!(next().expect("sent again").id, draft.id);
            h.get_by_label(t!("home.sending"));

            // The live event and the send's answer both bring the record.
            let record = Msg {
                backfilled: false,
                id: draft.id,
                person: me,
                at: 1_700_000_060,
                reply_to: None,
                quoted_author: None,
                edit_of: None,
                body: Body::Text("Count me in".into()),
                files: Vec::new(),
                live: None,
                sticker: None,
                replicas: Vec::new(),
                copy_files: Vec::new(),
            };
            let home = h.state_mut().as_mut().unwrap();
            for _ in 0..2 {
                let update = Update::Messages {
                    server: address.clone(),
                    channel: PROJECTS,
                    items: vec![record.clone()],
                    older: false,
                    page: None,
                };
                home.apply(&ctx, update, 2.);
            }
            h.run_steps(3);
            assert!(h.query_by_label(t!("home.sending")).is_none());
            assert!(h.query_by_label(t!("home.not_sent")).is_none());
            let chat = &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS];
            assert!(chat.outbox.is_empty());
            let mine = chat.messages.iter().filter(|msg| msg.id == draft.id);
            assert_eq!(mine.count(), 1);
            // A late failure of the first try doesn't bring it back.
            let home = h.state_mut().as_mut().unwrap();
            home.apply(&ctx, Update::Unsent(address.clone(), draft.clone()), 3.);
            h.run_steps(3);
            assert!(h.query_by_label(t!("home.not_sent")).is_none());
            let chat = &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS];
            assert!(chat.draft.is_empty());
        }
    }
}
