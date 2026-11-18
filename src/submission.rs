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
//! a document that has moved on.

use crate::util::{id, s, sys};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, Error, HookCtx, Kernel, Result, outbox};
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
pub async fn record_while(k: &Kernel, ctx: &Ctx, entity: &str, id: Uuid, (field, from): (&str, &[&str]), values: Value) -> Result<()> {
    let Value::Object(v) = values else { return Ok(()) };
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let doc = k.get_tx(&sys(ctx), &mut tx, entity, id, true).await?;
    if !from.contains(&s(&doc, field).unwrap_or_default()) {
        return Ok(());
    }
    k.set_values_tx(&sys(ctx), &mut tx, entity, id, v).await?;
    tx.commit().await?;
    Ok(())
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
