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

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::ConfigFile;

/// DNS キャッシュの TTL と上限。短めの TTL で rebinding 耐性を保ちつつ、
/// check_url と接続時 resolver の双方の DNS クエリを削減する。
/// 失敗（NXDOMAIN 等）も短時間だけ保持し、存在しない攻撃者ドメインへの
/// 反復クエリで DNS を叩き続けられないようにする。
const DNS_CACHE_TTL:Duration=Duration::from_secs(60);
const DNS_CACHE_NEGATIVE_TTL:Duration=Duration::from_secs(10);
const DNS_CACHE_CAP:usize=1024;

enum DnsEntry{
	Positive{addrs:Vec<SocketAddr>,expires:Instant},
	Negative{kind:std::io::ErrorKind,expires:Instant},
}

static DNS_CACHE:LazyLock<Mutex<lru::LruCache<String,DnsEntry>>>=LazyLock::new(||{
	Mutex::new(lru::LruCache::new(NonZeroUsize::new(DNS_CACHE_CAP).unwrap()))
});

/// ホスト名→アドレスの簡易 LRU キャッシュ付き解決。check_url と
/// ValidatingResolver で共有するため両者の DNS ビューが一致する。
/// ポートは呼び出し側で付け替えるため `:0` で解決する。
pub(crate) async fn cached_lookup_host(host:&str)->std::io::Result<Vec<SocketAddr>>{
	let key=normalize_host(host);
	{
		let mut cache=DNS_CACHE.lock().unwrap_or_else(|e|e.into_inner());
		let now=Instant::now();
		let hit=match cache.get(&key){
			Some(DnsEntry::Positive{addrs,expires}) if *expires>now=>Some(Ok(addrs.clone())),
			Some(DnsEntry::Negative{kind,expires}) if *expires>now=>{
				Some(Err(std::io::Error::new(*kind,"cached dns failure")))
			},
			_=>None,
		};
		if hit.is_none(){
			// 期限切れエントリを残さない。
			cache.pop(&key);
		}
		if let Some(result)=hit{
			return result;
		}
	}
	match tokio::net::lookup_host(format!("{}:0",host)).await{
		Ok(resolved)=>{
			let addrs:Vec<SocketAddr>=resolved.collect();
			let mut cache=DNS_CACHE.lock().unwrap_or_else(|e|e.into_inner());
			if addrs.is_empty(){
				// アドレスなしも接続不能として短期保持する。
				cache.put(key,DnsEntry::Negative{kind:std::io::ErrorKind::Other,expires:Instant::now()+DNS_CACHE_NEGATIVE_TTL});
			}else{
				cache.put(key,DnsEntry::Positive{addrs:addrs.clone(),expires:Instant::now()+DNS_CACHE_TTL});
			}
			Ok(addrs)
		},
		Err(e)=>{
			let kind=e.kind();
			let mut cache=DNS_CACHE.lock().unwrap_or_else(|e|e.into_inner());
			cache.put(key,DnsEntry::Negative{kind,expires:Instant::now()+DNS_CACHE_NEGATIVE_TTL});
			Err(e)
		},
	}
}

/// Lowercase + strip a trailing FQDN dot so `example.com.` matches a
/// `blocked_hosts` entry of `example.com`.
pub(crate) fn normalize_host(host:&str)->String{
	host.trim_end_matches('.').to_lowercase()
}

/// Suffix match so blocking `example.com` also covers `evil.example.com`.
/// A leading-dot entry (`.example.com`) only matches subdomains, never the apex.
pub(crate) fn is_host_blocked(blocked_hosts:Option<&Vec<String>>,host:&str)->bool{
	let Some(blocked_hosts)=blocked_hosts else{
		return false;
	};
	let host_lower=normalize_host(host);
	blocked_hosts.iter().map(|h|normalize_host(h)).any(|entry|{
		if let Some(suffix)=entry.strip_prefix('.'){
			host_lower.ends_with(&format!(".{}",suffix))
		}else{
			host_lower==entry||host_lower.ends_with(&format!(".{}",entry))
		}
	})
}

fn ipv4_blocked_default()->iprange::IpRange<ipnet::Ipv4Net>{
	// Default-deny list. Covers RFC1918 + loopback/link-local/metadata
	// (CGNAT/shared)/"this host". See finding #1.
	[
		"10.0.0.0/8",
		"172.16.0.0/12",
		"192.168.0.0/16",
		"127.0.0.0/8",
		"169.254.0.0/16",
		"100.64.0.0/10",
		"0.0.0.0/8",
	]
		.iter()
		.map(|s| s.parse().unwrap())
		.collect()
}

fn parse_v4_nets(nets:&Vec<String>)->iprange::IpRange<ipnet::Ipv4Net>{
	nets.iter()
	.map(|s| s.parse().unwrap())
	.collect()
}

/// True when a single resolved IP must be refused.
pub(crate) fn is_ip_blocked(config:&ConfigFile,ip:IpAddr)->bool{
	match ip{
		IpAddr::V4(v4)=>{
			if let Some(blocked)=config.blocked_networks.as_ref(){
				if parse_v4_nets(blocked).contains(&v4){
					return true;
				}
			}
			if ipv4_blocked_default().contains(&v4){
				if let Some(allowed)=config.allowed_networks.as_ref(){
					if parse_v4_nets(allowed).contains(&v4){
						return false;
					}
				}
				return true;
			}
			false
		},
		IpAddr::V6(v6)=>{
			// Loopback ::1 / unspecified :: / ULA fc00::/7 have no allow-override;
			// operator allow-lists are IPv4-only (allowed_networks: Vec<Ipv4Net>).
			if v6.is_multicast()||v6.is_unicast_link_local()||v6.is_loopback()||v6.is_unspecified()||v6.is_unique_local(){
				return true;
			}
			// IPv4-mapped (e.g. ::ffff:169.254.169.254): apply the IPv4 policy.
			if let Some(mapped)=v6.to_ipv4_mapped(){
				return is_ip_blocked(config,IpAddr::V4(mapped));
			}
			false
		},
	}
}

/// Fail-closed validation of a full resolution result: refuses when **any**
/// address is blocked (a mixed public+internal answer must not connect).
pub(crate) fn validate_resolved_ips(config:&ConfigFile,ips:&[SocketAddr])->Result<(),String>{
	if ips.is_empty(){
		return Err("Blocked address".to_owned());
	}
	for ip in ips{
		if is_ip_blocked(config,ip.ip()){
			return Err("Blocked address".to_owned());
		}
	}
	Ok(())
}

/// `reqwest::dns::Resolve` implementation that validates the addresses a
/// connection is actually opened to (finding #2, DNS rebinding / TOCTOU).
#[derive(Clone)]
pub(crate) struct ValidatingResolver{
	config:Arc<ConfigFile>,
	/// Hostname of the configured egress proxy, if any. With an HTTP proxy the
	/// target host is never resolved locally; only the proxy host goes through
	/// this resolver, and it must not be subjected to the SSRF policy.
	proxy_host:Option<String>,
}

impl ValidatingResolver{
	pub(crate) fn new(config:Arc<ConfigFile>)->Self{
		let proxy_host=config.proxy.as_ref()
			.and_then(|url|reqwest::Url::parse(url).ok())
			.and_then(|u|u.host_str().map(|h|normalize_host(h)));
		Self{config,proxy_host}
	}
}

type BoxError=Box<dyn std::error::Error+Send+Sync>;

impl reqwest::dns::Resolve for ValidatingResolver{
	fn resolve(&self,name:reqwest::dns::Name)->reqwest::dns::Resolving{
		let host=name.as_str().to_owned();
		let config=self.config.clone();
		let proxy_host=self.proxy_host.clone();
		Box::pin(async move{
			// The egress proxy itself is not a fetch target: resolve plainly.
			if let Some(proxy_host)=proxy_host.as_ref(){
				if normalize_host(&host)==*proxy_host{
					let ips:Vec<SocketAddr>=cached_lookup_host(&host).await
						.map_err(|e|Box::new(e) as BoxError)?;
					let addrs:reqwest::dns::Addrs=Box::new(ips.into_iter());
					return Ok(addrs);
				}
			}
			if is_host_blocked(config.blocked_hosts.as_ref(),&host){
				let err:BoxError="Blocked address".into();
				return Err(err);
			}
			// Port 0: the connector replaces it with the URL's port.
			let ips:Vec<SocketAddr>=cached_lookup_host(&host).await
				.map_err(|e|Box::new(e) as BoxError)?;
			// Fail closed on any blocked address (same policy as check_url).
			if let Err(s)=validate_resolved_ips(&config,&ips){
				let err:BoxError=s.into();
				return Err(err);
			}
			let addrs:reqwest::dns::Addrs=Box::new(ips.into_iter());
			Ok(addrs)
		})
	}
}

#[cfg(test)]
mod tests{
	use super::*;
	use reqwest::dns::Resolve;
	use std::sync::Arc;
	use crate::{ConfigFile, FilterType};

	fn test_config()->ConfigFile{
		ConfigFile{
			bind_addr:"127.0.0.1:0".to_owned(),
			timeout:1000,
			user_agent:"test".to_owned(),
			max_size:1024,
			proxy:None,
			filter_type:FilterType::Triangle,
			max_pixels:64,
			append_headers:vec![],
			load_system_fonts:false,
			webp_quality:75.0,
			encode_avif:false,
			allowed_networks:None,
			blocked_networks:None,
			blocked_hosts:None,
		}
	}

	fn v4(s:&str)->IpAddr{
		s.parse().unwrap()
	}

	fn v6(s:&str)->IpAddr{
		s.parse().unwrap()
	}

	#[test]
	fn default_deny_ranges(){
		let c=test_config();
		// Must be blocked by default.
		for ip in ["127.0.0.1","10.1.2.3","172.16.5.4","192.168.1.1","169.254.169.254","100.64.0.1","0.0.0.0"]{
			assert!(is_ip_blocked(&c,v4(ip)),"{} should be blocked",ip);
		}
		for ip in ["::1","::","fe80::1","fc00::1","ff02::1","::ffff:127.0.0.1","::ffff:169.254.169.254"]{
			assert!(is_ip_blocked(&c,v6(ip)),"{} should be blocked",ip);
		}
		// Public addresses pass.
		for ip in ["8.8.8.8","1.1.1.1","93.184.216.34"]{
			assert!(!is_ip_blocked(&c,v4(ip)),"{} should pass",ip);
		}
		assert!(!is_ip_blocked(&c,v6("2606:4700:4700::1111")),"public v6 should pass");
		// IPv4-mapped public passes.
		assert!(!is_ip_blocked(&c,v6("::ffff:8.8.8.8")),"mapped public should pass");
	}

	#[test]
	fn allow_list_opens_default_deny(){
		let mut c=test_config();
		c.allowed_networks=Some(vec!["192.168.0.0/16".to_owned()]);
		assert!(!is_ip_blocked(&c,v4("192.168.1.10")));
		assert!(is_ip_blocked(&c,v4("10.0.0.5")));
	}

	#[test]
	fn blocked_networks_win(){
		let mut c=test_config();
		c.blocked_networks=Some(vec!["8.8.8.0/24".to_owned()]);
		assert!(is_ip_blocked(&c,v4("8.8.8.8")));
		assert!(!is_ip_blocked(&c,v4("1.1.1.1")));
	}

	#[test]
	fn mixed_answer_is_refused(){
		let c=test_config();
		let ips=vec![
			SocketAddr::new(v4("93.184.216.34"),0),
			SocketAddr::new(v4("127.0.0.1"),0),
		];
		assert!(validate_resolved_ips(&c,&ips).is_err());
		assert!(validate_resolved_ips(&c,&[]).is_err());
		let ok=vec![SocketAddr::new(v4("93.184.216.34"),0)];
		assert!(validate_resolved_ips(&c,&ok).is_ok());
	}

	#[test]
	fn host_suffix_match(){
		let blocked=Some(vec!["example.com".to_owned()]);
		assert!(is_host_blocked(blocked.as_ref(),"example.com"));
		assert!(is_host_blocked(blocked.as_ref(),"evil.example.com"));
		assert!(is_host_blocked(blocked.as_ref(),"EXAMPLE.COM."));
		assert!(!is_host_blocked(blocked.as_ref(),"example.com.evil.org"));
		assert!(!is_host_blocked(blocked.as_ref(),"notexample.com"));
		assert!(!is_host_blocked(None,"example.com"));
		let dot=Some(vec![".example.com".to_owned()]);
		assert!(is_host_blocked(dot.as_ref(),"evil.example.com"));
		assert!(!is_host_blocked(dot.as_ref(),"example.com"));
	}

	#[test]
	fn dns_cache_returns_consistent_view(){
		let rt=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
		rt.block_on(async{
			let a=cached_lookup_host("localhost").await.unwrap();
			// Case + trailing-dot normalization hits the same entry.
			let b=cached_lookup_host("LOCALHOST.").await.unwrap();
			assert_eq!(a,b);
			assert!(!a.is_empty());
		});
	}

	#[test]
	fn dns_cache_negative(){
		let rt=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
		rt.block_on(async{
			// .invalid は RFC 2606 で解決不能が保証される。
			let name="nonexistent-name-for-test.invalid";
			assert!(cached_lookup_host(name).await.is_err());
			// 2 回目はネガティブキャッシュから即時 Err。
			assert!(cached_lookup_host(name).await.is_err());
		});
	}

	#[test]
	fn resolver_blocks_rebinding_target(){		// localhost must fail at resolve time even though "resolving" succeeds.
		let config=Arc::new(test_config());
		let r=ValidatingResolver::new(config);
		let rt=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
		rt.block_on(async{
			let name:reqwest::dns::Name="localhost".parse().unwrap();
			assert!(r.resolve(name).await.is_err());
		});
	}
}
