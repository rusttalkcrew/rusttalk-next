//! Report a message (#678): why, and an optional note. The app sends the signed message
//! with the key of only that message (and of its files), sealed to the people who review
//! reports; for a DM, the server's owner. The dialog names every recipient before
//! anything is sent. See `rusttalk_core::reports`.

use eframe::egui::{RichText, Ui};
use rusttalk_core::{
    identity::fingerprint,
    reports::{MAX_NOTE_CHARS, Reason},
};

use super::At;
use crate::{
    net::{Command, reports::Outgoing, reports::Recipients},
    server::{Body, Chat, Id, Server, Status},
    ui::{self, error_text},
};

/// A report being written. Frozen to the copy that was shown, so a reload can't
/// retarget it.
pub struct Draft {
    pub server: usize,
    pub channel: Id,
    pub message: Id,
    /// The edit that was shown, when the message was edited.
    pub edit: Option<Id>,
    pub dm: bool,
    pub sender: Id,
    pub reason: Option<Reason>,
    pub note: String,
    /// Who it would go to, once looked up (see `open`).
    pub recipients: Option<Result<Recipients, String>>,
    /// For a DM held on several servers: each server (index) and its DM channel the
    /// reporter may pick, whose owner then gets the report.
    pub choices: Vec<(usize, Id)>,
}

/// Whether this message can be reported: someone else's, readable, on a server that is
/// online.
pub fn reportable(body: &Body, mine: bool) -> bool {
    !mine && !matches!(body, Body::Unreadable | Body::Notice { .. })
}

/// A report of `person`'s message `id` as shown in this chat. In a DM, of a copy on a
/// server that holds it, is online and still has the DM (see `dm_choices`).
pub fn draft(servers: &[Server], at: At, chat: &Chat, (id, person): (Id, Id)) -> Option<Draft> {
    match at {
        At::Channel(index, channel) => in_channel(index, channel, chat, (id, person), false),
        At::Dm(peer) => {
            // Only the peer's: another copy may carry the same ID (say, one of mine on
            // another server).
            let msg = (chat.messages.iter()).find(|msg| msg.id == id && msg.person == peer)?;
            let choices = dm_choices(servers, peer, &msg.replicas);
            let &(index, channel) = choices.first()?;
            let mut draft = in_channel(index, channel, chat, (id, peer), true)?;
            draft.choices = choices;
            Some(draft)
        }
    }
}

/// The servers a report of the peer's DM copy held on `replicas` can go through: online,
/// with the DM. Those that know reports first, then those the peer doesn't own, so the
/// sender isn't picked to review their own message.
fn dm_choices(servers: &[Server], peer: Id, replicas: &[url::Url]) -> Vec<(usize, Id)> {
    let mut choices: Vec<(usize, Id)> = (replicas.iter())
        .filter_map(|held| {
            let index = servers.iter().position(|server| server.address == *held)?;
            let server = &servers[index];
            let dm = server.dir.dms.iter().find(|dm| dm.peer == peer)?;
            (server.status == Status::Online).then_some((index, dm.channel))
        })
        .collect();
    choices.sort_by_key(|(index, _)| {
        let dir = &servers[*index].dir;
        let knows =
            (dir.roles.iter()).any(|role| role.permissions.iter().any(|p| p == "review_reports"));
        let owned = (dir.members.iter())
            .any(|member| member.person == peer && member.roles.iter().any(|r| r == "owner"));
        (!knows, owned)
    });
    choices
}

/// A report of `person`'s message `id` in channel `channel` of server `index`.
pub fn in_channel(
    index: usize,
    channel: Id,
    chat: &Chat,
    (id, person): (Id, Id),
    dm: bool,
) -> Option<Draft> {
    let msg = (chat.messages.iter()).find(|msg| msg.id == id && msg.person == person)?;
    let edit = (chat.edited.get(&(msg.person, msg.id))).map(|(_, _, edit)| *edit);
    Some(Draft {
        server: index,
        channel,
        message: id,
        edit,
        dm,
        sender: msg.person,
        reason: None,
        note: String::new(),
        recipients: None,
        choices: Vec::new(),
    })
}

/// Start a report: ask who it would go to, to show in the dialog.
pub fn open(servers: &[Server], draft: Draft) -> Draft {
    servers[draft.server].send(Command::ReportRecipients {
        channel: draft.channel,
        message: draft.message,
        dm: draft.dm,
        sender: draft.sender,
    });
    draft
}

pub fn reason_label(reason: Reason) -> &'static str {
    match reason {
        Reason::Spam => t!("reports.reason_spam"),
        Reason::Harassment => t!("reports.reason_harassment"),
        Reason::Illegal => t!("reports.reason_illegal"),
        Reason::ChildSafety => t!("reports.reason_child_safety"),
        Reason::Other => t!("reports.reason_other"),
    }
}

/// The dialog's content. Returns the command to send once Report is pressed.
pub fn dialog(
    ui: &mut Ui,
    servers: &[Server],
    draft: &mut Draft,
    close: &mut bool,
) -> Option<Command> {
    // A DM on several servers: the reporter picks whose owner gets it, and the
    // recipients are looked up again there.
    if draft.choices.len() > 1 {
        ui.label(ui::bold(t!("reports.dm_pick")));
        let mut picked = draft.server;
        for (index, _) in &draft.choices {
            ui.radio_value(&mut picked, *index, servers[*index].name());
        }
        if picked != draft.server
            && let Some(&(index, channel)) = draft.choices.iter().find(|(i, _)| *i == picked)
        {
            (draft.server, draft.channel, draft.recipients) = (index, channel, None);
            return Some(Command::ReportRecipients {
                channel,
                message: draft.message,
                dm: true,
                sender: draft.sender,
            });
        }
        ui.add_space(ui::SPACE_S);
    }
    let server = &servers[draft.server];
    ui.label(match draft.dm {
        true => t!("reports.help_dm"),
        false => t!("reports.help"),
    });
    ui.add_space(ui::SPACE_S);
    ui.label(ui::bold(t!("reports.why")));
    // Radio buttons: one reason, and screen readers say so.
    for reason in Reason::ALL {
        ui.radio_value(&mut draft.reason, Some(reason), reason_label(reason));
    }
    ui.add_space(ui::SPACE_S);
    ui.label(ui::bold(t!("reports.note")));
    ui.add(
        ui::text_area(&mut draft.note)
            .char_limit(MAX_NOTE_CHARS)
            .hint_text(t!("reports.note_hint"))
            .desired_rows(2)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(ui::SPACE_S);
    // Every recipient by name, hidden members too: nobody reads a report unseen.
    let ready = match &draft.recipients {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(ui::caption(t!("reports.recipients_loading")));
            });
            None
        }
        Some(Err(problem)) => {
            error_text(ui, Some(problem));
            None
        }
        Some(Ok(recipients)) => {
            // With their whole fingerprint, as names needn't be unique and a short one
            // could be matched by a made-up key.
            let named =
                |person: &Id| format!("{} ({})", server.member_name(person), fingerprint(person));
            let names: Vec<String> = recipients.people.iter().map(named).collect();
            let names = names.join(", ");
            ui.label(t!("reports.recipients", names = names));
            // Which server's owner, when the DM is on several.
            if draft.dm {
                ui.label(ui::caption(t!("reports.dm_server", server = server.name())));
            }
            if let Some(before) = &recipients.previous_owner {
                let text = t!("reports.owner_changed", before = named(before));
                ui.label(RichText::new(text).color(ui::colors(ui).warning));
            }
            Some((recipients.people.clone(), recipients.owner))
        }
    };
    ui.label(ui::caption(t!("reports.privacy")));
    let mut send = false;
    ui::footer(ui, |ui| {
        send = ui
            .add_enabled_ui(draft.reason.is_some() && ready.is_some(), |ui| {
                ui::danger(ui, t!("reports.send"))
            })
            .inner
            .clicked();
        *close |= ui::secondary(ui, t!("common.cancel")).clicked();
    });
    let reason = draft.reason.filter(|_| send)?;
    let (recipients, owner) = ready?;
    Some(Command::Report(Box::new(Outgoing {
        channel: draft.channel,
        message: draft.message,
        edit: draft.edit,
        dm: draft.dm,
        reason,
        note: draft.note.trim().to_owned(),
        recipients,
        owner,
    })))
}
