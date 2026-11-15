//! The services ERP's integrations call for a company: tax authorities and e-invoicing portals (HMRC, AvaTax,
//! MyInvois, ANAF, the Commission's VIES and TEDB). Every call is made for a workspace and goes through
//! [`Provider::client`]: the platform is asked first whether that workspace may reach outside (a sandbox copy may not,
//! so it files nothing for the live company), and the call goes through the platform's vetted client (https, every
//! resolved address public and pinned, no redirects, a time limit).
//!
//! A provider's address is its production host, or its own test environment's where it has one, which the company's
//! settings choose (`gb_hmrc_mode`, `us_avatax_mode`, `my_myinvois_mode`, `ro_anaf_mode`). Through this release an
//! address in the environment (`FERRUM_GB_HMRC_URL` and the others) still stands in for the provider's, as before.

use ferrum_kernel::{Kernel, Result};
use std::time::Duration;
use uuid::Uuid;

/// A service an integration calls.
pub struct Provider {
    /// Its name, in messages.
    pub name: &'static str,
    /// The environment variable that, through this release, may stand in for its address.
    pub env: &'static str,
    /// Its hosts: production's and its own test environment's.
    pub hosts: &'static [&'static str],
}

impl Provider {
    /// The address the environment gives in place of the provider's own, if it gives one.
    pub fn env_address(&self) -> Option<String> {
        std::env::var(self.env).ok().filter(|u| !u.trim().is_empty())
    }

    /// A client for a call to `url` made for `tenant`, and the parsed address: refused for a sandbox (it reaches
    /// nothing outside) and by the platform's checks.
    pub async fn client(&self, k: &Kernel, tenant: Uuid, url: &str, timeout: Duration) -> Result<(reqwest::Client, reqwest::Url)> {
        k.outward(tenant).await?;
        k.vetted_client_for(tenant, url, k.settings.webhooks_allow_private, timeout, &format!("Ferrum-{}/1", self.name)).await
    }
}
