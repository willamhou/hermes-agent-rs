use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use reqwest::{Client, Response, Url, redirect::Policy};

pub fn build_safe_client() -> std::result::Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(Policy::none())
        .build()
}

pub async fn fetch_with_redirects(
    client: &Client,
    start: &str,
    max_redirects: usize,
) -> std::result::Result<(Url, Response), String> {
    let mut current = Url::parse(start).map_err(|e| format!("invalid URL: {e}"))?;

    for hop in 0..=max_redirects {
        validate_remote_url(&current).await?;

        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        if response.status().is_redirection() {
            if hop == max_redirects {
                return Err("too many redirects".to_string());
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| "redirect missing Location header".to_string())?
                .to_str()
                .map_err(|e| format!("invalid redirect Location header: {e}"))?;

            current = current
                .join(location)
                .map_err(|e| format!("invalid redirect target: {e}"))?;
            continue;
        }

        return Ok((current, response));
    }

    Err("too many redirects".to_string())
}

pub async fn validate_remote_url(url: &Url) -> std::result::Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported URL scheme: {other}")),
    }

    let host = url
        .host_str()
        .ok_or_else(|| "URL is missing a host".to_string())?
        .to_lowercase();

    if is_blocked_host(&host) {
        return Err(format!("blocked host: {host}"));
    }

    let port = url.port_or_known_default().unwrap_or(80);
    let resolved = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("failed to resolve host '{host}': {e}"))?
        .collect::<Vec<_>>();

    if resolved.is_empty() {
        return Err(format!("host '{host}' resolved to no addresses"));
    }

    for addr in resolved {
        if is_blocked_ip(addr.ip()) {
            return Err(format!("blocked address: {}", addr.ip()));
        }
    }

    Ok(())
}

fn is_blocked_host(host: &str) -> bool {
    const BLOCKED_HOSTS: &[&str] = &[
        "localhost",
        "metadata.google.internal",
        "metadata",
        "169.254.169.254",
    ];

    BLOCKED_HOSTS.contains(&host) || host.ends_with(".localhost")
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_blocked_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_blocked_ipv6(ipv6),
    }
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_unspecified()
        || is_carrier_grade_nat(ip)
        || is_benchmarking_range(ip)
}

fn is_carrier_grade_nat(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn is_benchmarking_range(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 198 && (18..=19).contains(&octets[1])
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ipv4_is_blocked() {
        assert!(is_blocked_ip("10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("192.168.1.1".parse().unwrap()));
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn localhost_host_is_blocked() {
        assert!(is_blocked_host("localhost"));
        assert!(is_blocked_host("api.localhost"));
        assert!(!is_blocked_host("example.com"));
    }
}
