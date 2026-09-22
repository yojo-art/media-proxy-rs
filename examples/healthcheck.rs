use std::path::PathBuf;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Only the field this healthcheck needs; other config.json fields are
/// ignored by serde.
#[derive(Deserialize)]
struct Cfg {
	bind_addr: String,
}

enum Target {
	Tcp(String),
	Unix(PathBuf),
}

/// Probe target for a TCP bind address: the host is always loopback.
///
/// A `bind_addr` is necessarily local -- the server cannot bind anything else --
/// so the host part tells the probe nothing, and taking it from the config would
/// let a config value turn this healthcheck into an outbound request to an
/// arbitrary host. Only the port is used, and the path is fixed to `/healthz`:
/// the liveness route that never goes through the SSRF-checked fetch path.
fn tcp_target(addr: &std::net::SocketAddr) -> Target {
	let host = match addr.ip() {
		std::net::IpAddr::V4(_) => "127.0.0.1",
		std::net::IpAddr::V6(_) => "[::1]",
	};
	Target::Tcp(format!("http://{}:{}/healthz", host, addr.port()))
}

/// Authority part of a URL: drops any path, query or fragment.
fn authority(s: &str) -> &str {
	match s.find(['/', '?', '#']) {
		Some(i) => &s[..i],
		None => s,
	}
}

/// Mirrors main.rs's `parse_bind_addr` (kept separate: this is a standalone
/// example binary, not linked against the main crate's private functions).
/// `http://IP:port` is accepted like `IP:port`, and every value main.rs would
/// refuse is refused here too, so the two cannot disagree about which address
/// the server is listening on.
fn parse_target(s: &str) -> Option<Target> {
	let s = s.trim();
	if let Some(rest) = s.strip_prefix("unix://") {
		return (!rest.is_empty()).then(|| Target::Unix(PathBuf::from(rest)));
	}
	if let Some(rest) = s.strip_prefix("unix:") {
		return (!rest.is_empty()).then(|| Target::Unix(PathBuf::from(rest)));
	}
	if let Some(rest) = s.strip_prefix("http://") {
		return authority(rest)
			.parse::<std::net::SocketAddr>()
			.ok()
			.map(|addr| tcp_target(&addr));
	}
	if s.contains("://") {
		// https:// (TLS is not supported) and unknown schemes: main.rs refuses
		// to start on these, so there is no listener to probe.
		return None;
	}
	if let Ok(addr) = s.parse::<std::net::SocketAddr>() {
		return Some(tcp_target(&addr));
	}
	if s.contains('/') || s.ends_with(".sock") {
		return Some(Target::Unix(PathBuf::from(s)));
	}
	None
}

/// Resolves the healthcheck target: an explicit CLI override (`args[1]`, parsed
/// exactly like `bind_addr`), or derived from the same config.json /
/// MEDIA_PROXY_CONFIG_PATH main.rs uses.
fn resolve_target(override_target: &Option<String>) -> Option<Target> {
	if let Some(s) = override_target {
		return parse_target(s);
	}
	let config_path = std::env::var("MEDIA_PROXY_CONFIG_PATH")
		.ok()
		.filter(|p| !p.is_empty())
		.unwrap_or_else(|| "config.json".to_owned());
	let file = std::fs::File::open(&config_path).ok()?;
	let cfg: Cfg = serde_json::from_reader(file).ok()?;
	parse_target(&cfg.bind_addr)
}

async fn check_unix(path: &std::path::Path) -> bool {
	let mut stream = match tokio::net::UnixStream::connect(path).await {
		Ok(s) => s,
		Err(_) => return false,
	};
	if stream
		.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
		.await
		.is_err()
	{
		return false;
	}
	let mut buf = Vec::new();
	// The connection is closed by the server after the response, so reading
	// to EOF is enough for this fixed, tiny healthz response.
	if stream.read_to_end(&mut buf).await.is_err() {
		return false;
	}
	buf.starts_with(b"HTTP/1.1 200") || buf.starts_with(b"HTTP/1.0 200")
}

async fn check_tcp(client: &reqwest::Client, url: &str) -> bool {
	matches!(client.get(url).send().await, Ok(s) if s.status().as_u16() == 200)
}

fn main() {
	let args: Vec<String> = std::env::args().collect();
	let override_target = args.get(1).cloned();
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let client = reqwest::Client::builder();
	// Never honour HTTP(S)_PROXY/ALL_PROXY: a loopback probe must reach healthz
	// directly instead of being rerouted through any operator-exported proxy.
	let client = client.no_proxy();
	let client = client.timeout(std::time::Duration::from_millis(400));
	let client = client.build().unwrap();
	// Fail fast. This runs as Docker's HEALTHCHECK (see Dockerfile) whose timeout
	// is 3s, so the total budget here must stay well under it. Cold-start
	// readiness is handled by HEALTHCHECK --start-period and by the build-time
	// wait loop, not by a long retry loop in this binary. Budget: 3*400ms + 2*100ms.
	for attempt in 0..3 {
		let client = client.clone();
		let override_target = override_target.clone();
		let ok = rt.block_on(async move {
			// config.json may not exist yet immediately after container start,
			// so resolving the target is itself part of the retry loop.
			match resolve_target(&override_target) {
				Some(Target::Tcp(url)) => check_tcp(&client, &url).await,
				Some(Target::Unix(path)) => check_unix(&path).await,
				None => false,
			}
		});
		if ok {
			println!("ok");
			std::process::exit(0);
		}
		if attempt < 2 {
			std::thread::sleep(std::time::Duration::from_millis(100));
		}
	}
	println!("healthcheck failed");
	std::process::exit(1);
}

#[cfg(test)]
mod tests {
	use super::*;

	fn tcp_url(s: &str) -> Option<String> {
		match parse_target(s) {
			Some(Target::Tcp(url)) => Some(url),
			_ => None,
		}
	}

	#[test]
	fn tcp_targets_probe_loopback_healthz() {
		// The host from bind_addr is never dialled, only its port is used.
		for addr in [
			"0.0.0.0:12766",
			"127.0.0.1:12766",
			"192.0.2.5:12766",
			"http://0.0.0.0:12766",
			"http://127.0.0.1:12766",
			"http://127.0.0.1:12766/healthz", // 旧CLI引数形式のURL
		] {
			assert_eq!(
				tcp_url(addr).as_deref(),
				Some("http://127.0.0.1:12766/healthz"),
				"{}",
				addr
			);
		}
		// IPv6 binds keep the same-family loopback.
		for addr in ["[::]:12766", "[::1]:12766", "http://[::1]:12766"] {
			assert_eq!(
				tcp_url(addr).as_deref(),
				Some("http://[::1]:12766/healthz"),
				"{}",
				addr
			);
		}
	}

	#[test]
	fn unix_targets_keep_the_socket_path() {
		for addr in [
			"unix:///run/media-proxy/proxy.sock",
			"unix:/run/media-proxy/proxy.sock",
			"/run/media-proxy/proxy.sock",
		] {
			match parse_target(addr) {
				Some(Target::Unix(path)) => {
					assert_eq!(
						path,
						PathBuf::from("/run/media-proxy/proxy.sock"),
						"{}",
						addr
					)
				}
				_ => panic!("{} should be a unix target", addr),
			}
		}
	}

	#[test]
	fn unsupported_values_are_rejected() {
		// main.rs refuses to start on these, so there is nothing to probe.
		for addr in [
			"",
			"12766",
			"localhost:12766",
			"http://localhost:12766",
			"http://127.0.0.1",
			"https://127.0.0.1:12766",
			"tcp://127.0.0.1:12766",
			"unix://",
		] {
			assert!(parse_target(addr).is_none(), "{} should be rejected", addr);
		}
	}
}
