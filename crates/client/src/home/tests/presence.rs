use super::*;
use crate::net::Update;

/// My bar shows what others see across my devices, and a status picked on another
/// device applies here too.
#[test]
fn my_status_follows_my_other_devices() {
    let (mut h, mut sent) = files_harness(true);
    let ctx = h.ctx.clone();
    let home = h.state_mut().as_mut().unwrap();
    let address = home.servers[0].address.clone();
    home.status = Some(Choice::Online);

    // This device is idle but another one is in use: the server keeps me online.
    home.idle = true;
    assert_eq!(home.shown_status(), Choice::Online);
    // Every device idle: the server shows me away, and so does my bar.
    home.apply(
        &ctx,
        Update::Presence(address.clone(), ME, Presence::Away),
        1.0,
    );
    assert_eq!(home.shown_status(), Choice::Away);
    assert_eq!(home.status, Some(Choice::Online), "still my choice");

    // Picked on my phone.
    home.apply(
        &ctx,
        Update::Chosen(address.clone(), Choice::Invisible),
        1.0,
    );
    assert_eq!(home.shown_status(), Choice::Invisible);

    // Back after a disconnect: the phone's newer pick wins, and the server hears
    // again that this device is idle.
    while sent.try_recv().is_ok() {}
    let mut dir = home.servers[0].dir.clone();
    dir.status = Some(Choice::Away);
    home.apply(&ctx, Update::Directory(address.clone(), Box::new(dir)), 2.0);
    assert_eq!(home.status, Some(Choice::Away));
    let sent_now: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
    assert!(sent_now.iter().any(|c| matches!(c, Command::SetIdle(true))));
    assert!(!sent_now.iter().any(|c| matches!(c, Command::SetStatus(_))));

    // A pick made here while the server was offline reaches it once it is back.
    home.servers[0].status = Status::Offline;
    home.set_status(Choice::Online);
    home.servers[0].status = Status::Online;
    while sent.try_recv().is_ok() {}
    let mut dir = home.servers[0].dir.clone();
    dir.status = Some(Choice::Away);
    home.apply(&ctx, Update::Directory(address, Box::new(dir)), 3.0);
    assert_eq!(home.status, Some(Choice::Online));
    let sent_now: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
    assert!(
        sent_now
            .iter()
            .any(|c| matches!(c, Command::SetStatus(Choice::Online)))
    );
}

/// Do not disturb picked on another device applies here, and this device going idle
/// (or the server showing me away) never turns it into Away.
#[test]
fn do_not_disturb_from_another_device_holds_while_idle() {
    let (mut h, mut sent) = files_harness(true);
    let ctx = h.ctx.clone();
    let home = h.state_mut().as_mut().unwrap();
    let address = home.servers[0].address.clone();
    home.status = Some(Choice::Online);

    home.apply(&ctx, Update::Chosen(address.clone(), Choice::Dnd), 1.0);
    assert!(home.dnd());
    assert_eq!(home.shown_status(), Choice::Dnd);
    home.idle = true;
    home.apply(
        &ctx,
        Update::Presence(address.clone(), ME, Presence::Away),
        1.0,
    );
    assert_eq!(home.shown_status(), Choice::Dnd);

    // A reconnect keeps the stored Do not disturb instead of sending another status.
    while sent.try_recv().is_ok() {}
    let mut dir = home.servers[0].dir.clone();
    dir.status = Some(Choice::Dnd);
    home.apply(&ctx, Update::Directory(address, Box::new(dir)), 2.0);
    assert!(home.dnd());
    let sent_now: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
    assert!(!sent_now.iter().any(|c| matches!(c, Command::SetStatus(_))));
}

/// While on Do not disturb a new message shows no notification, but still marks its
/// channel unread.
#[test]
fn do_not_disturb_keeps_notifications_quiet() {
    let (mut h, _sent) = files_harness(true);
    let ctx = h.ctx.clone();
    let home = h.state_mut().as_mut().unwrap();
    let address = home.servers[0].address.clone();
    home.focused = false;
    let message = |id: u8| Msg {
        id: [id; 32],
        person: ANN,
        at: notify::unix_now() + i64::from(id),
        body: Body::Text("lunch?".into()),
        backfilled: false,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        files: vec![],
        live: None,
        sticker: None,
        replicas: vec![],
        copy_files: vec![],
    };

    home.status = Some(Choice::Dnd);
    home.arrived(&ctx, 0, PROJECTS, message(1));
    assert_eq!(home.notify.shown, 0);
    assert!(home.notify.unread(&address, &PROJECTS));

    home.status = Some(Choice::Online);
    home.arrived(&ctx, 0, PROJECTS, message(2));
    assert_eq!(home.notify.shown, 1);
}
