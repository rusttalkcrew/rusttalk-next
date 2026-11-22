//! One background scan at a time. Each network step uses its server's existing
//! authenticated session, yielding between pages and between transferred messages.
use super::*;
use rusttalk_core::client::backfill::{
    DM_DEPTH, DM_PAGE, DmHistoryItem, InventoryEntry, Refusal, missing_dm_ids,
};
use tokio::sync::oneshot;

type Reply<T> = oneshot::Sender<Result<T>>;
type Sender = UnboundedSender<Command>;
type Inventory = Vec<InventoryEntry>;
#[derive(Default)]
pub(super) struct Progress {
    done: HashSet<Id>,
    refused: bool,
}

pub(super) type Completed = HashMap<(Url, Id, Url, Option<Id>), Progress>;

pub enum Destination {
    Unsupported,
    Missing,
    Channel(Id),
}

pub enum Request {
    Destination(Id, bool, Reply<Destination>),
    Needs(Id, Id, Id, Reply<bool>),
    Page(Id, Id, Option<Id>, Reply<Inventory>),
    Read(Id, Id, Id, Reply<Option<DmHistoryItem>>),
    Restore(Id, Id, Box<DmHistoryItem>, Reply<()>),
}

impl Request {
    pub(super) async fn run(
        self,
        ctx: &Ctx,
        url: &Url,
        client: &mut KeyClient<'_>,
        state: &mut State,
    ) {
        match self {
            Self::Destination(peer, create, reply) => {
                let result = if !state.dm_backfill {
                    Ok(Destination::Unsupported)
                } else if let Some(dm) = state
                    .last
                    .iter()
                    .flat_map(|dir| &dir.dms)
                    .find(|dm| dm.peer == peer)
                {
                    Ok(Destination::Channel(dm.channel))
                } else if create {
                    dm_channel(ctx, url, client, state, peer)
                        .await
                        .map(Destination::Channel)
                } else {
                    Ok(Destination::Missing)
                };
                let _ = reply.send(result);
            }
            Self::Needs(channel, peer, id, reply) => {
                let _ = reply.send(client.needs_dm_copy(&channel, peer, id).await);
            }
            Self::Page(channel, peer, before, reply) => {
                let result = async {
                    if !client.dm_private(&channel, &peer) {
                        ctx.send(Update::RetryDmBackfill);
                        anyhow::bail!("DM keys pending");
                    }
                    let page = client
                        .message_page(&channel, before.as_ref(), DM_PAGE)
                        .await?;
                    Ok(page
                        .into_iter()
                        .take(DM_PAGE as usize)
                        .map(|m| (m.id, m.person, m.at))
                        .collect())
                }
                .await;
                let _ = reply.send(result);
            }
            Self::Read(channel, peer, id, reply) => {
                let _ = reply.send(client.read_dm_copy(&channel, peer, id).await);
            }
            Self::Restore(channel, id, mut item, reply) => {
                let result = async {
                    let deleted = ctx.sync.current_dm_deletes().await;
                    if skipped(&deleted, id, item.body.edit_of) {
                        return Ok(());
                    }
                    anyhow::ensure!(
                        state.dm_backfill,
                        "destination does not support DM backfill"
                    );
                    let dir = state.last.as_ref().context("DM directory unavailable")?;
                    crate::emoji::for_replica(
                        &mut item.body.text,
                        &mut item.body.sticker,
                        &dir.emoji,
                    );
                    client.restore_dm_copy(&channel, *item).await
                }
                .await;
                let _ = reply.send(result);
            }
        }
    }
}

async fn ask<T>(server: &Sender, request: impl FnOnce(Reply<T>) -> Request) -> Result<T> {
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (tx, rx) = oneshot::channel();
    server
        .send(Command::Backfill(request(tx)))
        .map_err(|_| anyhow::anyhow!("server disconnected"))?;
    rx.await?
}

async fn inventory(server: &Sender, channel: Id, peer: Id) -> Result<Inventory> {
    let mut ids = Vec::new();
    while ids.len() < DM_DEPTH {
        let before = ids.last().map(|&(id, _, _)| id);
        let page = ask(server, |reply| Request::Page(channel, peer, before, reply)).await?;
        let end = page.len() < DM_PAGE as usize;
        ids.extend(page);
        if end {
            break;
        }
    }
    Ok(ids)
}

struct Route {
    peer: Id,
    channel: Id,
    source: Sender,
    source_url: Url,
    target: Sender,
    target_url: Url,
}

fn skipped(deleted: &crate::dm::Tombstones, id: Id, original: Option<Id>) -> bool {
    deleted.contains(&id) || original.is_some_and(|id| deleted.contains(&id))
}

fn request_status(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
}

impl Progress {
    fn remember(&mut self, id: Id, result: Result<()>) -> Result<()> {
        if let Err(error) = result {
            let status = request_status(&error);
            match (error.downcast_ref::<Refusal>(), status) {
                (Some(Refusal::Destination), _)
                | (_, Some(StatusCode::FORBIDDEN | StatusCode::PAYLOAD_TOO_LARGE)) => {
                    self.refused = true
                }
                (Some(Refusal::Record), _)
                | (_, Some(StatusCode::NOT_FOUND | StatusCode::CONFLICT)) => {}
                _ => return Err(error),
            }
            tracing::info!(%error, "DM history permanently refused on this route");
        }
        self.done.insert(id);
        Ok(())
    }
}

async fn repair(
    me: Id,
    route: Route,
    deleted: &crate::dm::Tombstones,
    completed: &mut Completed,
) -> Result<()> {
    let Route {
        peer,
        channel,
        source,
        source_url,
        target,
        target_url,
    } = route;
    let target_dm = ask(&target, |reply| Request::Destination(peer, false, reply)).await?;
    let known = match target_dm {
        Destination::Unsupported => return Ok(()),
        Destination::Missing => None,
        Destination::Channel(id) => Some(id),
    };
    let key = (source_url, channel, target_url, known);
    if completed.get(&key).is_some_and(|p| p.refused) {
        return Ok(());
    }
    let scan = async {
        let held = match known {
            Some(id) => inventory(&target, id, peer).await?,
            None => Vec::new(),
        };
        let source_ids = inventory(&source, channel, peer).await?;
        let missing = missing_dm_ids(&source_ids, &held, me);
        if !missing.iter().any(|id| !deleted.contains(id)) {
            return Ok(None);
        }
        let destination = match known {
            Some(id) => id,
            None => match ask(&target, |reply| Request::Destination(peer, true, reply)).await? {
                Destination::Channel(id) => id,
                _ => return Ok(None),
            },
        };
        Ok(Some((destination, missing)))
    }
    .await;
    let (destination, missing) = match scan {
        Ok(Some(scan)) => scan,
        Ok(None) => return Ok(()),
        Err(error) => {
            let status = request_status(&error);
            if !(known.is_none() && status == Some(StatusCode::CONFLICT))
                && matches!(
                    status,
                    Some(
                        StatusCode::CONFLICT
                            | StatusCode::FORBIDDEN
                            | StatusCode::PAYLOAD_TOO_LARGE
                    )
                )
            {
                completed.entry(key).or_default().refused = true;
            }
            return Err(error);
        }
    };
    let progress = completed
        .entry((key.0, channel, key.2, Some(destination)))
        .or_default();
    for id in missing {
        if skipped(deleted, id, None) || progress.done.contains(&id) {
            continue;
        }
        let transfer = async {
            if ask(&target, |reply| {
                Request::Needs(destination, peer, id, reply)
            })
            .await?
                && let Some(item) =
                    ask(&source, |reply| Request::Read(channel, peer, id, reply)).await?
                && !skipped(deleted, id, item.body.edit_of)
            {
                ask(&target, |reply| {
                    Request::Restore(destination, id, Box::new(item), reply)
                })
                .await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        progress.remember(id, transfer)?;
        if progress.refused {
            break;
        }
    }
    Ok(())
}

impl Net {
    pub fn backfill(
        &self,
        servers: &[crate::server::Server],
        deleted: &crate::dm::Tombstones,
    ) -> tokio::task::JoinHandle<()> {
        let mut routes = Vec::new();
        for source in servers.iter().filter(|s| s.status == Status::Online) {
            for dm in &source.dir.dms {
                for target in servers.iter().filter(|s| {
                    s.status == Status::Online
                        && s.address != source.address
                        && s.member(&dm.peer).is_some()
                }) {
                    routes.push(Route {
                        peer: dm.peer,
                        channel: dm.channel,
                        source: source.commands.clone(),
                        source_url: source.address.clone(),
                        target: target.commands.clone(),
                        target_url: target.address.clone(),
                    });
                }
            }
        }
        let me = *self.ctx.identity.certificate.person_id();
        let deleted = deleted.clone();
        let completed = self.completed_dm_copies.clone();
        self.runtime
            .as_ref()
            .expect("network runtime")
            .spawn(async move {
                let mut completed = completed.lock().await;
                for route in routes {
                    if let Err(error) = repair(me, route, &deleted, &mut completed).await {
                        tracing::info!(%error, "DM history scan deferred");
                    }
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn repair_probes_first_skips_no_copy_and_stops_a_refused_route() {
        let me = [1; 32];
        let peer = [2; 32];
        let channel = [3; 32];
        let destination = [4; 32];
        let (source, mut from_source) = unbounded_channel();
        let (target, mut from_target) = unbounded_channel();
        let route = || Route {
            peer,
            channel,
            source: source.clone(),
            target: target.clone(),
            source_url: "https://source.example".parse().unwrap(),
            target_url: "https://target.example".parse().unwrap(),
        };
        let held = [5; 32];
        let no_copy = [6; 32];
        let denied = [7; 32];
        let later = [8; 32];
        let responders = async {
            // Listen to both connections so a read on the wrong side fails immediately.
            for step in 0..8 {
                let (is_source, command) = tokio::select! {
                    Some(c) = from_source.recv() => (true,c),
                    Some(c) = from_target.recv() => (false,c),
                };
                let Command::Backfill(request) = command else {
                    panic!("unexpected command")
                };
                match (step, is_source, request) {
                    (0 | 7, false, Request::Destination(p, false, reply)) if p == peer => {
                        let _ = reply.send(Ok(Destination::Channel(destination)));
                    }
                    (1, false, Request::Page(c, _, None, reply)) if c == destination => {
                        let _ = reply.send(Ok(vec![]));
                    }
                    (2, true, Request::Page(c, _, None, reply)) if c == channel => {
                        let _ = reply.send(Ok(vec![
                            (later, me, 4),
                            (denied, me, 3),
                            (no_copy, me, 2),
                            (held, me, 1),
                        ]));
                    }
                    (3, false, Request::Needs(_, _, id, reply)) if id == held => {
                        let _ = reply.send(Ok(false));
                    }
                    (4, false, Request::Needs(_, _, id, reply)) if id == no_copy => {
                        let _ = reply.send(Ok(true));
                    }
                    (5, true, Request::Read(_, _, id, reply)) if id == no_copy => {
                        let _ = reply.send(Ok(None));
                    }
                    (6, false, Request::Needs(_, _, id, reply)) if id == denied => {
                        let _ = reply.send(Err(Refusal::Destination.into()));
                    }
                    _ => panic!("unexpected repair request at step {step}"),
                }
            }
        };
        let mut completed = Completed::default();
        let scans = async {
            for _ in 0..2 {
                repair(me, route(), &Default::default(), &mut completed)
                    .await
                    .unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(scans, responders);
        })
        .await
        .unwrap();
        let progress = completed.values().next().unwrap();
        assert!(progress.refused);
        assert_eq!(progress.done, HashSet::from([held, no_copy, denied]));
    }

    #[test]
    fn conflicted_post_is_terminal_only_for_that_record() {
        let mut progress = Progress::default();
        let id = [1; 32];
        progress.remember(id, Err(Refusal::Record.into())).unwrap();
        assert!(progress.done.contains(&id));
        assert!(!progress.refused && !progress.done.contains(&[2; 32]));
        progress
            .remember([2; 32], Err(Refusal::Destination.into()))
            .unwrap();
        assert!(progress.refused);
    }
    #[test]
    fn tombstones_skip_the_record_and_edits_of_it() {
        let mut deleted = crate::dm::Tombstones::default();
        deleted.add([1; 32], ["replica".to_owned()]);
        assert!(skipped(&deleted, [1; 32], None));
        assert!(skipped(&deleted, [2; 32], Some([1; 32])));
        assert!(!skipped(&deleted, [2; 32], Some([3; 32])));
    }
}
