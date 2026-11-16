//! A document sent to a receiver outside (an access point, a tax authority's platform): one way for every channel.
//! The document is marked Pending and the request queued on the channel's topic, where its connector sends it; the
//! receiver's answer is recorded on it; while the receiver has it, or it is on its way, it is neither sent again nor
//! cancelled; one never sent, or failed, or refused, is sent (again). A channel names its fields, its topic and its
//! statuses, and builds what it sends; nothing here names a country.

use crate::util::{id, s, sys};
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

/// Sets `values` on `entity`'s record `id` as the system, in a transaction of its own: what a connector records of the
/// receiver's answer.
pub async fn record(k: &Kernel, ctx: &Ctx, entity: &str, id: Uuid, values: Value) -> Result<()> {
    let Value::Object(v) = values else { return Ok(()) };
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    k.set_values_tx(&sys(ctx), &mut tx, entity, id, v).await?;
    tx.commit().await?;
    Ok(())
}
