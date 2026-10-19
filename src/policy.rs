use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::{Ipv4Net, Ipv6Net};
use url::{Host, Url};

use crate::api::{Error, ErrorCode};
use crate::config::Limits;

#[cfg(test)]
mod tests;

#[derive(Clone, Default)]
pub(crate) struct DomainFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl DomainFilter {
    pub(crate) fn new(
        include: &[String],
        exclude: &[String],
        limits: &Limits,
    ) -> Result<Self, Error> {
        if include.len().saturating_add(exclude.len()) > limits.max_domain_filters {
            return Err(Error::new(
                ErrorCode::InvalidInput,
                "Too many domain filters",
            ));
        }
        let normalize = |domains: &[String]| {
            domains
                .iter()
                .map(|domain| {
                    if domain.len() > limits.max_url_bytes {
                        return Err(invalid_filter());
                    }
                    normalize_host(domain).map_err(|_| invalid_filter())
                })
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            include: normalize(include)?,
            exclude: normalize(exclude)?,
        })
    }

    pub(crate) fn check(&self, url: &Url) -> Result<(), Error> {
        let host = normalize_host(url.host_str().ok_or_else(invalid_url)?)?;
        if self
            .exclude
            .iter()
            .any(|domain| matches_domain(&host, domain))
            || (!self.include.is_empty()
                && !self
                    .include
                    .iter()
                    .any(|domain| matches_domain(&host, domain)))
        {
            return Err(Error::new(
                ErrorCode::DomainFiltered,
                "Destination does not satisfy the domain filters",
            ));
        }
        Ok(())
    }
}

pub(crate) fn parse_url(input: &str, limits: &Limits) -> Result<Url, Error> {
    if input.is_empty()
        || input.len() > limits.max_url_bytes
        || input.chars().any(|c| c.is_control() || c.is_whitespace())
        || input.contains('\\')
    {
        return Err(invalid_url());
    }
    let mut url = Url::parse(input).map_err(|_| invalid_url())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "Only HTTP and HTTPS URLs are supported",
        ));
    }
    let authority = input
        .get(url.scheme().len()..)
        .and_then(|rest| rest.strip_prefix("://"))
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .filter(|authority| !authority.is_empty())
        .ok_or_else(invalid_url)?;
    if authority.contains('@') || !url.username().is_empty() || url.password().is_some() {
        return Err(Error::new(
            ErrorCode::InvalidInput,
            "URL credentials are not allowed",
        ));
    }
    let expected_port = if url.scheme() == "https" { 443 } else { 80 };
    if url.port_or_known_default() != Some(expected_port) {
        return Err(Error::new(
            ErrorCode::BlockedDestination,
            "Only the standard port for the URL scheme is allowed",
        ));
    }
    match url.host().ok_or_else(invalid_url)? {
        Host::Ipv4(ip) => validate_ip(IpAddr::V4(ip))?,
        Host::Ipv6(ip) => validate_ip(IpAddr::V6(ip))?,
        Host::Domain(host) => {
            let host = normalize_host(host)?;
            if !host.contains('.')
                || [
                    "localhost",
                    "local",
                    "localdomain",
                    "internal",
                    "intranet",
                    "lan",
                    "home",
                    "corp",
                    "arpa",
                    "test",
                    "invalid",
                    "example",
                    "onion",
                    "alt",
                    "ts.net",
                    "metadata.google.com",
                    "metadata.goog",
                ]
                .iter()
                .any(|suffix| matches_domain(&host, suffix))
            {
                return Err(blocked_destination());
            }
            url.set_host(Some(&host)).map_err(|_| invalid_url())?;
        }
    }
    url.set_fragment(None);
    if url.as_str().len() > limits.max_url_bytes {
        return Err(invalid_url());
    }
    Ok(url)
}

pub(crate) fn validate_ip(ip: IpAddr) -> Result<(), Error> {
    let allowed = match ip {
        IpAddr::V4(ip) => ![
            (Ipv4Addr::new(0, 0, 0, 0), 8),
            (Ipv4Addr::new(10, 0, 0, 0), 8),
            (Ipv4Addr::new(100, 64, 0, 0), 10),
            (Ipv4Addr::new(127, 0, 0, 0), 8),
            (Ipv4Addr::new(169, 254, 0, 0), 16),
            (Ipv4Addr::new(172, 16, 0, 0), 12),
            (Ipv4Addr::new(192, 0, 0, 0), 24),
            (Ipv4Addr::new(192, 0, 2, 0), 24),
            (Ipv4Addr::new(192, 31, 196, 0), 24),
            (Ipv4Addr::new(192, 52, 193, 0), 24),
            (Ipv4Addr::new(192, 88, 99, 0), 24),
            (Ipv4Addr::new(192, 168, 0, 0), 16),
            (Ipv4Addr::new(192, 175, 48, 0), 24),
            (Ipv4Addr::new(198, 18, 0, 0), 15),
            (Ipv4Addr::new(198, 51, 100, 0), 24),
            (Ipv4Addr::new(203, 0, 113, 0), 24),
            (Ipv4Addr::new(224, 0, 0, 0), 3),
            (Ipv4Addr::new(168, 63, 129, 16), 32),
        ]
        .iter()
        .any(|(network, prefix)| Ipv4Net::new_assert(*network, *prefix).contains(&ip)),
        IpAddr::V6(ip) => {
            let allocated = [
                (0x2001, 0x0200, 23),
                (0x2001, 0x0400, 23),
                (0x2001, 0x0600, 23),
                (0x2001, 0x0800, 22),
                (0x2001, 0x0c00, 23),
                (0x2001, 0x0e00, 23),
                (0x2001, 0x1200, 23),
                (0x2001, 0x1400, 22),
                (0x2001, 0x1800, 23),
                (0x2001, 0x1a00, 23),
                (0x2001, 0x1c00, 22),
                (0x2001, 0x2000, 19),
                (0x2001, 0x4000, 23),
                (0x2001, 0x4200, 23),
                (0x2001, 0x4400, 23),
                (0x2001, 0x4600, 23),
                (0x2001, 0x4800, 23),
                (0x2001, 0x4a00, 23),
                (0x2001, 0x4c00, 23),
                (0x2001, 0x5000, 20),
                (0x2001, 0x8000, 19),
                (0x2001, 0xa000, 20),
                (0x2001, 0xb000, 20),
                (0x2003, 0, 18),
                (0x2400, 0, 12),
                (0x2410, 0, 12),
                (0x2600, 0, 12),
                (0x2610, 0, 23),
                (0x2620, 0, 23),
                (0x2630, 0, 12),
                (0x2800, 0, 12),
                (0x2a00, 0, 12),
                (0x2a10, 0, 12),
                (0x2c00, 0, 12),
            ]
            .iter()
            .any(|(first, second, prefix)| {
                Ipv6Net::new_assert(Ipv6Addr::new(*first, *second, 0, 0, 0, 0, 0, 0), *prefix)
                    .contains(&ip)
            });
            allocated
                && !Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32)
                    .contains(&ip)
                && !Ipv6Net::new_assert(Ipv6Addr::new(0x2620, 0x4f, 0x8000, 0, 0, 0, 0, 0), 48)
                    .contains(&ip)
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(blocked_destination())
    }
}

fn normalize_host(input: &str) -> Result<String, Error> {
    if input.is_empty()
        || input.len() > 1024
        || input.contains(['/', '\\', '@', '?', '#', '%'])
        || input.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(invalid_url());
    }
    match Host::parse(input).map_err(|_| invalid_url())? {
        Host::Domain(domain) => {
            let domain = domain.strip_suffix('.').unwrap_or(&domain);
            if domain.is_empty()
                || domain.len() > 253
                || domain.split('.').any(|label| {
                    label.is_empty()
                        || label.len() > 63
                        || label.starts_with('-')
                        || label.ends_with('-')
                        || !label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
            {
                return Err(invalid_url());
            }
            Ok(domain.to_ascii_lowercase())
        }
        Host::Ipv4(ip) => Ok(ip.to_string()),
        Host::Ipv6(ip) => Ok(format!("[{ip}]")),
    }
}

fn matches_domain(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn invalid_url() -> Error {
    Error::new(
        ErrorCode::InvalidInput,
        "Invalid or oversized URL hostname or syntax",
    )
}

fn invalid_filter() -> Error {
    Error::new(
        ErrorCode::InvalidInput,
        "Domain filters must contain hostnames or IP literals without URLs, ports, or wildcards",
    )
}

fn blocked_destination() -> Error {
    Error::new(
        ErrorCode::BlockedDestination,
        "Destination is not an allowed public address",
    )
}
