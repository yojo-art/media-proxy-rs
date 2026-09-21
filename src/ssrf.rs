//! SSRF policy shared by the fail-fast pre-check (`check_url`) and the
//! connect-time enforcement resolver (`ValidatingResolver`).
//!
//! Background (finding #2, DNS rebinding / TOCTOU): validating a hostname's
//! addresses *before* fetching is not sufficient, because the HTTP client
//! resolves the same hostname *again* when connecting. An attacker running
//! the authoritative nameserver (TTL 0) can answer the first query with a
//! public IP and the second with `127.0.0.1`, bypassing even the RFC1918
//! block. `ValidatingResolver` closes this by validating the very addresses
//! the connection is opened to: hyper-util calls the configured
//! `reqwest::dns::Resolve` implementation for every hostname connect (IP
//! literals skip DNS entirely, so they cannot be rebound), which makes the
//! pre-check vs connect resolutions a non-issue.
//!
//! Caveat: this guarantee holds only when reqwest connects directly to the
//! target. When `config.proxy` is set, reqwest instead sends the request to
//! the configured HTTP proxy, and the *proxy* — not this process — resolves
//! and connects to the target host. `ValidatingResolver::resolve` is then
//! only ever called with the proxy's own hostname (see the `proxy_host`
//! guard below), so the target's addresses are never validated here, and the
//! rebinding TOCTOU this module otherwise closes reopens for proxied
//! deployments. In that configuration only `check_url`'s independent
//! pre-check applies to the target; operators enabling `proxy` must ensure
//! the proxy itself enforces an equivalent SSRF policy.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::ConfigFile;

/// DNS キャッシュの TTL と上限。短めの TTL で rebinding 耐性を保ちつつ、
/// check_url と接続時 resolver の双方の DNS クエリを削減する。
/// 失敗（NXDOMAIN 等）も短時間だけ保持し、存在しない攻撃者ドメインへの
/// 反復クエリで DNS を叩き続けられないようにする。
const DNS_CACHE_TTL: Duration = Duration::from_secs(60);
const DNS_CACHE_NEGATIVE_TTL: Duration = Duration::from_secs(10);
/// TTL for a *timeout* result specifically (shorter than
/// `DNS_CACHE_NEGATIVE_TTL`). Code-review finding: a lookup that merely timed
/// out once (transient resolver slowness) previously got the same 10s
/// negative-cache treatment as a genuine resolution failure (NXDOMAIN etc.),
/// so a single slow response could make a perfectly reachable host look
/// "blocked"/unresolvable to every request for the next 10 seconds.
const DNS_CACHE_TIMEOUT_NEGATIVE_TTL: Duration = Duration::from_secs(2);
const DNS_CACHE_CAP: usize = 1024;
/// Upper bound on a single resolution. Without it a blackholed resolver
/// (attacker authoritative NS or broken local resolver) blocks the caller
/// for the OS retry window while holding a `FETCH_SEMAPHORE` permit (M-03).
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
/// Bounds concurrent blocking-pool DNS lookups independently of
/// `FETCH_SEMAPHORE`. Code-review finding: `cached_lookup_host` is called
/// both by `check_url` (which now runs *before* `FETCH_SEMAPHORE` is
/// acquired, M-03) and by `ValidatingResolver`; each underlying
/// `tokio::net::lookup_host` occupies a blocking-pool thread for up to
/// `DNS_LOOKUP_TIMEOUT`, so without a separate cap here, many concurrent
/// requests to distinct slow-to-resolve hosts could occupy an unbounded
/// number of blocking-pool threads (shared with SVG/image encoding) before
/// ever touching the 32-permit `FETCH_SEMAPHORE`.
const DNS_LOOKUP_CONCURRENCY: usize = 128;

enum DnsEntry {
	Positive {
		addrs: Vec<SocketAddr>,
		expires: Instant,
	},
	Negative {
		kind: std::io::ErrorKind,
		expires: Instant,
	},
}

static DNS_CACHE: LazyLock<Mutex<lru::LruCache<String, DnsEntry>>> = LazyLock::new(|| {
	Mutex::new(lru::LruCache::new(
		NonZeroUsize::new(DNS_CACHE_CAP).unwrap(),
	))
});

static DNS_LOOKUP_SEMAPHORE: LazyLock<tokio::sync::Semaphore> =
	LazyLock::new(|| tokio::sync::Semaphore::new(DNS_LOOKUP_CONCURRENCY));

/// ホスト名→アドレスの簡易 LRU キャッシュ付き解決。check_url と
/// ValidatingResolver で共有するため両者の DNS ビューが一致する。
/// ポートは呼び出し側で付け替えるため `:0` で解決する。
pub(crate) async fn cached_lookup_host(host: &str) -> std::io::Result<Vec<SocketAddr>> {
	let key = normalize_host(host);
	{
		let mut cache = DNS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
		let now = Instant::now();
		let hit = match cache.get(&key) {
			Some(DnsEntry::Positive { addrs, expires }) if *expires > now => {
				Some(Ok(addrs.clone()))
			}
			Some(DnsEntry::Negative { kind, expires }) if *expires > now => {
				Some(Err(std::io::Error::new(*kind, "cached dns failure")))
			}
			_ => None,
		};
		if hit.is_none() {
			// 期限切れエントリを残さない。
			cache.pop(&key);
		}
		if let Some(result) = hit {
			return result;
		}
	}
	let _dns_permit = match DNS_LOOKUP_SEMAPHORE.acquire().await {
		Ok(permit) => permit,
		// Never actually closed; fail closed rather than unwrap on principle
		// (finding #3).
		Err(_) => {
			return Err(std::io::Error::new(
				std::io::ErrorKind::Other,
				"dns semaphore closed",
			))
		}
	};
	let lookup = tokio::time::timeout(
		DNS_LOOKUP_TIMEOUT,
		tokio::net::lookup_host(format!("{}:0", host)),
	)
	.await;
	match lookup {
		Ok(Ok(resolved)) => {
			let addrs: Vec<SocketAddr> = resolved.collect();
			let mut cache = DNS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
			if addrs.is_empty() {
				// アドレスなしも接続不能として短期保持する。
				cache.put(
					key,
					DnsEntry::Negative {
						kind: std::io::ErrorKind::Other,
						expires: Instant::now() + DNS_CACHE_NEGATIVE_TTL,
					},
				);
			} else {
				cache.put(
					key,
					DnsEntry::Positive {
						addrs: addrs.clone(),
						expires: Instant::now() + DNS_CACHE_TTL,
					},
				);
			}
			Ok(addrs)
		}
		Ok(Err(e)) => {
			let kind = e.kind();
			let mut cache = DNS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
			cache.put(
				key,
				DnsEntry::Negative {
					kind,
					expires: Instant::now() + DNS_CACHE_NEGATIVE_TTL,
				},
			);
			Err(e)
		}
		Err(_elapsed) => {
			// Cache the timeout as a negative entry so repeat requests to the
			// same host do not each wait again -- but only briefly: a timeout
			// means "slow this time", not "does not exist", so it gets a
			// shorter TTL than a genuine resolution failure above.
			let e = std::io::Error::new(std::io::ErrorKind::TimedOut, "dns lookup timeout");
			let mut cache = DNS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
			cache.put(
				key,
				DnsEntry::Negative {
					kind: e.kind(),
					expires: Instant::now() + DNS_CACHE_TIMEOUT_NEGATIVE_TTL,
				},
			);
			Err(e)
		}
	}
}

/// Lowercase + strip a trailing FQDN dot so `example.com.` matches a
/// `blocked_hosts` entry of `example.com`.
pub(crate) fn normalize_host(host: &str) -> String {
	host.trim_end_matches('.').to_lowercase()
}

/// Suffix match so blocking `example.com` also covers `evil.example.com`.
/// A leading-dot entry (`.example.com`) only matches subdomains, never the apex.
pub(crate) fn is_host_blocked(blocked_hosts: Option<&Vec<String>>, host: &str) -> bool {
	let Some(blocked_hosts) = blocked_hosts else {
		return false;
	};
	let host_lower = normalize_host(host);
	blocked_hosts
		.iter()
		.map(|h| normalize_host(h))
		.any(|entry| {
			if let Some(suffix) = entry.strip_prefix('.') {
				host_lower.ends_with(&format!(".{}", suffix))
			} else {
				host_lower == entry || host_lower.ends_with(&format!(".{}", entry))
			}
		})
}

// Built once: this policy never changes at runtime, so rebuilding it per IP
// per request is pure overhead.
static IPV4_BLOCKED_DEFAULT: std::sync::LazyLock<iprange::IpRange<ipnet::Ipv4Net>> =
	std::sync::LazyLock::new(|| {
		// Default-deny list. Covers RFC1918 + loopback/link-local/metadata
		// (CGNAT/shared)/"this host" (finding #1), plus non-routable/special-use
		// ranges that are still sometimes routed internally (M-04).
		[
			"10.0.0.0/8",
			"172.16.0.0/12",
			"192.168.0.0/16",
			"127.0.0.0/8",
			"169.254.0.0/16",
			"100.64.0.0/10",
			"0.0.0.0/8",
			"192.0.0.0/24",
			"192.0.2.0/24",
			"198.18.0.0/15",
			"198.51.100.0/24",
			"203.0.113.0/24",
			"224.0.0.0/4",
			"240.0.0.0/4",
		]
		.iter()
		.map(|s| s.parse().expect("static CIDR"))
		.collect()
	});

/// Operator-configured CIDRs are validated once at startup
/// ([`validate_network_config`]); at request time invalid entries are ignored
/// instead of panicking (M-05).
fn parse_v4_nets(nets: &Vec<String>) -> iprange::IpRange<ipnet::Ipv4Net> {
	nets.iter().filter_map(|s| s.parse().ok()).collect()
}

/// IPv6 counterpart of [`parse_v4_nets`]; honored for `blocked_networks`
/// (M-04) as well as for operator allow-overrides.
fn parse_v6_nets(nets: &Vec<String>) -> iprange::IpRange<ipnet::Ipv6Net> {
	nets.iter().filter_map(|s| s.parse().ok()).collect()
}

/// Fails fast on a bad `blocked_networks` / `allowed_networks` entry so a
/// typo (or an IPv6 CIDR in a historically IPv4-only field, M-05) is a
/// startup error instead of a per-request panic.
pub(crate) fn validate_network_config(config: &ConfigFile) -> Result<(), String> {
	for (field, nets) in [
		("blocked_networks", &config.blocked_networks),
		("allowed_networks", &config.allowed_networks),
	] {
		if let Some(nets) = nets {
			for net in nets {
				if net.parse::<ipnet::IpNet>().is_err() {
					return Err(format!("{}: invalid CIDR {:?}", field, net));
				}
			}
		}
	}
	Ok(())
}

/// Extracts the IPv4 address carried by IPv6 transition/translation formats
/// so they cannot bypass the IPv4 policy (M-04):
/// IPv4-mapped/-compatible, IPv4-translated, NAT64 (64:ff9b::/96) and
/// 6to4 (2002::/16). Teredo is handled separately (always refused).
fn v6_to_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
	if let Some(v4) = v6.to_ipv4() {
		return Some(v4);
	}
	let seg = v6.segments();
	// NAT64 well-known prefix 64:ff9b::/96
	if seg[0] == 0x0064
		&& seg[1] == 0xff9b
		&& seg[2] == 0
		&& seg[3] == 0
		&& seg[4] == 0
		&& seg[5] == 0
	{
		return Some(std::net::Ipv4Addr::new(
			(seg[6] >> 8) as u8,
			(seg[6] & 0xff) as u8,
			(seg[7] >> 8) as u8,
			(seg[7] & 0xff) as u8,
		));
	}
	// 6to4 2002::/16: next 32 bits hold the IPv4 address.
	if seg[0] == 0x2002 {
		return Some(std::net::Ipv4Addr::new(
			(seg[1] >> 8) as u8,
			(seg[1] & 0xff) as u8,
			(seg[2] >> 8) as u8,
			(seg[2] & 0xff) as u8,
		));
	}
	// IPv4-translated ::ffff:0:0:0/96
	if seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0xffff && seg[5] == 0 {
		return Some(std::net::Ipv4Addr::new(
			(seg[6] >> 8) as u8,
			(seg[6] & 0xff) as u8,
			(seg[7] >> 8) as u8,
			(seg[7] & 0xff) as u8,
		));
	}
	None
}

/// True when a single resolved IP must be refused.
pub(crate) fn is_ip_blocked(config: &ConfigFile, ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(v4) => {
			if let Some(blocked) = config.blocked_networks.as_ref() {
				if parse_v4_nets(blocked).contains(&v4) {
					return true;
				}
			}
			if IPV4_BLOCKED_DEFAULT.contains(&v4) {
				if let Some(allowed) = config.allowed_networks.as_ref() {
					if parse_v4_nets(allowed).contains(&v4) {
						return false;
					}
				}
				return true;
			}
			false
		}
		IpAddr::V6(v6) => {
			// Operator block list is honored for IPv6 too (M-04).
			if let Some(blocked) = config.blocked_networks.as_ref() {
				if parse_v6_nets(blocked).contains(&v6) {
					return true;
				}
			}
			// Teredo (2001:0000::/32) can carry an IPv4 address and is never
			// legitimate for this proxy; refuse it unconditionally (M-04).
			let seg = v6.segments();
			if seg[0] == 0x2001 && seg[1] == 0x0000 {
				return true;
			}
			// IPv4-mapped/-compatible/-translated, NAT64, 6to4: apply the
			// IPv4 policy (M-04).
			if let Some(v4) = v6_to_ipv4(&v6) {
				return is_ip_blocked(config, IpAddr::V4(v4));
			}
			// Loopback ::1 / unspecified :: / ULA fc00::/7 have an
			// `allowed_networks` override, mirroring the IPv4 policy.
			if v6.is_multicast()
				|| v6.is_unicast_link_local()
				|| v6.is_loopback()
				|| v6.is_unspecified()
				|| v6.is_unique_local()
			{
				if let Some(allowed) = config.allowed_networks.as_ref() {
					if parse_v6_nets(allowed).contains(&v6) {
						return false;
					}
				}
				return true;
			}
			false
		}
	}
}

/// Fail-closed validation of a full resolution result: refuses when **any**
/// address is blocked (a mixed public+internal answer must not connect).
pub(crate) fn validate_resolved_ips(config: &ConfigFile, ips: &[SocketAddr]) -> Result<(), String> {
	if ips.is_empty() {
		return Err("Blocked address".to_owned());
	}
	for ip in ips {
		if is_ip_blocked(config, ip.ip()) {
			return Err("Blocked address".to_owned());
		}
	}
	Ok(())
}

/// `reqwest::dns::Resolve` implementation that validates the addresses a
/// connection is actually opened to (finding #2, DNS rebinding / TOCTOU).
#[derive(Clone)]
pub(crate) struct ValidatingResolver {
	config: Arc<ConfigFile>,
	/// Hostname of the configured egress proxy, if any. With an HTTP proxy the
	/// target host is never resolved locally; only the proxy host goes through
	/// this resolver, and it must not be subjected to the SSRF policy.
	proxy_host: Option<String>,
}

/// Parses a `config.proxy` string the same permissive way reqwest's own
/// `Proxy::http`/`Proxy::all` do internally (their crate-private `IntoProxy`
/// retries with an `http://` prefix when the string has no scheme).
///
/// Code-review finding: a bare `reqwest::Url::parse` (as this used to be)
/// fails outright on a schemeless value like `"10.0.0.5:3128"` -- a natural
/// way to write it -- while `reqwest::Proxy::http`/`Proxy::all` still accept
/// it via that fallback. If `proxy_host` derivation disagreed with what the
/// client actually proxies through, the "proxy's own hostname" bypass branch
/// in `ValidatingResolver::resolve` would never trigger, and the proxy's own
/// (often RFC1918) address would run through the normal SSRF check instead
/// and get rejected as "Blocked address", silently breaking every proxied
/// fetch with no indication the root cause was the missing scheme.
fn parse_proxy_url(url: &str) -> Option<reqwest::Url> {
	reqwest::Url::parse(url)
		.ok()
		.or_else(|| reqwest::Url::parse(&format!("http://{}", url)).ok())
}

impl ValidatingResolver {
	pub(crate) fn new(config: Arc<ConfigFile>) -> Self {
		let proxy_host = config
			.proxy
			.as_ref()
			.and_then(|url| parse_proxy_url(url))
			.and_then(|u| u.host_str().map(|h| normalize_host(h)));
		Self { config, proxy_host }
	}
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl reqwest::dns::Resolve for ValidatingResolver {
	fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
		let host = name.as_str().to_owned();
		let config = self.config.clone();
		let proxy_host = self.proxy_host.clone();
		Box::pin(async move {
			// The egress proxy itself is not a fetch target: resolve plainly.
			// NB: this is also exactly where the module's connect-time SSRF
			// guarantee stops covering the real target when a proxy is
			// configured -- see the module doc's "Caveat" section above.
			if let Some(proxy_host) = proxy_host.as_ref() {
				if normalize_host(&host) == *proxy_host {
					let ips: Vec<SocketAddr> = cached_lookup_host(&host)
						.await
						.map_err(|e| Box::new(e) as BoxError)?;
					let addrs: reqwest::dns::Addrs = Box::new(ips.into_iter());
					return Ok(addrs);
				}
			}
			if is_host_blocked(config.blocked_hosts.as_ref(), &host) {
				let err: BoxError = "Blocked address".into();
				return Err(err);
			}
			// Port 0: the connector replaces it with the URL's port.
			let ips: Vec<SocketAddr> = cached_lookup_host(&host)
				.await
				.map_err(|e| Box::new(e) as BoxError)?;
			// Fail closed on any blocked address (same policy as check_url).
			if let Err(s) = validate_resolved_ips(&config, &ips) {
				let err: BoxError = s.into();
				return Err(err);
			}
			let addrs: reqwest::dns::Addrs = Box::new(ips.into_iter());
			Ok(addrs)
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ConfigFile, FilterType};
	use reqwest::dns::Resolve;
	use std::sync::Arc;

	fn test_config() -> ConfigFile {
		ConfigFile {
			bind_addr: "127.0.0.1:0".to_owned(),
			timeout: 1000,
			user_agent: "test".to_owned(),
			max_size: 1024,
			proxy: None,
			filter_type: FilterType::Triangle,
			max_pixels: 64,
			append_headers: vec![],
			load_system_fonts: false,
			webp_quality: 75.0,
			encode_avif: false,
			allowed_networks: None,
			blocked_networks: None,
			blocked_hosts: None,
			unix_socket_permissions: None,
		}
	}

	fn v4(s: &str) -> IpAddr {
		s.parse().unwrap()
	}

	fn v6(s: &str) -> IpAddr {
		s.parse().unwrap()
	}

	#[test]
	fn default_deny_ranges() {
		let c = test_config();
		// Must be blocked by default.
		for ip in [
			"127.0.0.1",
			"10.1.2.3",
			"172.16.5.4",
			"192.168.1.1",
			"169.254.169.254",
			"100.64.0.1",
			"0.0.0.0",
		] {
			assert!(is_ip_blocked(&c, v4(ip)), "{} should be blocked", ip);
		}
		for ip in [
			"::1",
			"::",
			"fe80::1",
			"fc00::1",
			"ff02::1",
			"::ffff:127.0.0.1",
			"::ffff:169.254.169.254",
		] {
			assert!(is_ip_blocked(&c, v6(ip)), "{} should be blocked", ip);
		}
		// Public addresses pass.
		for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
			assert!(!is_ip_blocked(&c, v4(ip)), "{} should pass", ip);
		}
		assert!(
			!is_ip_blocked(&c, v6("2606:4700:4700::1111")),
			"public v6 should pass"
		);
		// IPv4-mapped public passes.
		assert!(
			!is_ip_blocked(&c, v6("::ffff:8.8.8.8")),
			"mapped public should pass"
		);
	}

	#[test]
	fn allow_list_opens_default_deny() {
		let mut c = test_config();
		c.allowed_networks = Some(vec!["192.168.0.0/16".to_owned()]);
		assert!(!is_ip_blocked(&c, v4("192.168.1.10")));
		assert!(is_ip_blocked(&c, v4("10.0.0.5")));
	}

	#[test]
	fn blocked_networks_win() {
		let mut c = test_config();
		c.blocked_networks = Some(vec!["8.8.8.0/24".to_owned()]);
		assert!(is_ip_blocked(&c, v4("8.8.8.8")));
		assert!(!is_ip_blocked(&c, v4("1.1.1.1")));
	}

	#[test]
	fn ipv6_transition_ranges_are_blocked() {
		let c = test_config();
		// NAT64 / 6to4 / Teredo / IPv4-translated must not bypass the IPv4 policy.
		for ip in [
			"64:ff9b::7f00:1",
			"64:ff9b::a9fe:a9fe",
			"2002:7f00:1::",
			"2002:a9fe:a9fe::",
			"2001::1",
			"::ffff:0:7f00:1",
		] {
			assert!(is_ip_blocked(&c, v6(ip)), "{} should be blocked", ip);
		}
		// 6to4 wrapping a public IPv4 (8.8.8.8) stays allowed.
		assert!(
			!is_ip_blocked(&c, v6("2002:808:808::")),
			"6to4 of 8.8.8.8 should pass"
		);
	}

	#[test]
	fn ipv6_block_list_is_honored() {
		let mut c = test_config();
		c.blocked_networks = Some(vec!["2606:4700:4700::/48".to_owned()]);
		assert!(is_ip_blocked(&c, v6("2606:4700:4700::1111")));
		assert!(!is_ip_blocked(&c, v6("2606:1111::1")));
	}

	#[test]
	fn invalid_network_config_is_rejected_at_startup() {
		let mut c = test_config();
		c.blocked_networks = Some(vec!["10.0.0.0/99".to_owned()]);
		assert!(validate_network_config(&c).is_err());
		// IPv6 CIDRs are now valid policy entries (M-04), not a panic (M-05).
		c.blocked_networks = Some(vec!["2001:db8::/32".to_owned()]);
		assert!(validate_network_config(&c).is_ok());
	}

	#[test]
	fn mixed_answer_is_refused() {
		let c = test_config();
		let ips = vec![
			SocketAddr::new(v4("93.184.216.34"), 0),
			SocketAddr::new(v4("127.0.0.1"), 0),
		];
		assert!(validate_resolved_ips(&c, &ips).is_err());
		assert!(validate_resolved_ips(&c, &[]).is_err());
		let ok = vec![SocketAddr::new(v4("93.184.216.34"), 0)];
		assert!(validate_resolved_ips(&c, &ok).is_ok());
	}

	#[test]
	fn host_suffix_match() {
		let blocked = Some(vec!["example.com".to_owned()]);
		assert!(is_host_blocked(blocked.as_ref(), "example.com"));
		assert!(is_host_blocked(blocked.as_ref(), "evil.example.com"));
		assert!(is_host_blocked(blocked.as_ref(), "EXAMPLE.COM."));
		assert!(!is_host_blocked(blocked.as_ref(), "example.com.evil.org"));
		assert!(!is_host_blocked(blocked.as_ref(), "notexample.com"));
		assert!(!is_host_blocked(None, "example.com"));
		let dot = Some(vec![".example.com".to_owned()]);
		assert!(is_host_blocked(dot.as_ref(), "evil.example.com"));
		assert!(!is_host_blocked(dot.as_ref(), "example.com"));
	}

	#[test]
	fn dns_cache_returns_consistent_view() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		rt.block_on(async {
			let a = cached_lookup_host("localhost").await.unwrap();
			// Case + trailing-dot normalization hits the same entry.
			let b = cached_lookup_host("LOCALHOST.").await.unwrap();
			assert_eq!(a, b);
			assert!(!a.is_empty());
		});
	}

	#[test]
	fn dns_cache_negative() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		rt.block_on(async {
			// .invalid は RFC 2606 で解決不能が保証される。
			let name = "nonexistent-name-for-test.invalid";
			assert!(cached_lookup_host(name).await.is_err());
			// 2 回目はネガティブキャッシュから即時 Err。
			assert!(cached_lookup_host(name).await.is_err());
		});
	}

	#[test]
	fn resolver_blocks_rebinding_target() {
		// localhost must fail at resolve time even though "resolving" succeeds.
		let config = Arc::new(test_config());
		let r = ValidatingResolver::new(config);
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		rt.block_on(async {
			let name: reqwest::dns::Name = "localhost".parse().unwrap();
			assert!(r.resolve(name).await.is_err());
		});
	}

	#[test]
	fn proxy_url_accepts_schemeless_value() {
		// Mirrors reqwest::Proxy::http/all's own fallback (code-review finding)
		// so ValidatingResolver's proxy_host agrees with what the client
		// actually proxies through.
		let u = parse_proxy_url("10.0.0.5:3128").expect("schemeless proxy value should parse");
		assert_eq!(u.host_str(), Some("10.0.0.5"));
		let u =
			parse_proxy_url("http://10.0.0.5:3128").expect("explicit scheme should still parse");
		assert_eq!(u.host_str(), Some("10.0.0.5"));
	}

	#[test]
	fn resolver_treats_schemeless_proxy_host_as_proxy() {
		// 127.0.0.1 would otherwise be blocked by the default-deny list;
		// this only passes if the schemeless proxy value is recognized as
		// the proxy's own host (code-review finding).
		let mut c = test_config();
		c.proxy = Some("127.0.0.1:1".to_owned());
		let config = Arc::new(c);
		let r = ValidatingResolver::new(config);
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		rt.block_on(async {
			let name: reqwest::dns::Name = "127.0.0.1".parse().unwrap();
			assert!(r.resolve(name).await.is_ok());
		});
	}
}
