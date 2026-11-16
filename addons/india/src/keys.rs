//! The India add-on's handle on the vault (Ferrum docs/operations.md, "The vault"). Its secret fields (a company's IRP credentials and session) are read with it, in
//! the caller's transaction; no other app's are.

use ferrum_kernel::vault::{Vault, Zeroizing};
use ferrum_kernel::{Error, Kernel, Result, Tx};
use std::sync::RwLock;
use uuid::Uuid;

/// The India add-on's handle on the vault, from its registration.
static VAULT: RwLock<Option<Vault>> = RwLock::new(None);

/// Keeps the handle the registration gave The India add-on.
// ponytail: the last registration's handle serves the whole process (`ferrum start` sets the server up twice and serves
// with the second); keep it beside a site of its own if one process ever serves two kernels at once.
pub(crate) fn remember(v: Vault) {
    *VAULT.write().unwrap_or_else(|e| e.into_inner()) = Some(v);
}

/// The value of one of The India add-on's own secret fields, read in `tx`: none when it is not set. A closed vault refuses, and
/// says why; never with a value.
pub(crate) async fn field(k: &Kernel, tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid, field: &str) -> Result<Option<Zeroizing<String>>> {
    let v = VAULT.read().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| {
        Error::Unavailable("The India add-on was registered without its vault handle: register it as the server does, hooks.app(\"india\", …)".into())
    })?;
    v.field(k, tx, tenant, entity, record, field).await
}
