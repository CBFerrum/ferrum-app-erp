//! ERP's handle on the vault (Ferrum docs/operations.md, "The vault"). Its secret fields (a carrier connection's credentials) are read with it, in
//! the caller's transaction; no other app's are.

use ferrum_kernel::vault::{Vault, Zeroizing};
use ferrum_kernel::{Error, Kernel, Result, Site, Tx};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Each registration's handle, by the site it registered: one process may hold kernels of several registries (a test's
/// side by side), and each kernel accepts only the handle its own registry gave.
static HANDLES: Mutex<Vec<(usize, Vault)>> = Mutex::new(Vec::new());

fn address(site: &Arc<dyn Site>) -> usize {
    Arc::as_ptr(site) as *const () as usize
}

/// Keeps the handle ERP's registration was given, beside the site it registered with it.
pub(crate) fn remember(site: &Arc<dyn Site>, v: Vault) {
    HANDLES.lock().unwrap_or_else(|e| e.into_inner()).push((address(site), v));
}

fn handle(k: &Kernel) -> Result<Vault> {
    let missing = || Error::Unavailable("ERP was registered without its vault handle: register it as the server does, hooks.app(\"erp\", …)".into());
    let at = address(k.hooks.site("selling").ok_or_else(missing)?);
    let held = HANDLES.lock().unwrap_or_else(|e| e.into_inner());
    held.iter().rev().find(|(a, _)| *a == at).map(|(_, v)| v.clone()).ok_or_else(missing)
}

/// Makes the workspace's key and ERP's own key in it if they are not there yet, each in a transaction of its own:
/// before ERP writes a secret field in a transaction that something then uses the vault beside, on another
/// connection, which would otherwise wait on keys made in that transaction until it timed out.
/// Nothing while the vault is closed: then nothing is written into it either.
pub(crate) async fn ready(k: &Kernel, tenant: Uuid) -> Result<()> {
    match handle(k)?.seal(k, ferrum_kernel::vault::Scope::Workspace(tenant), "erp-ready", b"").await {
        Ok(_) | Err(Error::Unavailable(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The value of one of ERP's own secret fields, read in `tx`: none when it is not set. A closed vault refuses, and
/// says why; never with a value.
pub(crate) async fn field(k: &Kernel, tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid, field: &str) -> Result<Option<Zeroizing<String>>> {
    handle(k)?.field(k, tx, tenant, entity, record, field).await
}
