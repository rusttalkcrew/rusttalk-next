//! Wayland: apps can't see keys pressed in other windows, so the desktop binds the
//! shortcuts through its GlobalShortcuts portal (KDE, GNOME 48+, Hyprland). It shows
//! its own confirmation, may pick other keys, and reports presses and releases.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    thread,
};

use anyhow::Result;
use ashpd::desktop::{
    Session,
    global_shortcuts::{GlobalShortcuts, NewShortcut, Shortcut},
};
use eframe::egui;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::{Action, Support, deliver};
use crate::audio::lock;

enum Request {
    /// Bind these actions with their preferred keys, in place of the old ones.
    Bind(Vec<(Action, String)>),
    /// Open the desktop's own settings for them.
    Configure,
}

pub struct Portal {
    requests: mpsc::UnboundedSender<Request>,
    support: Arc<Mutex<Support>>,
}

impl Portal {
    pub fn start(ctx: egui::Context) -> Self {
        let (requests, receive) = mpsc::unbounded_channel();
        let support = Arc::new(Mutex::new(Support::Checking));
        let shared = support.clone();
        let spawned = thread::Builder::new()
            .name("rusttalk-shortcuts".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| runtime.block_on(run(receive, &shared, &ctx)));
                if let Err(error) = result {
                    tracing::warn!(error = %format_args!("{error:#}"), "global shortcuts portal unavailable");
                }
                *lock(&shared) = Support::Unsupported;
                ctx.request_repaint();
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "starting the global shortcuts thread failed");
            *lock(&support) = Support::Unsupported;
        }
        Self { requests, support }
    }

    pub fn bind(&self, shortcuts: Vec<(Action, String)>) {
        let _ = self.requests.send(Request::Bind(shortcuts));
    }

    pub fn configure(&self) {
        let _ = self.requests.send(Request::Configure);
    }

    pub fn support(&self) -> Support {
        lock(&self.support).clone()
    }
}

fn action(id: &str) -> Option<Action> {
    Action::ALL.into_iter().find(|action| action.id() == id)
}

/// Show the keys the desktop picked.
fn set_triggers(support: &Mutex<Support>, ctx: &egui::Context, shortcuts: &[Shortcut]) {
    if let Support::Portal { triggers, .. } = &mut *lock(support) {
        *triggers = (shortcuts.iter())
            .filter_map(|s| Some((action(s.id())?, s.trigger_description().to_owned())))
            .collect();
    }
    ctx.request_repaint();
}

async fn run(
    mut requests: mpsc::UnboundedReceiver<Request>,
    support: &Mutex<Support>,
    ctx: &egui::Context,
) -> Result<()> {
    let portal = GlobalShortcuts::new().await?;
    let mut activated = portal.receive_activated().await?;
    let mut deactivated = portal.receive_deactivated().await?;
    let mut changed = portal.receive_shortcuts_changed().await?;
    *lock(support) = Support::Portal {
        configurable: portal.version() >= 2,
        triggers: BTreeMap::new(),
    };
    ctx.request_repaint();
    // Signals only come for this app's sessions, and only one is open at a time.
    let mut session: Option<Session<GlobalShortcuts>> = None;
    loop {
        tokio::select! {
            request = requests.recv() => match request {
                None => return Ok(()),
                Some(Request::Bind(shortcuts)) => {
                    if let Some(old) = session.take()
                        && let Err(error) = old.close().await
                    {
                        tracing::warn!(%error, "closing the old shortcuts session failed");
                    }
                    deliver(Action::PushToTalk, false);
                    set_triggers(support, ctx, &[]);
                    if shortcuts.is_empty() {
                        continue;
                    }
                    let new = match portal.create_session(Default::default()).await {
                        Ok(new) => new,
                        Err(error) => {
                            tracing::warn!(%error, "starting a global shortcuts session failed");
                            continue;
                        }
                    };
                    let wanted: Vec<_> = (shortcuts.iter())
                        .map(|(action, keys)| {
                            NewShortcut::new(action.id(), action.label())
                                .preferred_trigger(keys.as_str())
                        })
                        .collect();
                    // The desktop may ask the person first; closing its dialog binds nothing.
                    match portal.bind_shortcuts(&new, &wanted, None, Default::default()).await
                        .and_then(|request| request.response())
                    {
                        Ok(bound) => set_triggers(support, ctx, bound.shortcuts()),
                        Err(error) => tracing::warn!(%error, "binding global shortcuts failed"),
                    }
                    session = Some(new);
                }
                Some(Request::Configure) => {
                    if let Some(session) = &session
                        && let Err(error) = portal.configure_shortcuts(session, None, Default::default()).await
                    {
                        tracing::warn!(%error, "opening the desktop's shortcut settings failed");
                    }
                }
            },
            Some(event) = activated.next() => {
                if let Some(action) = action(event.shortcut_id()) {
                    deliver(action, true);
                }
            }
            Some(event) = deactivated.next() => {
                if let Some(action) = action(event.shortcut_id()) {
                    deliver(action, false);
                }
            }
            Some(event) = changed.next() => set_triggers(support, ctx, event.shortcuts()),
        }
    }
}
