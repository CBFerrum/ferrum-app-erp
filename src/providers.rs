//! The services ERP's integrations call for a company: tax authorities and e-invoicing portals (HMRC, AvaTax,
//! MyInvois, ANAF, the Commission's VIES and TEDB). Every call is made for a workspace and goes through
//! [`Provider::client`]: the platform is asked first whether that workspace may reach outside (a sandbox copy may not,
//! so it files nothing for the live company), and the call goes through the platform's vetted client (https, every
//! resolved address public and pinned, no redirects, a time limit).
//!
//! A provider's address is its production host, or its own test environment's where it has one, which the company's
//! settings choose (`gb_hmrc_mode`, `us_avatax_mode`, `my_myinvois_mode`, `ro_anaf_mode`). An operator's setting (each
//! add-on's `operator_settings`, `gb.hmrc_url` and the others, or its variable `FERRUM_GB_HMRC_URL`, which wins) may
//! stand in for the provider's address, read at each call; but outside development (`webhooks_allow_private`, which only
//! the development and test configurations turn on) it is called only when its host is one of the provider's, so a
//! changed row or environment cannot send a company's credentials anywhere else. It is refused, saying so, rather than
//! passed over.

use ferrum_kernel::{Error, Kernel, Result};
use std::time::Duration;
use uuid::Uuid;

/// A service an integration calls.
pub struct Provider {
    /// Its name, in messages.
    pub name: &'static str,
    /// The variable of the operator's setting that may stand in for its address (the add-on's `operator_settings`).
    pub env: &'static str,
    /// Its hosts: production's and its own test environment's.
    pub hosts: &'static [&'static str],
}

impl Provider {
    /// The address the operator's setting gives in place of the provider's own, if it gives one: the environment's, else
    /// the settings table's (`ferrum_kernel::env`), at each call.
    pub fn env_address(&self) -> Option<String> {
        ferrum_kernel::env::var(self.env).filter(|u| !u.trim().is_empty())
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
            "{} is called only at its own addresses ({}), not at {}: unset the setting {}, which only a development server may point elsewhere",
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

    /// The address from the settings table (nothing in the environment) reaches the add-on, at the next call, and a
    /// changed row naming another host is refused like a changed environment: the company's credential stays with HMRC.
    #[test]
    fn a_stored_address_is_used_and_held_to_the_providers_hosts() {
        assert!(std::env::var("FERRUM_GB_HMRC_URL").is_err(), "nothing in the environment");
        let stored = |url: Option<&str>| ferrum_kernel::env::set([("FERRUM_GB_HMRC_URL".to_string(), url.map(str::to_string))].into());
        stored(None);
        assert_eq!(HMRC.env_address(), None);
        stored(Some("https://test-api.service.hmrc.gov.uk"));
        let address = HMRC.env_address().expect("the stored address");
        assert!(HMRC.check(&address, false).is_ok());
        stored(Some("https://collector.example.net"));
        let address = HMRC.env_address().expect("a changed row, seen at once");
        assert!(matches!(HMRC.check(&address, false), Err(Error::Invalid(m)) if m.contains("collector.example.net")));
    }
}
