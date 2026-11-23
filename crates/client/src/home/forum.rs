//! Forum channels: the channel's messages grouped into posts, each with its replies
//! folded away under it. A post is a message without a reply reference; its first line
//! is the title. A post with a poll is sent as the poll first and then the post's text,
//! pointing at the poll, since a poll carries nothing but its question. A reply points
//! at the post or at another reply in its thread. All of these references are inside
//! the encrypted message, so the server never learns which messages belong together.

use std::collections::{HashMap, HashSet};

use eframe::egui::{self, CursorIcon, Id as UiId, ScrollArea, Sense, Ui, UiBuilder};
use rusttalk_core::{client::MAX_TEXT_CHARS, messages::PollPart};

use super::{
    Dialog, Embeds, Home, Pick, To, View, add_picture, allow_previews, composer, composer_id,
    empty, follow_invite, markdown, mentions, message, pane_title, picker, poll_draft, polls,
    preview_status, queue_preview, time, toast_line,
};
use crate::{
    files::{self, Ask},
    net::{Command, Draft},
    server::{Body, Chat, Directory, Id, Msg, Status},
    ui,
};

mod editor;
mod sort;

use sort::{Order, Period};

/// Reserved encrypted quote hint for text explicitly wrapping a poll.
const POLL_WRAPPER: Id = [0; 32];

/// Longest post title, in characters.
const TITLE_CHARS: usize = 100;
/// The column with a post's vote buttons, at its right.
const VOTES_WIDTH: f32 = 40.0;
/// Replies followed back to their post before giving up on a broken chain.
const MAX_DEPTH: usize = 256;

/// A post with the poll it carries, its reply count, the time of its newest message
/// and its up and down votes.
struct Post {
    msg: Msg,
    poll: Option<Msg>,
    replies: usize,
    last: i64,
    up: usize,
    down: usize,
}

/// What is being typed for a new post, or an edit of one.
#[derive(Clone, Default)]
pub(crate) struct NewPost {
    open: bool,
    /// The post being edited.
    editing: Option<Id>,
    title: String,
    body: String,
    /// Showing the post as it will look instead of the Markdown.
    preview: bool,
    /// The poll that goes with the post, at most one.
    poll: Option<polls::Editor>,
}

/// The editor owns both IDs until the server confirms each half. Keep a separate
/// snapshot so a late failure cannot overwrite the next post being written.
pub(crate) struct Submission {
    form: NewPost,
    text: Draft,
    poll: Option<Draft>,
    failed: bool,
}

impl Submission {
    fn prepare(form: &NewPost, written: Written, channel: Id, chat: &Chat, me: Id) -> Option<Self> {
        let (text, poll, edit) = written;
        let mut poll = match poll {
            Some((question, part)) => Some(poll_draft(channel, question, part)?),
            None => None,
        };
        let mut text = text_draft(channel, text, poll.as_ref().map(|p| (POLL_WRAPPER, p.id)))?;
        text.forum_post = true;
        if let Some(poll) = &mut poll {
            poll.forum_post = true;
        }
        text.edit_of = edit;
        if let Some(id) = edit {
            text.revision = chat.next_revision(me, id, polls::now_ms());
        }
        Some(Self {
            form: form.clone(),
            text,
            poll,
            failed: false,
        })
    }

    fn next(&self) -> &Draft {
        self.poll.as_ref().unwrap_or(&self.text)
    }
}

/// Called after link preview consent, even when the user has switched channels.
pub(super) fn dispatch(
    chat: &mut Chat,
    commands: &tokio::sync::mpsc::UnboundedSender<Command>,
    draft: Draft,
) {
    if !draft.forum_post {
        crate::server::Server::send_to(commands, Command::Send(draft));
        return;
    }
    // Access may have been lost while the consent prompt was open.
    if let Some(post) = chat.submissions.iter_mut().find(|p| p.text.id == draft.id) {
        post.text = draft;
        let next = post.next().clone();
        send_part(chat, commands, next);
    }
}

pub(super) fn send_part(
    chat: &mut Chat,
    commands: &tokio::sync::mpsc::UnboundedSender<Command>,
    draft: Draft,
) {
    let id = draft.id;
    if !crate::server::Server::send_to(commands, Command::Send(draft)) {
        failed(chat, id);
    }
}

pub(super) fn failed(chat: &mut Chat, id: Id) {
    if let Some(post) = chat.submissions.iter_mut().find(|p| p.next().id == id) {
        post.failed = true;
    }
}

pub(super) fn confirmed(chat: &mut Chat, id: Id) -> Option<Draft> {
    let index = chat.submissions.iter().position(|p| p.next().id == id)?;
    let post = &mut chat.submissions[index];
    if post.poll.take().is_some() {
        post.failed = false;
        return Some(post.text.clone());
    }
    chat.submissions.remove(index);
    None
}

/// Which posts show their replies, and the one being answered.
#[derive(Clone, Default)]
struct Folds {
    query: String,
    open: HashSet<Id>,
    answering: Option<Id>,
}

/// Markdown as a message by `person` shows it, with mentions and emoji.
fn show_markdown(ui: &mut Ui, dir: &Directory, channel: Id, person: Id, text: String) {
    let msg = Msg {
        backfilled: false,
        id: [0; 32],
        quoted_author: None,
        person,
        at: 0,
        reply_to: None,
        edit_of: None,
        body: Body::Text(text),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    };
    let mention = mentions::Message::cached(ui, &msg, dir, channel);
    mention.doc.show(ui, dir, &mention, false, false);
}

/// A post's title: its first line, without heading marks.
fn title(text: &str) -> &str {
    text.lines()
        .next()
        .unwrap_or("")
        .trim_start_matches('#')
        .trim()
}

/// A post's text after the title.
fn body(text: &str) -> String {
    let rest = text.lines().skip(1).collect::<Vec<_>>().join("\n");
    rest.trim().to_owned()
}

/// The start of a post's text after the title, a few lines of Markdown.
fn preview(text: &str) -> String {
    let rest = text.lines().skip(1).take(4).collect::<Vec<_>>().join("\n");
    let more = text.lines().nth(5).is_some();
    let rest = rest.trim();
    match rest.char_indices().nth(300) {
        Some((at, _)) => format!("{}…", &rest[..at]),
        None if more => format!("{rest}\n…"),
        None => rest.to_owned(),
    }
}

/// The text a new post is sent as: the title as a heading, then the body.
fn post_text(title: &str, body: &str) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("# {title}\n{}", body.trim()).trim().to_owned()
}

fn is_poll(msg: &Msg) -> bool {
    msg.reply_to.is_none() && matches!(msg.body, Body::Poll(..))
}

fn marked(msg: &Msg) -> bool {
    matches!(msg.body, Body::Text(_))
        && msg.sticker.is_none()
        && msg.reply_to.is_some()
        && msg.quoted_author == Some(POLL_WRAPPER)
}

/// Both halves of a verified post resolve to the same root. Ambiguous references
/// leave the poll independent, including when deleting a wrapper.
struct Threads<'a> {
    by_id: HashMap<Id, &'a Msg>,
    roots: HashMap<Id, Id>,
}

impl<'a> Threads<'a> {
    fn new(messages: &'a [Msg]) -> Self {
        let by_id: HashMap<_, _> = messages.iter().map(|m| (m.id, m)).collect();
        let mut roots = HashMap::new();
        let mut owners = HashMap::new();
        for msg in messages {
            if marked(msg) {
                roots.insert(msg.id, msg.id);
                if let Some(poll) = msg.reply_to.and_then(|id| by_id.get(&id))
                    && is_poll(poll)
                    && poll.person == msg.person
                {
                    owners
                        .entry(poll.id)
                        .and_modify(|owner| *owner = None)
                        .or_insert(Some(msg.id));
                }
            } else if is_poll(msg)
                || (matches!(msg.body, Body::Text(_))
                    && msg.sticker.is_none()
                    && msg.reply_to.is_none())
            {
                roots.insert(msg.id, msg.id);
            }
        }
        for (poll, owner) in owners {
            if let Some(owner) = owner {
                roots.insert(poll, owner);
            }
        }
        Self { by_id, roots }
    }

    fn poll(&self, msg: &Msg) -> Option<&'a Msg> {
        let id = msg.reply_to?;
        (self.roots.get(&id) == Some(&msg.id) && id != msg.id).then(|| self.by_id[&id])
    }

    /// Canonical post and the direct reply that starts this branch.
    fn branch(&self, msg: &Msg) -> Option<(Id, Id)> {
        let mut at = msg;
        for _ in 0..MAX_DEPTH {
            if self.roots.contains_key(&at.id) {
                return None;
            }
            let parent = at.reply_to?;
            if let Some(root) = self.roots.get(&parent) {
                return Some((*root, at.id));
            }
            at = self.by_id.get(&parent)?;
        }
        None
    }
}

pub(super) fn carried_poll(chat: &Chat, msg: &Msg) -> Option<Id> {
    Threads::new(&chat.messages).poll(msg).map(|poll| poll.id)
}

fn posts(messages: &[Msg]) -> Vec<Post> {
    let threads = Threads::new(messages);
    let mut posts: HashMap<_, _> = messages
        .iter()
        .filter(|msg| threads.roots.get(&msg.id) == Some(&msg.id))
        .map(|msg| {
            (
                msg.id,
                Post {
                    msg: msg.clone(),
                    poll: threads.poll(msg).cloned(),
                    replies: 0,
                    last: msg.at,
                    up: 0,
                    down: 0,
                },
            )
        })
        .collect();
    for msg in messages {
        if let Some((id, _)) = threads.branch(msg)
            && let Some(post) = posts.get_mut(&id)
        {
            post.replies += 1;
            post.last = post.last.max(msg.at);
        }
    }
    let mut posts: Vec<_> = messages
        .iter()
        .filter_map(|msg| posts.remove(&msg.id))
        .collect();
    posts.sort_by_key(|post| std::cmp::Reverse(post.last));
    posts
}

/// Match decrypted post titles, bodies and poll questions/options on this device.
fn matches_search(post: &Post, query: &str) -> bool {
    std::iter::once(&post.msg)
        .chain(&post.poll)
        .any(|msg| match &msg.body {
            Body::Text(text) => text.to_lowercase().contains(query),
            Body::Poll(question, poll) => {
                question.to_lowercase().contains(query)
                    || poll
                        .options
                        .iter()
                        .any(|option| option.to_lowercase().contains(query))
            }
            _ => false,
        })
}

/// A post's replies: each reply to the post, oldest first, followed by every answer
/// further down its chain, which are returned as nested (one step in). A reply to the
/// post or to the reply it sits under drops its quote, the layout shows it.
fn thread(messages: &[Msg], post: Id) -> (Vec<Msg>, Vec<Id>) {
    let threads = Threads::new(messages);
    let (mut shown, mut nested) = (Vec::new(), Vec::new());
    let mut branches: HashMap<Id, Vec<&Msg>> = HashMap::new();
    let mut tops = Vec::new();
    for msg in messages {
        if let Some((root, top)) = threads.branch(msg)
            && root == post
        {
            if top == msg.id {
                tops.push(top);
            }
            branches.entry(top).or_default().push(msg);
        }
    }
    for top in tops {
        for &msg in branches.get(&top).into_iter().flatten() {
            let mut msg = msg.clone();
            if msg.id != top {
                nested.push(msg.id);
            }
            if msg.id == top || msg.reply_to == Some(top) {
                msg.reply_to = None;
            }
            shown.push(msg);
        }
    }
    (shown, nested)
}

fn text_draft(channel: Id, text: String, reply_to: Option<(Id, Id)>) -> Option<Draft> {
    let Ok(id) = rusttalk_core::keys::random_key() else {
        tracing::warn!("no randomness for a post ID");
        return None;
    };
    Some(Draft {
        forum_post: false,
        queued: Default::default(),
        id: *id,
        channel,
        peer: None,
        text,
        reply_to,
        edit_of: None,
        revision: 0,
        files: Vec::new(),
        keep: Vec::new(),
        sticker: None,
        friend: None,
        poke: false,
        preview: None,
        poll: None,
        inline: Vec::new(),
    })
}

/// What the post form sends: the text, a poll to go with a new post, and the post an
/// edit is for.
type Written = (String, Option<(String, PollPart)>, Option<Id>);

/// The form for a new post or an edit of one. Returns what to send when Post is clicked.
fn new_post(
    ui: &mut Ui,
    new: &mut NewPost,
    dir: &Directory,
    channel: Id,
    online: bool,
) -> Option<Written> {
    let mut send = None;
    ui::card(ui, |ui| {
        if new.editing.is_some() {
            ui.label(ui::bold(format!("✏ {}", t!("forum.edit_post"))));
        }
        ui.add(
            ui::field(&mut new.title)
                .hint_text(t!("forum.title_hint"))
                .char_limit(TITLE_CHARS)
                .desired_width(f32::INFINITY),
        );
        let body_id = ui.make_persistent_id(("forum-body", channel));
        if editor::toolbar(ui, body_id, &mut new.body, new.preview) {
            new.preview = !new.preview;
        }
        let text = post_text(&new.title, &new.body);
        if new.preview {
            ui::card(ui, |ui| {
                ui.set_width(ui.available_width());
                show_markdown(ui, dir, channel, dir.me, text.clone());
            });
        } else {
            ui.add(
                ui::text_area(&mut new.body)
                    .id(body_id)
                    .hint_text(t!("forum.body_hint"))
                    .desired_rows(8)
                    .desired_width(f32::INFINITY),
            );
        }
        if let Some(editor) = &mut new.poll
            && let Some(polls::Action::Cancel) = polls::editor(ui, editor, true)
        {
            new.poll = None;
        }
        if !ui.is_enabled() {
            return;
        }
        let count = text.chars().count();
        ui.add_space(ui::SPACE_XS);
        ui.horizontal(|ui| {
            let poll_ok = new.poll.as_ref().is_none_or(polls::Editor::valid);
            let ready =
                online && !new.title.trim().is_empty() && count <= MAX_TEXT_CHARS && poll_ok;
            let label = match new.editing {
                Some(_) => t!("common.save"),
                None => t!("forum.post"),
            };
            let post = ui.add_enabled_ui(ready, |ui| ui::primary(ui, label));
            if post.inner.clicked() {
                let poll = new.poll.as_ref().map(|editor| editor.poll(polls::now()));
                send = Some((text.clone(), poll, new.editing));
            }
            if ui::secondary(ui, t!("common.cancel")).clicked() {
                *new = NewPost::default();
            }
            // A poll can't change once sent, so an edit has none to add.
            if new.editing.is_some() {
                ui.weak(format!("{count} / {MAX_TEXT_CHARS}"));
                return;
            }
            let plus = ui::icon_button(
                ui,
                "+",
                t!("forum.add"),
                ui::Look::Plain,
                egui::Vec2::splat(ui::CONTROL_HEIGHT),
            );
            ui::menu_wide(&plus, |m| {
                let item = ui::Item::new("📊", t!("forum.add_poll"))
                    .hint(t!("forum.add_poll_help"))
                    .enabled(new.poll.is_none());
                if m.add(item).clicked() {
                    new.poll = Some(polls::Editor::default());
                }
            });
            ui.weak(format!("{count} / {MAX_TEXT_CHARS}"));
        });
    });
    send
}

/// What a click in the list asks for.
enum Asked {
    Pick(Id, Pick),
    /// Choices on a poll, or on a post: 0 up, 1 down, none to take a vote back.
    Vote(Id, Vec<u8>),
    Toggle(Id),
    Answer(Id),
    Edit(Id, String),
    Delete(Id),
}

/// Up and down vote buttons with the score between them.
fn votes(ui: &mut Ui, post: &Post, mine: &[u8]) -> Option<Vec<u8>> {
    let size = egui::Vec2::splat(ui::CONTROL_HEIGHT - 4.0);
    let accent = ui::accent(ui).base;
    let mut picked = None;
    ui.vertical_centered(|ui| {
        ui.set_width(VOTES_WIDTH);
        ui.spacing_mut().item_spacing.y = 0.0;
        let look = |on: bool, color| {
            if on {
                ui::Look::On(color)
            } else {
                ui::Look::Plain
            }
        };
        let up = look(mine == [0], accent);
        if ui::icon_button(ui, "↑", t!("forum.up"), up, size).clicked() {
            picked = Some(polls::toggle(mine, 0, false));
        }
        let score = post.up as i64 - post.down as i64;
        let tip = t!("forum.votes", up = post.up, down = post.down);
        ui.label(ui::bold(score.to_string())).on_hover_text(tip);
        let down = look(mine == [1], ui::colors(ui).danger);
        if ui::icon_button(ui, "↓", t!("forum.down"), down, size).clicked() {
            picked = Some(polls::toggle(mine, 1, false));
        }
    });
    picked
}

pub(super) fn view(ui: &mut Ui, home: &mut Home, index: usize, channel: Id, chat: &mut Chat) {
    let in_server = matches!(home.view, View::Server(_));
    let server = &home.servers[index];
    let name = (server.channel(&channel)).map_or_else(String::new, |c| c.name.clone());
    let title_text = format!("💬 {name}");
    let can_post = server.can_write(&channel);
    let online = server.status == Status::Online;
    let to = To::Channel(server.address.clone(), channel);
    let now = ui.input(|input| input.time);
    pane_title(
        ui,
        "chat-title",
        &title_text,
        in_server.then_some(&mut home.layout),
        Some(channel),
    );
    toast_line(ui, &mut home.toast);
    if let Some(out) = (home.sending.iter_mut()).find(|o| o.to == to && o.preview.is_some()) {
        preview_status(ui, &home.voice, out);
    }
    let folds_id = UiId::unique(("forum-folds", server.dir.me, &server.address, channel));
    let mut new = std::mem::take(&mut chat.new_post);
    let mut folds: Folds = ui.data(|d| d.get_temp(folds_id)).unwrap_or_default();
    let order_id = UiId::unique(("forum-order", channel));
    let (mut order, mut period): (Order, Period) =
        ui.data(|d| d.get_temp(order_id)).unwrap_or_default();
    ui.add_space(ui::SPACE_S);
    ui.horizontal(|ui| {
        if can_post && !new.open {
            new.open = ui::primary(ui, &format!("✏ {}", t!("forum.new_post"))).clicked();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if order.timed() {
                let button = ui::secondary(ui, period.label());
                ui::menu(&button, |m| {
                    for choice in Period::ALL {
                        if m.item("", choice.label()).clicked() {
                            period = choice;
                        }
                    }
                });
            }
            let label = format!("{} {}", order.icon(), order.label());
            let button = ui::secondary(ui, &label).on_hover_text(t!("forum.sort"));
            ui::menu_wide(&button, |m| {
                for choice in Order::ALL {
                    let item = ui::Item::new(choice.icon(), choice.label()).hint(choice.hint());
                    if m.add(item).clicked() {
                        order = choice;
                    }
                }
            });
        });
    });
    let mut sent = None;
    let mut retry = None;
    let mut discard = None;
    ui.add_space(ui::SPACE_S);

    // A dropped file goes with the reply being written.
    let dropped: Vec<_> = ui.input(|input| {
        (input.raw.dropped_files.iter())
            .map(|file| file.path().to_owned())
            .collect()
    });
    if online
        && folds.answering.is_some()
        && !dropped.is_empty()
        && let Some(problem) = files::stage(chat, dropped)
    {
        home.toast = Some((problem, now + 6.0));
    }
    let settings = home.voice.audio.settings();
    let mut asked = None;
    let mut live = None;
    ui.add(
        ui::field(&mut folds.query)
            .id(folds_id.with("search"))
            .hint_text(t!("forum.search")),
    );
    let query = folds.query.trim().to_lowercase();
    let can_page = online && !chat.loading && !chat.at_start && chat.oldest.is_some();
    let mut older = can_page && !query.is_empty();
    let mut reply = (None, None);
    let mut posts = posts(&chat.messages);
    for post in posts
        .iter_mut()
        .filter(|post| matches!(post.msg.body, Body::Text(_)))
    {
        let tally = chat.post_votes(&post.msg);
        (post.up, post.down) = (tally.voters[0].len(), tally.voters[1].len());
    }
    if !query.is_empty() {
        posts.retain(|post| matches_search(post, &query));
    }
    sort::sort(&mut posts, order, period, polls::now());
    if let Some(post) = folds.answering
        && !posts
            .iter()
            .any(|p| p.msg.id == post && folds.open.contains(&post))
    {
        folds.answering = None;
    }
    let scope = picker::Scope {
        local: Some(&home.servers[index].dir),
        others: (home.servers.iter().enumerate())
            .filter(|(at, server)| *at != index && !server.dir.emoji.is_empty())
            .map(|(_, server)| (server.name(), &server.dir))
            .collect(),
    };
    let embeds = Embeds {
        servers: &home.servers,
        previews: &home.previews,
        allowed: &settings.link_previews,
    };
    let server = &home.servers[index];
    let (dir, me) = (&server.dir, server.dir.me);
    ScrollArea::vertical()
        .id_salt(("forum", channel))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // The form scrolls with the posts, so its buttons stay reachable.
            for (i, submission) in chat
                .submissions
                .iter_mut()
                .enumerate()
                .filter(|(_, p)| p.failed)
            {
                ui.push_id(submission.text.id, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled_ui(can_post && online, |ui| {
                                ui::primary(ui, t!("forum.retry"))
                            })
                            .inner
                            .clicked()
                        {
                            submission.failed = false;
                            retry = Some(submission.next().clone());
                        }
                        if ui::secondary(ui, t!("common.cancel")).clicked() {
                            discard = Some(i);
                        }
                    });
                    // A retry uses the same payload and IDs, including when an ACK was lost.
                    ui.add_enabled_ui(false, |ui| {
                        new_post(ui, &mut submission.form, dir, channel, false)
                    });
                });
            }
            if can_post && new.open {
                sent = new_post(ui, &mut new, dir, channel, online);
                ui.add_space(ui::SPACE_S);
            }
            if !query.is_empty() && online && !chat.at_start {
                ui.label(t!("forum.searching"));
            }
            if posts.is_empty() {
                if chat.loading {
                    ui.vertical_centered(|ui| ui.spinner());
                } else {
                    empty(
                        ui,
                        &title_text,
                        if query.is_empty() {
                            t!("forum.empty")
                        } else {
                            t!("forum.no_matches")
                        },
                    );
                }
            }
            for post in &posts {
                let id = post.msg.id;
                let open = folds.open.contains(&id);
                let line = t!(
                    "forum.post_line",
                    count = post.replies,
                    name = server.member_name(&post.msg.person),
                    time = time(post.last)
                );
                let names = |person: &Id| server.member_name(person);
                let card = ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
                    ui::card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // A poll on its own takes votes on its options instead.
                        let votable = matches!(post.msg.body, Body::Text(_));
                        let column = if votable { VOTES_WIDTH } else { 0.0 };
                        ui.horizontal_top(|ui| {
                            let content = ui.available_width() - column;
                            ui.vertical(|ui| {
                                ui.set_width(content);
                                if let Body::Text(text) = &post.msg.body {
                                    ui.label(ui::bold(title(text)).size(ui::TEXT_TITLE));
                                    let shown = if open { body(text) } else { preview(text) };
                                    if !shown.is_empty() {
                                        show_markdown(ui, dir, channel, post.msg.person, shown);
                                    }
                                    let links = markdown::cached(ui, text)
                                        .links()
                                        .map(String::from)
                                        .collect::<Vec<_>>();
                                    files::attachments(
                                        ui,
                                        &mut home.files,
                                        (&server.address, &server.commands),
                                        &post.msg.files,
                                        &links,
                                    );
                                }
                                if let Some(action) = crate::call::chat_live(
                                    ui,
                                    &home.voice.live_files,
                                    dir,
                                    &post.msg,
                                ) {
                                    live = Some(action);
                                }
                                let poll = post.poll.as_ref().unwrap_or(&post.msg);
                                if let Body::Poll(question, options) = &poll.body {
                                    let tally = chat.tally(poll).unwrap_or_default();
                                    if let Some(choices) =
                                        polls::card(ui, question, options, &tally, &me, names, None)
                                    {
                                        asked = Some(Asked::Vote(poll.id, choices));
                                    }
                                }
                                ui.add_space(ui::SPACE_XS);
                                ui.horizontal(|ui| {
                                    let fold = if open {
                                        t!("forum.hide_replies").to_owned()
                                    } else {
                                        line.clone()
                                    };
                                    if ui::secondary(ui, &fold).clicked() {
                                        asked = Some(Asked::Toggle(id));
                                    }
                                    if open {
                                        ui.label(ui::caption(&line));
                                    }
                                });
                            });
                            if votable {
                                let tally = chat.post_votes(&post.msg);
                                let mine: Vec<u8> = (tally.voters.iter().enumerate())
                                    .filter(|(_, voters)| voters.contains(&me))
                                    .map(|(at, _)| at as u8)
                                    .collect();
                                let picked = ui.add_enabled_ui(can_post && online, |ui| {
                                    votes(ui, post, &mine)
                                });
                                if let Some(choices) = picked.inner {
                                    asked = Some(Asked::Vote(id, choices));
                                }
                            }
                        });
                        if !open {
                            return;
                        }
                        let (replies, nested) = thread(&chat.messages, id);
                        let mut last: Option<&Msg> = None;
                        for msg in replies.iter() {
                            if matches!(msg.body, Body::PollPart(_)) {
                                continue;
                            }
                            let start = !last.is_some_and(|prev| {
                                prev.person == msg.person
                                    && msg.at - prev.at < 300
                                    && msg.reply_to.is_none()
                                    && nested.contains(&prev.id) == nested.contains(&msg.id)
                            });
                            let mut show = |ui: &mut Ui| {
                                message(ui, chat, msg, start, dir, &embeds, channel, |ui| {
                                    let links: Vec<String> = match &msg.body {
                                        Body::Text(text) => markdown::cached(ui, text)
                                            .links()
                                            .map(String::from)
                                            .collect(),
                                        _ => Vec::new(),
                                    };
                                    let at = (&server.address, &server.commands);
                                    files::attachments(ui, &mut home.files, at, &msg.files, &links);
                                    if let Some(action) =
                                        crate::call::chat_live(ui, &home.voice.live_files, dir, msg)
                                    {
                                        live = Some(action);
                                    }
                                })
                            };
                            let picked = if nested.contains(&msg.id) {
                                ui.indent(("nested", msg.id), show).inner
                            } else {
                                show(ui)
                            };
                            if let Some(pick) = picked {
                                asked = Some(Asked::Pick(id, pick));
                            }
                            last = Some(msg);
                        }
                        if folds.answering == Some(id) {
                            if chat.editing.is_none() && chat.reply_to.is_none() {
                                chat.reply_to = Some((post.msg.person, id));
                            }
                            reply = composer(
                                ui,
                                chat,
                                dir,
                                online,
                                channel,
                                &name,
                                (&mut home.emoji_picker, &scope),
                                (&mut home.toast, now),
                            );
                        } else if can_post
                            && ui::secondary(ui, &format!("↩ {}", t!("forum.answer"))).clicked()
                        {
                            asked = Some(Asked::Answer(id));
                        }
                    });
                });
                let mut card = card.response;
                if !open {
                    card = card.on_hover_cursor(CursorIcon::PointingHand);
                    if card.clicked() {
                        asked = Some(Asked::Toggle(id));
                    }
                }
                let mine = post.msg.person == me;
                let deletable = mine || dir.perms.delete_messages;
                ui::context_menu(&card, |m| {
                    if let Body::Text(text) = &post.msg.body
                        && mine
                        && m.item("✏", t!("forum.edit_post")).clicked()
                    {
                        asked = Some(Asked::Edit(id, text.clone()));
                    }
                    if deletable && m.danger("🗑", t!("common.delete")).clicked() {
                        asked = Some(Asked::Delete(id));
                    }
                });
                ui.add_space(ui::SPACE_XS);
            }
            if can_page && query.is_empty() {
                older = ui::secondary(ui, t!("forum.older")).clicked();
            }
        });
    drop((embeds, scope));
    let server = &home.servers[index];
    if let Some(action) = live {
        super::handle_live_file(
            ui,
            &mut home.voice,
            &mut home.files,
            chat,
            Some(server),
            action,
        );
    }
    let address = server.address.clone();
    let mut outgoing = Vec::new();
    if older && let Some(oldest) = chat.oldest {
        chat.loading = true;
        server.send(Command::Older(channel, oldest));
    }
    if let Some(i) = discard {
        chat.submissions.remove(i);
    }
    if let Some(draft) = retry {
        send_part(chat, &server.commands, draft);
    }
    if let Some(written) = sent
        && let Some(post) = Submission::prepare(&new, written, channel, chat, me)
    {
        outgoing.push(post.text.clone());
        chat.submissions.push(post);
        new = NewPost::default();
    }
    match reply {
        (Some(Command::Send(draft)), _) => outgoing.push(draft),
        (Some(command), _) => server.send(command),
        (None, Some(false)) => home
            .files
            .ask(ui.ctx(), Ask::Attach(address.clone(), channel)),
        (None, Some(true)) => home
            .files
            .ask(ui.ctx(), Ask::Live(address.clone(), channel)),
        (None, None) => {}
    }
    // Ctrl+V in the reply box may attach a picture or copied files (see paste.rs).
    if online
        && ui.memory(|memory| memory.has_focus(composer_id(channel)))
        && crate::paste::wanted(ui.ctx())
    {
        home.files
            .paste(ui.ctx(), Ask::Attach(address.clone(), channel));
    }
    let person = |id: Id| (chat.messages.iter().find(|m| m.id == id)).map(|m| m.person);
    match asked {
        Some(Asked::Toggle(post)) if folds.open.remove(&post) => {
            if folds.answering == Some(post) {
                folds.answering = None;
                chat.reply_to = None;
            }
        }
        Some(Asked::Toggle(post) | Asked::Answer(post)) => {
            folds.open.insert(post);
            if can_post && chat.editing.is_none() {
                folds.answering = Some(post);
                chat.reply_to = person(post).map(|person| (person, post));
            }
        }
        Some(Asked::Pick(post, Pick::Reply(person, id))) => {
            folds.answering = Some(post);
            chat.reply_to = Some((person, id));
            if chat.editing.take().is_some() {
                chat.clear_draft();
            }
            ui.memory_mut(|memory| memory.request_focus(composer_id(channel)));
        }
        Some(Asked::Pick(post, Pick::Edit(id, text))) => {
            folds.answering = Some(post);
            chat.editing = Some(id);
            chat.reply_to = None;
            chat.open_draft(&home.servers[index].dir, &text);
            ui.memory_mut(|memory| memory.request_focus(composer_id(channel)));
        }
        Some(Asked::Edit(post, text)) => {
            new = NewPost {
                open: true,
                editing: Some(post),
                title: title(&text).to_owned(),
                body: body(&text),
                ..NewPost::default()
            };
        }
        Some(Asked::Pick(_, Pick::Delete(id)) | Asked::Delete(id)) => {
            home.dialog = Some(Dialog::Delete(index, channel, id));
        }
        Some(Asked::Pick(_, Pick::Poll(part))) => {
            outgoing.extend(poll_draft(channel, String::new(), part))
        }
        Some(Asked::Vote(poll, choices)) => {
            let sent = polls::now_ms();
            let part = PollPart::Vote {
                poll,
                sent,
                choices,
            };
            outgoing.extend(poll_draft(channel, String::new(), part));
        }
        Some(Asked::Pick(_, Pick::AddPicture(name, kind, bytes))) => {
            add_picture(&home.servers[index], name, kind, &bytes);
        }
        Some(Asked::Pick(_, Pick::Previews(id, allow))) => {
            allow_previews(&home.voice, &mut home.previews, id, allow);
        }
        Some(Asked::Pick(_, Pick::Invite(text))) => {
            let joining = (&mut home.join, &mut home.invite, &home.net);
            follow_invite(&home.servers, &mut home.view, joining, text);
        }
        None => {}
    }
    // A link gets its preview card, as in the chat; whatever follows waits behind it.
    for draft in outgoing {
        if let Some(draft) = queue_preview(ui, &home.voice, &mut home.sending, &to, draft) {
            dispatch(chat, &home.servers[index].commands, draft);
        }
    }
    chat.new_post = new;
    ui.data_mut(|d| {
        d.insert_temp(folds_id, folds);
        d.insert_temp(order_id, (order, period));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u8, reply_to: Option<u8>, at: i64, text: &str) -> Msg {
        Msg {
            backfilled: false,
            id: [id; 32],
            quoted_author: None,
            person: [1; 32],
            at,
            reply_to: reply_to.map(|r| [r; 32]),
            edit_of: None,
            body: Body::Text(text.into()),
            files: Vec::new(),
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        }
    }

    fn poll(id: u8, at: i64) -> Msg {
        let poll = rusttalk_core::messages::Poll {
            options: vec!["A".into(), "B".into()],
            multiple: false,
            ends_at: 0,
        };
        Msg {
            body: Body::Poll("Q".into(), poll),
            ..msg(id, None, at, "")
        }
    }

    #[test]
    fn groups_replies_under_their_post_newest_activity_first() {
        let messages = [
            msg(1, None, 10, "# Saturday raid\nWho is in?"),
            msg(2, None, 20, "Rules"),
            msg(3, Some(1), 30, "Me"),
            // A reply to a reply still belongs to the first post.
            msg(4, Some(3), 40, "Me too"),
            // Its post isn't loaded: it shows nowhere.
            msg(5, Some(9), 50, "Lost"),
        ];
        let listed: Vec<_> = posts(&messages)
            .iter()
            .map(|p| (p.msg.id[0], p.replies, p.last))
            .collect();
        assert_eq!(listed, [(1, 2, 40), (2, 0, 20)]);

        let (shown, nested) = thread(&messages, [1; 32]);
        let ids: Vec<_> = shown
            .iter()
            .map(|m| (m.id[0], m.reply_to.map(|r| r[0])))
            .collect();
        assert_eq!(ids, [(3, None), (4, None)]);
        assert_eq!(nested, [[4; 32]]);
    }

    #[test]
    fn answers_further_down_sit_one_step_in_under_their_reply() {
        let messages = [
            msg(1, None, 10, "# Post"),
            msg(2, Some(1), 20, "First reply"),
            msg(3, Some(1), 30, "Second reply"),
            msg(4, Some(2), 40, "Answer to the first"),
            // Deeper still: shown at the same step, quoting what it answers.
            msg(5, Some(4), 50, "Answer to the answer"),
        ];
        let (shown, nested) = thread(&messages, [1; 32]);
        let ids: Vec<_> = shown
            .iter()
            .map(|m| (m.id[0], m.reply_to.map(|r| r[0])))
            .collect();
        assert_eq!(ids, [(2, None), (4, None), (5, Some(4)), (3, None)]);
        assert_eq!(nested, [[4; 32], [5; 32]]);
    }

    #[test]
    fn a_post_carries_the_poll_its_author_sent_before_it() {
        let messages = [
            poll(1, 10),
            Msg {
                quoted_author: Some(POLL_WRAPPER),
                ..msg(2, Some(1), 11, "# With a poll\nVote below")
            },
            msg(3, Some(2), 20, "Voted"),
            // A poll on its own is a post of its own.
            poll(4, 30),
            // Even its own author can reply without wrapping the poll.
            msg(6, Some(4), 38, "Another thought"),
            Msg {
                person: [2; 32],
                ..msg(5, Some(4), 40, "Me neither")
            },
        ];
        let listed: Vec<_> = posts(&messages)
            .iter()
            .map(|p| (p.msg.id[0], p.poll.as_ref().map(|m| m.id[0]), p.replies))
            .collect();
        assert_eq!(listed, [(4, None, 2), (2, Some(1), 1)]);
        let (shown, _) = thread(&messages, [2; 32]);
        assert_eq!(shown.iter().map(|m| m.id[0]).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn missing_or_unverified_poll_never_hides_the_marked_post() {
        let wrapper = Msg {
            quoted_author: Some(POLL_WRAPPER),
            ..msg(2, Some(1), 10, "# Title")
        };
        let replies = [msg(3, Some(1), 12, "First"), msg(4, Some(3), 14, "Nested")];
        let mut messages = vec![wrapper.clone()];
        assert_eq!(posts(&messages)[0].msg.id, wrapper.id);
        messages.extend(replies);
        messages.push(Msg {
            person: [9; 32],
            ..poll(1, 8)
        });
        assert!(posts(&messages).iter().all(|p| p.poll.is_none()));
        assert!(thread(&messages, wrapper.id).0.is_empty());
        let mut chat = Chat::default();
        chat.messages = messages.clone();
        assert!(carried_poll(&chat, &wrapper).is_none());
        messages.pop();
        messages.push(poll(1, 8));
        let listed = posts(&messages);
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].replies, listed[0].last), (2, 14));
        let (shown, nested) = thread(&messages, wrapper.id);
        assert_eq!(shown.iter().map(|m| m.id[0]).collect::<Vec<_>>(), [3, 4]);
        assert!(shown.iter().all(|m| m.reply_to.is_none()));
        assert_eq!(nested, [[4; 32]]);
    }

    #[test]
    fn failed_forum_parts_retry_from_visible_editor_with_original_ids() {
        use crate::net::Update;
        use egui_kittest::kittest::Queryable;
        let channel = [3; 32];
        for dark in [false, true] {
            let (mut h, mut commands) = super::super::tests::files_harness(dark);
            let ctx = h.ctx.clone();
            ctx.data_mut(|d| d.insert_temp(super::super::files_id(channel), false));
            h.set_size(egui::vec2(820., 1180.));
            let server = &mut h.state_mut().as_mut().unwrap().servers[0];
            server.dir.categories[0].channels[0].forum = true;
            let address = server.address.clone();
            let mut editor = polls::Editor::default();
            editor.question = "Which day?".into();
            editor.options = vec!["Friday".into(), "Saturday".into()];
            let chat = server.chats.get_mut(&channel).unwrap();
            chat.new_post = NewPost {
                open: true,
                title: "Dinner".into(),
                body: "https://example.org/menu".into(),
                poll: Some(editor),
                ..Default::default()
            };
            h.run_steps(3);
            h.get_by_label(t!("forum.post")).click_accesskit();
            h.run_steps(3);
            let mut next = || {
                std::iter::from_fn(|| commands.try_recv().ok()).find_map(|cmd| match cmd {
                    Command::Send(d) => Some(d),
                    _ => None,
                })
            };
            assert!(next().is_none());
            let home = h.state_mut().as_mut().unwrap();
            assert!(
                home.sending
                    .front()
                    .is_some_and(|out| out.to == To::Channel(address.clone(), channel))
            );
            home.send_queued(&ctx, 0.);
            assert!(next().is_none());
            h.get_by_label(t!("previews.not_now")).click_accesskit();
            h.run_steps(3);
            h.state_mut().as_mut().unwrap().send_queued(&ctx, 1.);
            let poll = next().unwrap();
            assert!(poll.poll.is_some());
            assert!(next().is_none());
            let home = h.state_mut().as_mut().unwrap();
            let chat = home.servers[0].chats.get_mut(&channel).unwrap();
            chat.new_post = NewPost {
                open: true,
                title: "Next post".into(),
                ..Default::default()
            };
            home.apply(&ctx, Update::Unsent(address.clone(), poll.clone()), 1.);
            for size in [
                egui::vec2(390., 844.),
                egui::vec2(1100., 700.),
                egui::vec2(820., 1180.),
            ] {
                h.set_size(size);
                h.run_steps(3);
                assert!(h.query_all_by_value("Dinner").next().is_some());
                assert!(h.query_all_by_value("Which day?").next().is_some());
                assert!(
                    h.query_all_by_value("https://example.org/menu")
                        .next()
                        .is_some()
                );
            }
            h.get_by_label(t!("forum.retry")).click_accesskit();
            h.run_steps(3);
            assert_eq!(next().unwrap().id, poll.id);
            let receipt = |id| Update::ForumSent(address.clone(), channel, id);
            let home = h.state_mut().as_mut().unwrap();
            home.apply(
                &ctx,
                Update::ForumSent(
                    "https://elsewhere.example/".parse().unwrap(),
                    channel,
                    poll.id,
                ),
                2.,
            );
            home.apply(
                &ctx,
                Update::ForumSent(address.clone(), [8; 32], poll.id),
                2.,
            );
            assert!(next().is_none());
            home.apply(&ctx, receipt(poll.id), 2.);
            home.apply(&ctx, receipt(poll.id), 2.);
            let text = next().unwrap();
            assert_eq!(text.reply_to, Some((POLL_WRAPPER, poll.id)));
            assert!(next().is_none());
            home.apply(&ctx, Update::Unsent(address.clone(), text.clone()), 3.);
            h.run_steps(3);
            h.get_by_label(t!("forum.retry")).click_accesskit();
            h.run_steps(3);
            assert_eq!(next().unwrap().id, text.id);
            assert!(next().is_none());
            let home = h.state_mut().as_mut().unwrap();
            home.apply(&ctx, receipt(text.id), 4.);
            home.apply(&ctx, Update::Unsent(address.clone(), text), 5.);
            let chat = &home.servers[0].chats[&channel];
            assert!(chat.submissions.is_empty() && chat.draft.is_empty());
            assert_eq!(chat.new_post.title, "Next post");
        }
    }

    #[test]
    fn search_pages_past_votes_in_both_themes() {
        use egui_kittest::kittest::Queryable;
        let channel = [3; 32];
        for dark in [false, true] {
            let (mut h, mut commands) = super::super::tests::files_harness(dark);
            h.ctx
                .data_mut(|d| d.insert_temp(super::super::files_id(channel), false));
            let ctx = h.ctx.clone();
            let home = h.state_mut().as_mut().unwrap();
            let server = &mut home.servers[0];
            server.dir.categories[0].channels[0].forum = true;
            let address = server.address.clone();
            let votes = (1..=50)
                .map(|n| Msg {
                    body: Body::PollPart(PollPart::Vote {
                        poll: [51; 32],
                        sent: n.into(),
                        choices: vec![0],
                    }),
                    ..msg(n, None, 1700000000 + i64::from(n), "")
                })
                .collect();
            home.apply(
                &ctx,
                crate::net::Update::Messages {
                    server: address.clone(),
                    channel,
                    items: votes,
                    older: false,
                    page: Some((false, Some([1; 32]))),
                },
                0.0,
            );
            assert!(home.servers[0].chats[&channel].messages.is_empty());
            h.run_steps(2);
            let server = &h.state().as_ref().unwrap().servers[0];
            let field = UiId::unique(("forum-folds", server.dir.me, &server.address, channel))
                .with("search");
            ctx.memory_mut(|m| m.request_focus(field));
            h.event(egui::Event::Text("SATURDAY".into()));
            h.run_steps(2);
            assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command|
                matches!(command, Command::Older(id, before) if id == channel && before == [1; 32])));
            h.state_mut().as_mut().unwrap().apply(
                &ctx,
                crate::net::Update::Messages {
                    server: address,
                    channel,
                    older: true,
                    page: Some((true, Some([51; 32]))),
                    items: vec![
                        msg(
                            51,
                            None,
                            1700000000,
                            "# Weekend plans\nBoard games on Saturday afternoon.",
                        ),
                        msg(52, None, 1700000001, "# Announcements"),
                    ],
                },
                0.0,
            );
            h.run_steps(3);
            assert!(h.query_by_label("Weekend plans").is_some());
            assert!(h.query_by_label("Announcements").is_none());
        }
    }

    #[test]
    fn title_is_the_first_line_and_posts_send_it_as_a_heading() {
        assert_eq!(title("# Saturday raid\nWho is in?"), "Saturday raid");
        assert_eq!(title("Plain title"), "Plain title");
        assert_eq!(body("# T\nline one\n\nline two"), "line one\n\nline two");
        assert_eq!(
            preview("# T\n**line** one\nline two"),
            "**line** one\nline two"
        );
        assert_eq!(preview("# T\n1\n2\n3\n4\n5"), "1\n2\n3\n4\n…");
        assert_eq!(
            post_text("  Saturday \n raid ", " Who? "),
            "# Saturday raid\nWho?"
        );
    }
}
