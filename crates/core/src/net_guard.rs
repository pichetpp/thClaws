//! SSRF guard for the runner-side fetch tools (`WebFetch`, `WebScrape`,
//! `FetchImages`).
//!
//! A page the agent extracts is untrusted content, and in multiuser mode tool
//! calls are force-auto-approved — so an image/link like
//! `http://169.254.169.254/latest/meta-data/…` or `http://192.168.1.1/…` could
//! otherwise pivot the runner into its own network / cloud metadata. This
//! module refuses any URL whose host is (or resolves to) a private, loopback,
//! link-local, ULA, or otherwise non-public address, and rejects non-http(s)
//! schemes.
//!
//! Opt out with `THCLAWS_ALLOW_PRIVATE_FETCH=1` (e.g. local dev fetching your
//! own `localhost` server).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

/// Escape hatch for users who legitimately fetch localhost / LAN.
pub fn allow_private() -> bool {
    std::env::var("THCLAWS_ALLOW_PRIVATE_FETCH").ok().as_deref() == Some("1")
}

/// `Ok(())` if the URL is safe to fetch from the runner; `Err(reason)` otherwise.
/// Resolves hostnames and checks **every** resolved address, so a name pointing
/// at a private IP is blocked too. (DNS-rebinding between this check and the
/// actual connect is out of scope — this stops the common cases: literal
/// private IPs, `localhost`, and names resolving to internal ranges.)
pub async fn guard(url_str: &str) -> Result<(), String> {
    if allow_private() {
        return Ok(());
    }
    let url = url::Url::parse(url_str).map_err(|e| format!("bad url: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme '{s}' is not fetchable")),
    }
    let host = url
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?;

    // Literal IP in the URL — check directly, no DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_ip(ip);
    }
    // Obvious loopback names before paying for DNS.
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err(blocked_msg("localhost"));
    }

    let host_owned = host.to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    // to_socket_addrs blocks on DNS — keep it off the async worker.
    let addrs = tokio::task::spawn_blocking(move || {
        (host_owned.as_str(), port)
            .to_socket_addrs()
            .map(|it| it.map(|sa| sa.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|e| format!("resolve join error: {e}"))?
    .map_err(|e| format!("resolve {host}: {e}"))?;

    if addrs.is_empty() {
        return Err(format!("{host} did not resolve"));
    }
    for ip in addrs {
        check_ip(ip)?;
    }
    Ok(())
}

fn blocked_msg(what: &str) -> String {
    format!(
        "refuses to fetch {what} — non-public target \
         (set THCLAWS_ALLOW_PRIVATE_FETCH=1 to allow local/LAN fetches)"
    )
}

/// True → blocked. Covers loopback, private, link-local (incl. 169.254 cloud
/// metadata), CGNAT, ULA, unspecified/broadcast/documentation, and
/// IPv4-mapped IPv6.
fn check_ip(ip: IpAddr) -> Result<(), String> {
    let blocked = match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                is_blocked_v4(mapped)
            } else {
                is_blocked_v6(v6)
            }
        }
    };
    if blocked {
        Err(blocked_msg(&ip.to_string()))
    } else {
        Ok(())
    }
}

fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local() // 169.254.0.0/16 — AWS/GCP/Azure metadata
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10 CGNAT
        || o[0] == 0 // 0.0.0.0/8
}

fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    let seg = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || (seg[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
        || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(s: &str) -> bool {
        check_ip(s.parse().unwrap()).is_err()
    }

    #[test]
    fn blocks_internal_addresses() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254", // cloud metadata
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1", // CGNAT
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:169.254.169.254",
        ] {
            assert!(blocked(ip), "should block {ip}");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:2800:220::1"] {
            assert!(!blocked(ip), "should allow {ip}");
        }
    }

    #[tokio::test]
    async fn guard_rejects_scheme_and_literal_private() {
        assert!(guard("ftp://example.com/x").await.is_err());
        assert!(guard("http://127.0.0.1/x").await.is_err());
        assert!(guard("http://169.254.169.254/meta").await.is_err());
        assert!(guard("http://localhost:3000/x").await.is_err());
    }
}
