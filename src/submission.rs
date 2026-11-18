//! A document sent to a receiver outside (an access point, a tax authority's platform): one way for every channel.
//! The document is marked Pending and the request queued on the channel's topic, where its connector sends it; the
//! receiver's answer is recorded on it; while the receiver has it, or it is on its way, it is neither sent again nor
//! cancelled; one never sent, or failed, or refused, is sent (again). A channel names its fields, its topic and its
//! statuses, and builds what it sends; nothing here names a country.
//!
//! The request reaches its connector at least once (the platform's docs/events.md): again when an attempt fails, and
//! again, even after the connector finished, when the replica handling it dies. So a connector, for a call that must
//! happen once: does nothing for a document no longer waiting (its answer is recorded); asks [`first_call`] just
//! before the call, and when this is not the first delivery to make it, asks the receiver what became of the request
//! (by the reference the receiver keeps it under) instead of sending again, or, where the receiver cannot be asked,
//! leaves the document to a person, saying why; and records the answer with [`record_while`], which writes nothing over
//! a document that has moved on. A call that certainly did nothing (never connected, [`never_sent`], or a receiver's own
//! "not accepted", such as 429) ends with [`failed_call`], which gives the claim back so the ordinary retry sends it
//! again with no person; any other failure keeps the claim.

use crate::util::{id, s, sys};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, Error, HookCtx, Kernel, Result, Tx, outbox};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// Where a channel keeps its state on the document (`<prefix>_status`, `<prefix>_error`), and what it holds.
pub struct Channel {
    /// What is sent, as a refusal names it ("invoice", "e-way bill").
    pub what: &'static str,
    /// The channel's fields' prefix ("it_sdi": `it_sdi_status`, `it_sdi_error`).
    pub prefix: &'static str,
    /// Where a request to send is queued.
    pub topic: &'static str,
    /// The statuses in which the receiver has the document, or it is on its way.
    pub held: &'static [&'static str],
    /// What a cancel is told to do instead while the receiver has it.
    pub instead: &'static str,
}

impl Channel {
    /// One of the channel's fields on the document (`status`, `error`, `message_id` ...).
    pub fn field(&self, name: &str) -> String {
        format!("{}_{name}", self.prefix)
    }

    /// The channel's status on the document.
    pub fn status<'a>(&self, doc: &'a Doc) -> Option<&'a str> {
        s(doc, &self.field("status"))
    }

    /// Whether the receiver has the document, or it is on its way.
    pub fn holds(&self, doc: &Doc) -> bool {
        self.status(doc).is_some_and(|st| self.held.contains(&st))
    }

    /// Refuses a cancel while the receiver has the document.
    pub fn refuse_cancel(&self, doc: &Doc) -> Result<()> {
        if self.holds(doc) { Err(Error::invalid(self.instead)) } else { Ok(()) }
    }

    /// Refuses sending what the receiver has, or what is on its way.
    pub fn sendable(&self, doc: &Doc) -> Result<()> {
        match self.status(doc) {
            Some(st) if self.held.contains(&st) => Err(Error::invalid(format!("the {} is already {}", self.what, st.to_lowercase()))),
            _ => Ok(()),
        }
    }

    /// Refuses recording an answer for a document not waiting for one (`refusal` says so).
    pub fn waiting(&self, doc: &Doc, refusal: &str) -> Result<()> {
        if self.status(doc) == Some("Pending") { Ok(()) } else { Err(Error::invalid(refusal.to_string())) }
    }

    /// Marks the document Pending, its error cleared, with `values` of the channel's own, and queues `event` (what
    /// the channel built) on its topic. Answers the status.
    pub async fn queue(&self, hc: &mut HookCtx<'_>, doc: &Doc, values: Value, event: Value) -> Result<Value> {
        let answer = self.record(hc, doc, "Pending", None, values).await?;
        enqueue(hc, doc, self.topic, event).await?;
        Ok(answer)
    }

    /// Records `status` on the document, with `error` (none: cleared) and `values` of the channel's own. Answers the
    /// status.
    pub async fn record(&self, hc: &mut HookCtx<'_>, doc: &Doc, status: &str, error: Option<String>, values: Value) -> Result<Value> {
        let mut v = match values {
            Value::Object(v) => v,
            _ => Map::new(),
        };
        v.insert(self.field("status"), json!(status));
        v.insert(self.field("error"), json!(error));
        save(hc, doc, Value::Object(v)).await?;
        Ok(json!({"status": status}))
    }
}

/// Sets `values` on the document a hook acts on, as the system.
pub async fn save(hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
    let rid = id(doc, "id").ok_or_else(|| Error::internal("a document without its id"))?;
    let Value::Object(v) = values else { return Ok(()) };
    let entity = hc.entity.def.name.clone();
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, &entity, rid, v).await
}

/// Queues `event` on `topic`, for the document a hook acts on.
pub async fn enqueue(hc: &mut HookCtx<'_>, doc: &Doc, topic: &str, event: Value) -> Result<()> {
    let rid = id(doc, "id").ok_or_else(|| Error::internal("a document without its id"))?;
    let entity = hc.entity.def.name.clone();
    outbox::enqueue(hc.tx, hc.ctx.tenant, topic, Some(&entity), Some(rid), event).await
}

/// Sets `values` on `entity`'s record `id` as the system, in a transaction of its own, while its status field is one of
/// `from` (`(field, from)`, read with the record locked): what a connector records of the receiver's answer. A record
/// that has moved on, its answer recorded by an earlier delivery, is left as it is: a success is never written over by
/// what a repeat heard.
pub async fn record_while(k: &Kernel, ctx: &Ctx, entity: &str, id: Uuid, status: (&str, &[&str]), values: Value) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    if record_while_tx(k, ctx, &mut tx, entity, id, status, values).await? {
        tx.commit().await?;
    }
    Ok(())
}

/// [`record_while`] in a transaction the caller holds (an action recording a receiver's answer by hand). Answers whether
/// it was recorded.
pub async fn record_while_tx(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, (field, from): (&str, &[&str]), values: Value) -> Result<bool> {
    let Value::Object(v) = values else { return Ok(false) };
    let doc = k.get_tx(&sys(ctx), tx, entity, id, true).await?;
    if !from.contains(&s(&doc, field).unwrap_or_default()) {
        return Ok(false);
    }
    k.set_values_tx(&sys(ctx), tx, entity, id, v).await?;
    Ok(true)
}

/// Whether this delivery of `ev` is the first to make `consumer`'s outside call for it: asked just before the call and
/// recorded at once, in a transaction of its own (none is open across the call). No: an earlier delivery began the
/// call, and the receiver may have acted on it before that delivery's replica died or its attempt failed; the
/// connector asks the receiver what became of it, or leaves it to a person, and does not send it blind again.
pub async fn first_call(k: &Kernel, ev: &Event, consumer: &str) -> Result<bool> {
    let mut tx = k.db.system_tenant_tx(ev.tenant_id).await?;
    let first = outbox::first_time(&mut tx, ev.tenant_id, consumer, ev.id).await?;
    tx.commit().await?;
    Ok(first)
}

/// Ends a delivery whose outside call failed with `why`, under the claim [`first_call`] gave it. When the call certainly
/// did nothing (`unsent`: the connection was never made, see [`never_sent`], or the receiver answered with its own
/// "not accepted, nothing recorded", such as 429) the claim is given back (`outbox::give_back`) in a transaction of its
/// own, so the dispatcher's ordinary retry sends it again; otherwise it stays, and the retry asks the receiver or leaves
/// it to a person. Only a connector that holds the claim may say `unsent`: one asking about an earlier delivery's call
/// does not hold it. Answers `why`.
pub async fn failed_call(k: &Kernel, ev: &Event, consumer: &str, unsent: bool, why: Error) -> Result<()> {
    if unsent {
        let mut tx = k.db.system_tenant_tx(ev.tenant_id).await?;
        outbox::give_back(&mut tx, ev.tenant_id, consumer, ev.id).await?;
        tx.commit().await?;
    }
    Err(why)
}

/// Whether a call that failed with `e` certainly never reached the receiver: the connection (or its TLS handshake) was
/// never made, so not a byte of the request was written. A timeout is not, unless it is one of connecting; nor is any
/// error once the connection was made, as the request may have gone.
pub fn never_sent(e: &reqwest::Error) -> bool {
    e.is_connect()
}

#[cfg(test)]
mod tests {
    use super::never_sent;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// What a connector's call can meet: a port nobody listens on, a server that takes the request and never answers,
    /// and one that closes halfway through its answer. Only the first never reached it.
    #[tokio::test]
    async fn only_a_connection_never_made_is_certainly_unsent() {
        let client = reqwest::Client::builder().timeout(Duration::from_millis(300)).build().unwrap();
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let refused = client.post(format!("http://{closed}/x")).body("doc").send().await.unwrap_err();
        assert!(never_sent(&refused), "{refused:?}");
        let serve = async |answer: Option<&'static [u8]>| {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let at = l.local_addr().unwrap();
            tokio::spawn(async move {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                match answer {
                    Some(part) => {
                        let _ = s.write_all(part).await;
                    }
                    None => std::future::pending::<()>().await,
                }
            });
            at
        };
        let hangs = serve(None).await;
        let timed_out = client.post(format!("http://{hangs}/x")).body("doc").send().await.unwrap_err();
        assert!(timed_out.is_timeout() && !never_sent(&timed_out), "{timed_out:?}");
        let cut = serve(Some(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npart")).await;
        let reply = client.post(format!("http://{cut}/x")).body("doc").send().await;
        let broken = match reply {
            Ok(r) => r.text().await.unwrap_err(),
            Err(e) => e,
        };
        assert!(!never_sent(&broken), "{broken:?}");
    }
}
