//! The services ERP's integrations call for a company: tax authorities and e-invoicing portals (HMRC, AvaTax,
//! MyInvois, ANAF, the Commission's VIES and TEDB). Every call is made for a workspace and goes through
//! [`Provider::client`]: the platform is asked first whether that workspace may reach outside (a sandbox copy may not,
//! so it files nothing for the live company), and the call goes through the platform's vetted client (https, every
//! resolved address public and pinned, no redirects, a time limit).
//!
//! A provider's address is its production host, or its own test environment's where it has one, which the company's
//! settings choose (`gb_hmrc_mode`, `us_avatax_mode`, `my_myinvois_mode`, `ro_anaf_mode`). Through this release an
//! address in the environment (`FERRUM_GB_HMRC_URL` and the others) still stands in for the provider's, as before; but
//! outside development (`webhooks_allow_private`, which only the development and test configurations turn on) it is
//! called only when its host is one of the provider's, so a changed environment cannot send a company's credentials
//! anywhere else. It is refused, saying so, rather than passed over.

use ferrum_kernel::{Error, Kernel, Result};
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

    /// Whether `url` may be called: one of the provider's hosts, or, only in development, any.
    pub fn check(&self, url: &str, development: bool) -> Result<()> {
        if development {
            return Ok(());
        }
        let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase));
        if host.as_deref().is_some_and(|h| self.hosts.contains(&h)) {
            return Ok(());
        }
        // The host alone: an address can carry a user name and password.
        Err(Error::invalid(format!(
            "{} is called only at its own addresses ({}), not at {}: unset {}, which only a development server may point elsewhere",
            self.name,
            self.hosts.join(", "),
            host.as_deref().unwrap_or("an address without a host"),
            self.env
        )))
    }

    /// A client for a call to `url` made for `tenant`, and the parsed address: refused for a sandbox (it reaches
    /// nothing outside), for an address that is not the provider's, and by the platform's checks.
    pub async fn client(&self, k: &Kernel, tenant: Uuid, url: &str, timeout: Duration) -> Result<(reqwest::Client, reqwest::Url)> {
        k.outward(tenant).await?;
        let development = k.settings.webhooks_allow_private;
        self.check(url, development)?;
        k.vetted_client_for(tenant, url, development, timeout, &format!("Ferrum-{}/1", self.name)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HMRC: Provider = Provider { name: "HMRC", env: "FERRUM_GB_HMRC_URL", hosts: &["api.service.hmrc.gov.uk", "test-api.service.hmrc.gov.uk"] };

    #[test]
    fn outside_development_only_the_providers_hosts_are_called() {
        for ok in [
            "https://api.service.hmrc.gov.uk/organisations/vat/1/returns",
            "https://test-api.service.hmrc.gov.uk/oauth/token",
            "https://API.service.hmrc.gov.uk/x",
        ] {
            assert!(HMRC.check(ok, false).is_ok(), "{ok}");
        }
        // What a changed environment could name: another host, one that only looks like the provider's, a user name
        // before the host, a private address, no address at all.
        for bad in [
            "https://collector.example.net/oauth/token",
            "https://api.service.hmrc.gov.uk.example.net/oauth/token",
            "https://api.service.hmrc.gov.uk@collector.example.net/oauth/token",
            "http://127.0.0.1:8080/oauth/token",
            "api.service.hmrc.gov.uk/oauth/token",
            "",
        ] {
            let refused = HMRC.check(bad, false);
            assert!(matches!(&refused, Err(Error::Invalid(m)) if m.contains("FERRUM_GB_HMRC_URL")), "{bad}: {refused:?}");
        }
        // What the refusal says names the host, never a user name or password the address carries.
        let said = HMRC.check("https://ops:hunter2@collector.example.net/oauth/token", false).unwrap_err().to_string();
        assert!(said.contains("collector.example.net") && !said.contains("hunter2") && !said.contains("ops:"), "{said}");
        // A development server calls its test doubles wherever they listen.
        assert!(HMRC.check("http://127.0.0.1:8080/oauth/token", true).is_ok());
    }
}
