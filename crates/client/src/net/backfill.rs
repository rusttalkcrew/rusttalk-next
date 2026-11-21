//! One background scan at a time. Each network step uses its server's existing
//! authenticated session, yielding between pages and between transferred messages.
use super::*;
use rusttalk_core::client::backfill::{DM_DEPTH, DM_PAGE, DmHistoryItem, missing_dm_ids};
use tokio::sync::oneshot;

type Reply<T> = oneshot::Sender<Result<T>>;
type Sender = UnboundedSender<Command>;
type Inventory = Vec<(Id, Id)>;

pub enum Request {
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
            Self::Page(channel, peer, before, reply) => {
                let result = async {
                    anyhow::ensure!(client.dm_private(&channel, &peer), "DM keys pending");
                    let page = client
                        .message_page(&channel, before.as_ref(), DM_PAGE)
                        .await?;
                    Ok(page
                        .into_iter()
                        .take(DM_PAGE as usize)
                        .map(|m| (m.id, m.person))
                        .collect())
                }
                .await;
                let _ = reply.send(result);
            }
            Self::Read(channel, peer, id, reply) => {
                let _ = reply.send(client.read_dm_copy(&channel, peer, id).await);
            }
            Self::Restore(peer, id, mut item, reply) => {
                let result = async {
                    let deleted = ctx.sync.current_dm_deletes().await;
                    if deleted.contains(&id)
                        || item.body.edit_of.is_some_and(|id| deleted.contains(&id))
                    {
                        return Ok(());
                    }
                    let channel = dm_channel(ctx, url, client, state, peer).await?;
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
        let before = ids.last().map(|&(id, _)| id);
        let page = ask(server, |reply| Request::Page(channel, peer, before, reply)).await?;
        let end = page.len() < DM_PAGE as usize;
        ids.extend(page);
        if end {
            break;
        }
    }
    Ok(ids)
}

pub(super) type Route = (Id, Id, Sender, Option<Id>, Sender);

async fn repair(
    me: Id,
    (peer, channel, source, target_channel, target): Route,
    deleted: &crate::dm::Tombstones,
) -> Result<()> {
    let source_ids = inventory(&source, channel, peer).await?;
    let held = match target_channel {
        Some(channel) => inventory(&target, channel, peer).await?,
        None => Vec::new(),
    };
    for id in missing_dm_ids(&source_ids, &held, me) {
        if deleted.contains(&id) {
            continue;
        }
        let transfer = async {
            if let Some(item) =
                ask(&source, |reply| Request::Read(channel, peer, id, reply)).await?
                && !item
                    .body
                    .edit_of
                    .is_some_and(|original| deleted.contains(&original))
            {
                ask(&target, |reply| {
                    Request::Restore(peer, id, Box::new(item), reply)
                })
                .await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = transfer {
            tracing::warn!(%error, "DM history record could not be backfilled");
            if source.is_closed() || target.is_closed() {
                break;
            }
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
                    routes.push((
                        dm.peer,
                        dm.channel,
                        source.commands.clone(),
                        target
                            .dir
                            .dms
                            .iter()
                            .find(|d| d.peer == dm.peer)
                            .map(|d| d.channel),
                        target.commands.clone(),
                    ));
                }
            }
        }
        let me = *self.ctx.identity.certificate.person_id();
        let deleted = deleted.clone();
        self.runtime
            .as_ref()
            .expect("network runtime")
            .spawn(async move {
                for route in routes {
                    if let Err(error) = repair(me, route, &deleted).await {
                        tracing::info!(%error, "DM history scan deferred");
                    }
                }
            })
    }
}
