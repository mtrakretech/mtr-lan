use anyhow::Result;
use std::{error::Error, net::IpAddr};

use trust_dns_resolver::TokioAsyncResolver;

pub struct DnsResolver {
    resolver: TokioAsyncResolver,
}
impl DnsResolver {
    pub(crate) async fn new() -> Result<Self> {
        let resolver = TokioAsyncResolver::tokio_from_system_conf()?;
        Ok(Self { resolver })
    }

    pub(crate) async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, Box<dyn Error>> {
        log::info!("resolving {host} ...");
        // Windows' native resolver also handles PC names through configured LAN name
        // services; a DNS-only resolver frequently misses WORKSTATION-style names.
        if let Ok(result) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::net::lookup_host((host, 0)),
        )
        .await
        {
            if let Ok(addresses) = result {
                let ips: std::collections::HashSet<_> =
                    addresses.map(|a| a.ip()).filter(IpAddr::is_ipv4).collect();
                if !ips.is_empty() {
                    return Ok(ips.into_iter().collect());
                }
            }
        }
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            self.resolver.lookup_ip(host),
        )
        .await??;
        for ip in response.iter() {
            log::info!("{host}: adding ip {ip}");
        }
        Ok(response.iter().filter(IpAddr::is_ipv4).collect())
    }
}
