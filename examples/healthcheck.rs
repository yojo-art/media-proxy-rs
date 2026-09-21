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

/// Mirrors main.rs's `parse_bind_addr` (kept separate: this is a standalone
/// example binary, not linked against the main crate's private functions).
fn parse_target(s: &str) -> Option<Target> {
	let s = s.trim();
	if let Some(rest) = s.strip_prefix("unix://") {
		return (!rest.is_empty()).then(|| Target::Unix(PathBuf::from(rest)));
	}
	if let Some(rest) = s.strip_prefix("unix:") {
		return (!rest.is_empty()).then(|| Target::Unix(PathBuf::from(rest)));
	}
	if s.starts_with("http://") || s.starts_with("https://") {
		return Some(Target::Tcp(s.to_owned()));
	}
	if let Ok(addr) = s.parse::<std::net::SocketAddr>() {
		// A wildcard bind address is not itself connectable; probe loopback.
		let host = match addr.ip() {
			std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
			std::net::IpAddr::V6(ip) if ip.is_unspecified() => "::1".to_owned(),
			std::net::IpAddr::V4(ip) => ip.to_string(),
			std::net::IpAddr::V6(ip) => format!("[{}]", ip),
		};
		return Some(Target::Tcp(format!(
			"http://{}:{}/healthz",
			host,
			addr.port()
		)));
	}
	if s.contains('/') || s.ends_with(".sock") {
		return Some(Target::Unix(PathBuf::from(s)));
	}
	None
}

/// Resolves the healthcheck target: an explicit CLI override (`args[1]`), or
/// derived from the same config.json / MEDIA_PROXY_CONFIG_PATH main.rs uses.
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
