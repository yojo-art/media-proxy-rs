use core::str;
use std::{
	io::Write,
	net::SocketAddr,
	path::{Path, PathBuf},
	pin::Pin,
	str::FromStr,
	sync::Arc,
};

use axum::{http::HeaderMap, response::IntoResponse, Router};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

mod browsersafe;
mod image_test;
mod img;
mod ssrf;
mod svg;

/// Bounds concurrent fetch+encode work so that per-request memory budgets
/// (#4/#5) cannot be multiplied without limit (finding #6). Auth and
/// per-client rate limiting remain deployment decisions (e.g. reverse proxy
/// or network policy in front of this Misskey/Cherrypick media proxy).
static FETCH_SEMAPHORE: std::sync::LazyLock<tokio::sync::Semaphore> =
	std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(32));

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigFile {
	bind_addr: String,
	timeout: u64,
	user_agent: String,
	max_size: u64,
	proxy: Option<String>,
	filter_type: FilterType,
	max_pixels: u32,
	append_headers: Vec<String>,
	load_system_fonts: bool,
	webp_quality: f32,
	encode_avif: bool,
	allowed_networks: Option<Vec<String>>,
	blocked_networks: Option<Vec<String>>,
	blocked_hosts: Option<Vec<String>>,
	/// Octal permission bits for a `unix://` `bind_addr` socket, e.g. `"0660"`.
	/// Defaults to `0666` (existing behavior) when unset.
	unix_socket_permissions: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct RequestParams {
	url: String,
	//#[serde(rename = "static")]
	r#static: Option<String>,
	emoji: Option<String>,
	avatar: Option<String>,
	preview: Option<String>,
	badge: Option<String>,
	fallback: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum FilterType {
	Nearest,
	Triangle,
	CatmullRom,
	Gaussian,
	Lanczos3,
}
impl Into<image::imageops::FilterType> for FilterType {
	fn into(self) -> image::imageops::FilterType {
		match self {
			FilterType::Nearest => image::imageops::Nearest,
			FilterType::Triangle => image::imageops::Triangle,
			FilterType::CatmullRom => image::imageops::CatmullRom,
			FilterType::Gaussian => image::imageops::Gaussian,
			FilterType::Lanczos3 => image::imageops::Lanczos3,
		}
	}
}
impl Into<fast_image_resize::FilterType> for FilterType {
	fn into(self) -> fast_image_resize::FilterType {
		match self {
			FilterType::Nearest => fast_image_resize::FilterType::Box,
			FilterType::Triangle => fast_image_resize::FilterType::Bilinear,
			FilterType::CatmullRom => fast_image_resize::FilterType::CatmullRom,
			FilterType::Gaussian => fast_image_resize::FilterType::Mitchell,
			FilterType::Lanczos3 => fast_image_resize::FilterType::Lanczos3,
		}
	}
}
async fn shutdown_signal() {
	use futures::{future::FutureExt, pin_mut};
	use tokio::signal;
	let ctrl_c = async {
		signal::ctrl_c()
			.await
			.expect("failed to install Ctrl+C handler");
	}
	.fuse();

	#[cfg(unix)]
	let terminate = async {
		signal::unix::signal(signal::unix::SignalKind::terminate())
			.expect("failed to install signal handler")
			.recv()
			.await;
	}
	.fuse();
	#[cfg(not(unix))]
	let terminate = std::future::pending::<()>().fuse();
	pin_mut!(ctrl_c, terminate);
	futures::select! {
		_ = ctrl_c => {},
		_ = terminate => {},
	}
}
fn main() {
	let config_path = match std::env::var("MEDIA_PROXY_CONFIG_PATH") {
		Ok(path) => {
			if path.is_empty() {
				"config.json".to_owned()
			} else {
				path
			}
		}
		Err(_) => "config.json".to_owned(),
	};
	if !std::path::Path::new(&config_path).exists() {
		let default_config=ConfigFile{
			bind_addr: "0.0.0.0:12766".to_owned(),
			timeout:10000,
			user_agent: "https://github.com/yojo-art/media-proxy-rs".to_owned(),
			max_size:256*1024*1024,
			proxy:None,
			filter_type:FilterType::Triangle,
			max_pixels:2048,
			append_headers:[
				"Content-Security-Policy:default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'".to_owned(),
				"Access-Control-Allow-Origin:*".to_owned(),
			].to_vec(),
			load_system_fonts:true,
			webp_quality: 75f32,
			encode_avif:false,
			allowed_networks:None,
			blocked_networks:None,
			blocked_hosts:None,
			unix_socket_permissions:None,
		};
		let default_config = serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path)
			.expect("create default config.json")
			.write_all(default_config.as_bytes())
			.unwrap();
	}
	let mut config: ConfigFile =
		serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();
	if let Ok(networks) = std::env::var("MEDIA_PROXY_ALLOWED_NETWORKS") {
		let mut allowed_networks = config.allowed_networks.take().unwrap_or_default();
		for networks in networks.split(",") {
			allowed_networks.push(networks.to_owned());
		}
		config.allowed_networks.replace(allowed_networks);
	}
	if let Ok(networks) = std::env::var("MEDIA_PROXY_BLOCKED_NETWORKS") {
		let mut blocked_networks = config.blocked_networks.take().unwrap_or_default();
		for networks in networks.split(",") {
			blocked_networks.push(networks.to_owned());
		}
		config.blocked_networks.replace(blocked_networks);
	}
	if let Ok(networks) = std::env::var("MEDIA_PROXY_BLOCKED_HOSTS") {
		let mut blocked_hosts = config.blocked_hosts.take().unwrap_or_default();
		for networks in networks.split(",") {
			blocked_hosts.push(networks.to_owned());
		}
		config.blocked_hosts.replace(blocked_hosts);
	}
	// A bad CIDR must stop startup with a clear message (M-05), not turn into
	// a panic on every request.
	if let Err(e) = crate::ssrf::validate_network_config(&config) {
		eprintln!("invalid network configuration: {}", e);
		std::process::exit(1);
	}
	// Resolved up front (not inside serve_on_unix_socket) so a bad value is a
	// startup error, consistent with the network config validation above.
	let unix_socket_mode: u32 = match &config.unix_socket_permissions {
		Some(s) => match parse_unix_socket_mode(s) {
			Ok(mode) => mode,
			Err(e) => {
				eprintln!("invalid unix_socket_permissions: {}", e);
				std::process::exit(1);
			}
		},
		None => 0o666,
	};
	let dummy_png = Arc::new(include_bytes!("../asset/dummy.png").to_vec());
	let config = Arc::new(config);
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let client = reqwest::ClientBuilder::new();
	let client = match &config.proxy {
		Some(url) => {
			// See ssrf.rs module doc "Caveat": with an egress proxy configured, the
			// proxy resolves and connects to the target, so ValidatingResolver's
			// connect-time SSRF re-validation (DNS-rebinding protection) does not
			// apply to fetch targets in this mode. Only check_url's independent
			// pre-check protects proxied requests.
			eprintln!("WARNING: proxy is configured ({}). Connect-time SSRF re-validation (DNS-rebinding protection) does not apply to fetch targets in this mode; ensure the proxy itself enforces an equivalent SSRF policy.",url);
			// Proxy::all (not Proxy::http): code-review finding -- Proxy::http
			// only intercepts http:// targets, so https:// targets (the
			// majority of real media URLs) would otherwise connect directly,
			// bypassing the proxy and the warning above entirely.
			let proxy = match reqwest::Proxy::all(url) {
				Ok(proxy) => proxy,
				Err(e) => {
					eprintln!("invalid proxy configuration {:?}: {}", url, e);
					std::process::exit(1);
				}
			};
			client.proxy(proxy)
		}
		None => client,
	};
	// Do NOT follow redirects automatically: each redirect target must pass
	// check_url again (finding #2). Redirects are followed manually in get_file.
	let client = client.redirect(reqwest::redirect::Policy::none());
	// Connect-time SSRF enforcement: validate the addresses the connection is
	// actually opened to, closing the DNS-rebinding TOCTOU between check_url's
	// lookup and the connect-time lookup (finding #2).
	let client = client.dns_resolver(std::sync::Arc::new(crate::ssrf::ValidatingResolver::new(
		config.clone(),
	)));
	let client = client.build().unwrap();
	let mut fontdb = resvg::usvg::fontdb::Database::new();
	if config.load_system_fonts {
		fontdb.load_system_fonts();
	}
	if std::path::Path::new("asset/font/").exists() {
		fontdb.load_fonts_dir("asset/font/");
	}
	fontdb.load_font_source(resvg::usvg::fontdb::Source::Binary(Arc::new(
		include_bytes!("../asset/font/Aileron-Light.otf"),
	)));
	let fontdb = Arc::new(fontdb);
	let arg_tup = (client, config, dummy_png, fontdb);
	rt.block_on(async {
		let bind_addr = arg_tup.1.bind_addr.clone();
		let app = Router::new();
		// Liveness probe that never touches the fetch path: the SSRF policy
		// denies loopback by default, so a self-check going through get_file
		// can never succeed (Docker build-time check and HEALTHCHECK both
		// target 127.0.0.1).
		let app = app.route(
			"/healthz",
			axum::routing::get(|| async { (axum::http::StatusCode::OK, "ok") }),
		);
		let arg_tup0 = arg_tup.clone();
		let app = app.route(
			"/",
			axum::routing::get(move |headers, parms| {
				get_file(None, headers, arg_tup0.clone(), parms)
			}),
		);
		let app = app.route(
			"/{*path}",
			axum::routing::get(move |path, headers, parms| {
				get_file(Some(path), headers, arg_tup.clone(), parms)
			}),
		);
		// A single bad request must not kill the process (finding #3).
		// With panic="abort" removed, this layer turns handler panics into 500s.
		let app = app.layer(tower_http::catch_panic::CatchPanicLayer::new());
		// NOTE: handlers do not use ConnectInfo, so plain into_make_service()
		// works for both TCP and UDS listeners.
		match parse_bind_addr(&bind_addr) {
			Ok(BindTarget::Tcp(addr)) => {
				let listener = match tokio::net::TcpListener::bind(addr).await {
					Ok(listener) => listener,
					Err(e) => {
						eprintln!("failed to bind TCP {}: {}", addr, e);
						std::process::exit(1);
					}
				};
				eprintln!("listening on {}", addr);
				axum::serve(listener, app.into_make_service())
					.with_graceful_shutdown(shutdown_signal())
					.await
					.unwrap();
			}
			Ok(BindTarget::Unix(path)) => {
				#[cfg(not(unix))]
				{
					let _ = &path;
					eprintln!("unix domain socket is only supported on unix platforms");
					std::process::exit(1);
				}
				#[cfg(unix)]
				{
					serve_on_unix_socket(app, &path, unix_socket_mode).await;
				}
			}
			Err(e) => {
				eprintln!("{}", e);
				std::process::exit(1);
			}
		}
	});
}

/// Where to listen, decided by `bind_addr`.
enum BindTarget {
	Tcp(SocketAddr),
	Unix(PathBuf),
}

/// Canonical form is `unix:///path/to/sock` (or `unix:/path/to/sock`).
/// For compatibility a bare absolute path such as `/var/run/.../proxy.sock`
/// is also accepted as UDS, with a deprecation warning telling the operator
/// to add the `unix://` prefix. Anything else must parse as `SocketAddr`.
fn parse_bind_addr(s: &str) -> Result<BindTarget, String> {
	let s = s.trim();
	if let Some(rest) = s.strip_prefix("unix://") {
		if rest.is_empty() {
			return Err(format!("invalid bind_addr {:?}: empty socket path", s));
		}
		return Ok(BindTarget::Unix(PathBuf::from(rest)));
	}
	if let Some(rest) = s.strip_prefix("unix:") {
		if rest.is_empty() {
			return Err(format!("invalid bind_addr {:?}: empty socket path", s));
		}
		return Ok(BindTarget::Unix(PathBuf::from(rest)));
	}
	if let Ok(addr) = s.parse::<SocketAddr>() {
		return Ok(BindTarget::Tcp(addr));
	}
	// Compatibility fallback so the earlier bare-path form keeps working.
	if s.contains('/') || s.ends_with(".sock") {
		eprintln!(
			"WARNING: bind_addr {:?} has no scheme; treating it as a unix socket. Use \"unix://{}\" instead.",
			s, s
		);
		return Ok(BindTarget::Unix(PathBuf::from(s)));
	}
	Err(format!(
		"invalid bind_addr {:?}: expected \"IP:port\" or \"unix:///path/to.sock\"",
		s
	))
}

/// Parses `unix_socket_permissions` (e.g. `"0666"`, `"660"`, `"0o600"`) as
/// octal permission bits. Accepts an optional `0o` prefix.
fn parse_unix_socket_mode(s: &str) -> Result<u32, String> {
	let s = s.trim();
	let digits = s.strip_prefix("0o").unwrap_or(s);
	if digits.is_empty() || !digits.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
		return Err(format!("{:?}: expected octal permission bits", s));
	}
	let mode = u32::from_str_radix(digits, 8).map_err(|e| format!("{:?}: {}", s, e))?;
	if mode > 0o777 {
		return Err(format!("{:?}: out of range (expected 0..=0777)", s));
	}
	Ok(mode)
}

/// Bind a UDS listener: create parent dirs, drop a stale socket file left by
/// an unclean shutdown, then chmod to `mode` (default `0666`, configurable via
/// `unix_socket_permissions`) so a reverse proxy running as a different user
/// can connect.
#[cfg(unix)]
async fn serve_on_unix_socket(app: Router, path: &Path, mode: u32) {
	use std::os::unix::fs::PermissionsExt;
	if let Some(parent) = path.parent() {
		if !parent.as_os_str().is_empty() {
			if let Err(e) = tokio::fs::create_dir_all(parent).await {
				eprintln!(
					"failed to create socket parent dir {}: {}",
					parent.display(),
					e
				);
				std::process::exit(1);
			}
		}
	}
	match tokio::fs::symlink_metadata(path).await {
		Ok(_) => {
			if let Err(e) = tokio::fs::remove_file(path).await {
				eprintln!("failed to remove stale socket {}: {}", path.display(), e);
				std::process::exit(1);
			}
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
		Err(e) => {
			eprintln!("failed to stat socket path {}: {}", path.display(), e);
			std::process::exit(1);
		}
	}
	let listener = match tokio::net::UnixListener::bind(path) {
		Ok(listener) => listener,
		Err(e) => {
			eprintln!("failed to bind unix socket {}: {}", path.display(), e);
			std::process::exit(1);
		}
	};
	if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
		eprintln!(
			"failed to chmod {:o} socket {}: {}",
			mode,
			path.display(),
			e
		);
		std::process::exit(1);
	}
	eprintln!("listening on unix://{}", path.display());
	axum::serve(listener, app.into_make_service())
		.with_graceful_shutdown(shutdown_signal())
		.await
		.unwrap();
}
async fn check_url(config: &Arc<ConfigFile>, url: impl AsRef<str>) -> Result<(), String> {
	let u = reqwest::Url::from_str(url.as_ref()).map_err(|e| format!("{:?}", e))?;
	match u.scheme().to_lowercase().as_str() {
		"http" | "https" => {}
		scheme => return Err(format!("scheme: {}", scheme)),
	}
	let host = u.host_str().ok_or_else(|| "no host".to_owned())?;
	// Fail fast with a clear error. The connect-time ValidatingResolver
	// re-enforces the same policy on the addresses actually connected to,
	// closing the DNS-rebinding TOCTOU (finding #2).
	if crate::ssrf::is_host_blocked(config.blocked_hosts.as_ref(), host) {
		return Err("Blocked address".to_owned());
	}
	// Async resolution so a slow attacker-controlled nameserver cannot stall
	// the async worker thread (finding #10). Shared LRU cache with the
	// connect-time resolver keeps both DNS views consistent.
	let ips = crate::ssrf::cached_lookup_host(host)
		.await
		.map_err(|e| format!("{:?} {}", e, host))?;
	crate::ssrf::validate_resolved_ips(config, &ips)
}
async fn get_file(
	_path: Option<axum::extract::Path<String>>,
	client_headers: axum::http::HeaderMap,
	(client, config, dummy_img, fontdb): (
		reqwest::Client,
		Arc<ConfigFile>,
		Arc<Vec<u8>>,
		Arc<resvg::usvg::fontdb::Database>,
	),
	axum::extract::Query(q): axum::extract::Query<RequestParams>,
) -> Result<(axum::http::StatusCode, HeaderMap, axum::body::Body), axum::response::Response> {
	let mut headers = HeaderMap::new();
	// q.url uses {:?} so percent-decoded CR/LF cannot forge log lines (finding #8).
	println!(
		"{}\t{:?}\tavatar:{:?}\tpreview:{:?}\tbadge:{:?}\temoji:{:?}\tstatic:{:?}\tfallback:{:?}",
		chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
		q.url,
		q.avatar,
		q.preview,
		q.badge,
		q.emoji,
		q.r#static,
		q.fallback,
	);
	if let Ok(url) = q.url.parse() {
		headers.append("X-Remote-Url", url);
	}
	if config.encode_avif {
		headers.append("Vary", "Accept,Range".parse().unwrap());
	}
	let time = chrono::Utc::now();
	if let Err(s) = check_url(&config, &q.url).await {
		if let Ok(v) = s.parse() {
			headers.append("X-Proxy-Error", v);
		}
		if q.fallback.is_some() {
			headers.append("Content-Type", "image/png".parse().unwrap());
			return Err((axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response());
		}
		return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
	};

	println!(
		"check_url {}ms",
		(chrono::Utc::now() - time).num_milliseconds()
	);
	// Bound concurrent work (finding #6). The permit is acquired only after
	// URL validation so DNS resolution cannot occupy every permit (M-03) and
	// is held through buffered fetch+encode so memory/CPU budgets cannot be
	// stacked. Streaming browsersafe passthrough below releases it when the
	// response is handed off (its memory footprint stays small while streaming).
	// NOTE: acquire() on a never-closed semaphore never fails with Err, so a
	// timeout is what actually sheds load with 503 instead of queueing forever.
	let _permit = match tokio::time::timeout(
		std::time::Duration::from_secs(30),
		FETCH_SEMAPHORE.acquire(),
	)
	.await
	{
		Ok(Ok(permit)) => permit,
		_ => {
			headers.append("X-Proxy-Error", "Overloaded".parse().unwrap());
			return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, headers).into_response());
		}
	};
	// DNS-rebinding TOCTOU (finding #2) is closed for direct fetches: reqwest's
	// ValidatingResolver (ssrf.rs) re-validates the addresses actually connected
	// to, using the same DNS cache as check_url, so a second (possibly attacker-
	// controlled) resolution can never bypass the policy. Redirects are also
	// re-validated hop by hop below regardless.
	// Residual risk: when `config.proxy` is set, the egress proxy — not this
	// process — resolves and connects to the target host, so ValidatingResolver
	// never sees the target's addresses (see ssrf.rs's module doc "Caveat" and
	// `ValidatingResolver::resolve`'s `proxy_host` branch). Only this check_url
	// pre-check protects proxied requests, and the classic rebinding TOCTOU
	// applies again in that configuration.
	const MAX_REDIRECTS: u8 = 5;
	let mut current_url = q.url.clone();
	let mut redirects: u8 = 0;
	let resp = loop {
		let req = client.get(&current_url);
		let req = req.timeout(std::time::Duration::from_millis(config.timeout));
		let req = req.header("User-Agent", config.user_agent.clone());
		let req = if let Some(range) = client_headers.get("Range") {
			req.header("Range", range.as_bytes())
		} else {
			req
		};
		let resp = match req.send().await {
			Ok(resp) => resp,
			Err(e) => {
				if q.fallback.is_some() {
					headers.append("Content-Type", "image/png".parse().unwrap());
					return Err(
						(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
					);
				}
				return Err((
					axum::http::StatusCode::BAD_REQUEST,
					headers,
					format!("{:?}", e),
				)
					.into_response());
			}
		};
		if !resp.status().is_redirection() {
			break resp;
		}
		if redirects >= MAX_REDIRECTS {
			headers.append("X-Proxy-Error", "TooManyRedirects".parse().unwrap());
			return Err((axum::http::StatusCode::BAD_GATEWAY, headers).into_response());
		}
		let location = resp
			.headers()
			.get(axum::http::header::LOCATION)
			.and_then(|v| v.to_str().ok().map(|s| s.to_owned()));
		let Some(location) = location else {
			break resp;
		};
		// Resolve relative Location headers against the current URL, then
		// re-validate every hop with check_url (finding #2).
		let base = reqwest::Url::from_str(&current_url)
			.map_err(|e| {
				(
					axum::http::StatusCode::BAD_REQUEST,
					headers.clone(),
					format!("{:?}", e),
				)
					.into_response()
			})
			.map_err(axum::response::Response::from)?;
		let next = base
			.join(&location)
			.map_err(|e| {
				(
					axum::http::StatusCode::BAD_REQUEST,
					headers.clone(),
					format!("{:?}", e),
				)
					.into_response()
			})
			.map_err(axum::response::Response::from)?;
		// Drain only a bounded prefix of the redirect body (H-02). Reading
		// the whole body with `resp.bytes()` let an attacker buffer gigabytes
		// per hop (no max_size check applies here).
		const MAX_REDIRECT_DRAIN: usize = 64 * 1024;
		let mut stream = resp.bytes_stream();
		let mut drained = 0usize;
		while let Some(Ok(chunk)) = stream.next().await {
			drained += chunk.len();
			if drained >= MAX_REDIRECT_DRAIN {
				break;
			}
		}
		drop(stream);
		let next_str = next.to_string();
		if let Err(s) = check_url(&config, &next_str).await {
			if let Ok(v) = s.parse() {
				headers.append("X-Proxy-Error", v);
			}
			return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
		}
		current_url = next_str;
		redirects += 1;
	};
	fn add_remote_header(
		key: &'static str,
		headers: &mut HeaderMap,
		remote_headers: &reqwest::header::HeaderMap,
	) {
		for v in remote_headers.get_all(key) {
			// Never unwrap on attacker-controlled bytes: from_bytes rejects
			// CTLs/DEL and we must not panic (finding #3). Invalid values are
			// dropped instead of aborting the whole process.
			if let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
				headers.append(key, value);
			}
		}
	}
	let remote_headers = resp.headers();
	add_remote_header("Content-Disposition", &mut headers, remote_headers);
	add_remote_header("Content-Type", &mut headers, remote_headers);
	let is_img = if let Some(media) = headers.get("Content-Type") {
		let s = String::from_utf8_lossy(media.as_bytes());
		s.starts_with("image/")
	} else {
		false
	};
	if !is_img {
		add_remote_header("Content-Length", &mut headers, remote_headers);
		add_remote_header("Content-Range", &mut headers, remote_headers);
		add_remote_header("Accept-Ranges", &mut headers, remote_headers);
	}
	let mut is_accept_avif = false;
	if !config.encode_avif {
		//force no avif
	} else if let Some(accept) = client_headers.get("Accept") {
		if let Ok(accept) = std::str::from_utf8(accept.as_bytes()) {
			for e in accept.split(",") {
				if e == "image/avif" {
					is_accept_avif = true;
				}
			}
		}
	}
	headers.append("Cache-Control", "max-age=300".parse().unwrap());
	// Refuse MIME sniffing so reflected content types cannot be reinterpreted
	// (finding #7; also mitigates the #9 sniffing concern).
	headers.append("X-Content-Type-Options", "nosniff".parse().unwrap());
	for line in config.append_headers.iter() {
		if let Some(idx) = line.find(":") {
			if idx + 1 >= line.len() {
				continue;
			}
			if let Ok(k) = axum::http::HeaderName::from_str(&line[0..idx]) {
				if let Ok(v) = line[idx + 1..].parse() {
					headers.append(k, v);
				}
			}
		}
	}
	RequestContext {
		is_accept_avif,
		headers,
		parms: q,
		src_bytes: Vec::new(),
		config,
		codec: Err(None),
		dummy_img,
		fontdb,
	}
	.encode(resp, is_img)
	.await
}
struct RequestContext {
	is_accept_avif: bool,
	headers: HeaderMap,
	parms: RequestParams,
	src_bytes: Vec<u8>,
	config: Arc<ConfigFile>,
	codec: Result<image::ImageFormat, Option<image::ImageError>>,
	dummy_img: Arc<Vec<u8>>,
	fontdb: Arc<resvg::usvg::fontdb::Database>,
}
impl RequestContext {
	pub fn disposition_ext(headers: &mut HeaderMap, ext: &str) {
		let k = "Content-Disposition";
		if let Some(cd) = headers.get(k) {
			let s = std::str::from_utf8(cd.as_bytes());
			if let Ok(s) = s {
				let cd = mailparse::parse_content_disposition(s);
				let cd_utf8 = cd.params.get("filename*");
				let mut name = None;
				if let Some(cd_utf8) = cd_utf8 {
					let cd_utf8 = cd_utf8.to_uppercase();
					if cd_utf8.starts_with("UTF-8''") && cd_utf8.len() > 7 {
						name = urlencoding::decode(&cd_utf8[7..])
							.map(|s| s.to_string())
							.ok();
					}
				}
				if name.is_none() {
					if let Some(filename) = cd.params.get("filename") {
						let m_filename = format!("_:{}", filename);
						let parsed = mailparse::parse_header(&m_filename.as_bytes());
						if let Ok((parsed, _)) = &parsed {
							name = Some(parsed.get_value());
						} else if cd.params.get("name").is_none() {
							name = Some(filename.clone());
						}
					}
				}
				let name = name.unwrap_or_else(|| {
					cd.params
						.get("name")
						.map(|s| s.clone())
						.unwrap_or_else(|| "null".to_owned())
				});
				let mut name_arr: Vec<&str> = name.split('.').collect();
				name_arr.pop();
				let name = name_arr.join(".") + ext;
				let name = urlencoding::encode(&name);
				let content_disposition =
					format!("inline; filename=\"{}\";filename*=UTF-8''{};", name, name);
				headers.remove(k);
				// Must not unwrap: a crafted remote filename must never panic (finding #3).
				if let Ok(v) = content_disposition.parse() {
					headers.append(k, v);
				}
			}
		}
	}
}
impl RequestContext {
	async fn encode(
		mut self,
		resp: reqwest::Response,
		mut is_img: bool,
	) -> Result<(axum::http::StatusCode, HeaderMap, axum::body::Body), axum::response::Response> {
		let mut is_svg = false;
		let mut content_type = None;
		if let Some(media) = self.headers.get("Content-Type") {
			let s = String::from_utf8_lossy(media.as_bytes());
			if s.as_ref() == "image/svg+xml" {
				is_svg = true;
			} else {
				content_type = Some(s);
			}
		}
		let status = resp.status();
		// リモートがエラーを返したら本文をデコードせずエラーを返す
		if !status.is_success() {
			return Err(self.remote_error_response(status));
		}
		let resp = PreDataStream::new(resp).await;
		if let Some(Ok(head)) = resp.head.as_ref() {
			//utf8にパースできて空白文字を削除した後の先頭部分が<svgの場合はsvg
			if std::str::from_utf8(&head)
				.map(|s| s.trim().starts_with("<svg"))
				.unwrap_or(false)
			{
				is_svg = true;
			} else {
				self.codec = image::guess_format(head).map_err(|e| Some(e));
				if self.codec.is_err() {
					if let Some(content_type) = content_type.as_ref() {
						match content_type.as_ref() {
							"image/x-targa" | "image/x-tga" => {
								self.codec = Ok(image::ImageFormat::Tga)
							}
							_ => {}
						}
					}
					if head.starts_with(&[0xFF, 0x0A])
						|| head.starts_with(&[
							0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
						]) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/jxl".parse().unwrap());
					}
					if head.starts_with(&[0xFF, 0x4F, 0xFF, 0x51])
						|| head.starts_with(&[
							0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
						]) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/jp2".parse().unwrap());
					}
					if head.starts_with(&[0x49, 0x49, 0xBC]) {
						is_img = true;
						self.headers.remove("Content-Type");
						self.headers
							.append("Content-Type", "image/jxr".parse().unwrap());
					}
				}
			}
		}
		if is_svg {
			self.load_all(resp).await?;
			// SVG parsing/rendering is CPU-bound and attacker-controlled:
			// run it on the blocking pool with a deadline so it cannot
			// occupy a tokio worker or the semaphore indefinitely (H-01).
			let src_bytes = std::mem::take(&mut self.src_bytes);
			let fontdb = self.fontdb.clone();
			let size_hint = self.image_size_hint();
			let max_decode_pixels = self.max_decode_pixels();
			let timeout_ms = self.config.timeout;
			if let Ok(img) = crate::svg::render_svg_blocking(
				src_bytes,
				fontdb,
				size_hint,
				max_decode_pixels,
				timeout_ms,
			)
			.await
			{
				self.headers.remove("Content-Length");
				self.headers.remove("Content-Range");
				self.headers.remove("Accept-Ranges");
				self.headers.remove("Cache-Control");
				self.headers.append(
					"Cache-Control",
					"max-age=31536000, immutable".parse().unwrap(),
				);
				return Err(self.response_img(img));
			} else {
				// Never reflect the remote SVG bytes inline: serving attacker XML
				// as image/svg+xml from the proxy origin is a stored-XSS vector
				// (finding #7). Fail closed, or serve the dummy image when the
				// caller asked for a fallback.
				self.headers.remove("Content-Type");
				self.headers.remove("Content-Length");
				self.headers.remove("Content-Range");
				self.headers.remove("Accept-Ranges");
				if self.parms.fallback.is_some() {
					self.headers
						.append("Content-Type", "image/png".parse().unwrap());
					return Err((
						axum::http::StatusCode::OK,
						self.headers.clone(),
						(*self.dummy_img).clone(),
					)
						.into_response());
				}
				self.headers
					.append("X-Proxy-Error", "SvgEncodeError".parse().unwrap());
				return Err(
					(axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response()
				);
			}
		} else if is_img || self.codec.is_ok() {
			self.headers.remove("Content-Length");
			self.headers.remove("Content-Range");
			self.headers.remove("Accept-Ranges");
			self.load_all(resp).await?;
			let dummy_img = self.dummy_img.clone();
			let is_fallback = self.parms.fallback.is_some();
			let timeout_ms = self.config.timeout;
			let mut header = self.headers.clone();
			let mut handle = self;
			// Raster decode/resize/encode is CPU-bound and attacker-controlled,
			// same as SVG rendering (H-01): bound it with a deadline so a
			// pathological image cannot occupy a blocking-pool thread
			// indefinitely (code-review finding: this path had no timeout at
			// all, unlike the SVG path). Same caveat as svg.rs's
			// render_svg_blocking: abort() cannot preempt an already-running
			// closure, but does stop one still queued on the blocking pool.
			let task =
				tokio::runtime::Handle::current().spawn_blocking(move || handle.encode_img());
			let abort_handle = task.abort_handle();
			let resp = match tokio::time::timeout(
				std::time::Duration::from_millis(timeout_ms.max(1)),
				task,
			)
			.await
			{
				Ok(Ok(resp)) => resp,
				Ok(Err(_join_error)) => {
					header.append(
						"X-Proxy-Error",
						format!("ImageEncodeThread").parse().unwrap(),
					);
					return Err(if is_fallback {
						header.remove("Content-Type");
						header.append("Content-Type", "image/png".parse().unwrap());
						(axum::http::StatusCode::OK, header, (*dummy_img).clone()).into_response()
					} else {
						(axum::http::StatusCode::INTERNAL_SERVER_ERROR, header).into_response()
					});
				}
				Err(_elapsed) => {
					abort_handle.abort();
					header.append("X-Proxy-Error", "ImageEncodeTimeout".parse().unwrap());
					return Err(if is_fallback {
						header.remove("Content-Type");
						header.append("Content-Type", "image/png".parse().unwrap());
						(axum::http::StatusCode::OK, header, (*dummy_img).clone()).into_response()
					} else {
						(axum::http::StatusCode::GATEWAY_TIMEOUT, header).into_response()
					});
				}
			};
			if is_fallback {
				return Err(if resp.status() == axum::http::StatusCode::OK {
					resp
				} else {
					header.remove("Content-Type");
					header.append("Content-Type", "image/png".parse().unwrap());
					(axum::http::StatusCode::OK, header, (*dummy_img).clone()).into_response()
				});
			}
			return Err(resp);
		}
		// browsersafe(音声/動画)以外はリモートバイトを中継しない: ダミー画像を返す。
		// Content-Type 不明もここに含める (省略による回避を防ぐ)。
		let mut is_browsersafe = false;
		if let Some(media) = self.headers.get("Content-Type") {
			let s = String::from_utf8_lossy(media.as_bytes());
			if crate::browsersafe::FILE_TYPE_BROWSERSAFE.contains(&s.as_ref()) {
				is_browsersafe = true;
			}
		}
		if !is_browsersafe {
			self.headers.remove("Content-Type");
			self.headers.remove("Content-Length");
			self.headers.remove("Content-Range");
			self.headers.remove("Accept-Ranges");
			self.headers
				.append("Content-Type", "image/png".parse().unwrap());
			self.headers
				.append("X-Proxy-Error", "NonBrowsersafeType".parse().unwrap());
			return Err((
				axum::http::StatusCode::OK,
				self.headers.clone(),
				(*self.dummy_img).clone(),
			)
				.into_response());
		}
		let body = axum::body::Body::from_stream(resp);
		// ここまで来た時点で status.is_success() は保証されている
		// (encode()冒頭でエラーは remote_error_response に流れる)。
		self.headers.remove("Cache-Control");
		self.headers.append(
			"Cache-Control",
			"max-age=31536000, immutable".parse().unwrap(),
		);
		if status == reqwest::StatusCode::PARTIAL_CONTENT {
			Ok((
				axum::http::StatusCode::PARTIAL_CONTENT,
				self.headers.clone(),
				body,
			))
		} else {
			Ok((axum::http::StatusCode::OK, self.headers.clone(), body))
		}
	}
	/// リモートからのエラーはエラーとして返す
	/// Content-Length/Content-Range を残すと空ボディと矛盾して壊れるから消す
	fn remote_error_response(mut self, status: reqwest::StatusCode) -> axum::response::Response {
		self.headers.remove("Content-Length");
		self.headers.remove("Content-Range");
		self.headers.remove("Accept-Ranges");
		self.headers.append(
			"X-Proxy-Error",
			format!("status:{}", status.as_u16()).parse().unwrap(),
		);
		if self.parms.fallback.is_some() {
			self.headers.remove("Content-Type");
			self.headers
				.append("Content-Type", "image/png".parse().unwrap());
			(
				axum::http::StatusCode::OK,
				self.headers.clone(),
				(*self.dummy_img).clone(),
			)
				.into_response()
		} else {
			let status = match status {
				reqwest::StatusCode::BAD_REQUEST => axum::http::StatusCode::BAD_REQUEST,
				reqwest::StatusCode::FORBIDDEN => axum::http::StatusCode::FORBIDDEN,
				reqwest::StatusCode::NOT_FOUND => axum::http::StatusCode::NOT_FOUND,
				reqwest::StatusCode::REQUEST_TIMEOUT => axum::http::StatusCode::GATEWAY_TIMEOUT,
				reqwest::StatusCode::GONE => axum::http::StatusCode::GONE,
				reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS => {
					axum::http::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
				}
				_ => axum::http::StatusCode::BAD_GATEWAY,
			};
			(status, self.headers.clone()).into_response()
		}
	}
	async fn load_all(&mut self, mut resp: PreDataStream) -> Result<(), axum::response::Response> {
		let len_hint = resp
			.content_length
			.unwrap_or(2048.min(self.config.max_size));
		if len_hint > self.config.max_size {
			self.headers.append(
				"X-Proxy-Error",
				format!("lengthHint:{}>{}", len_hint, self.config.max_size)
					.parse()
					.unwrap(),
			);
			return Err((axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response());
		}
		// Never trust the remote Content-Length hint for pre-allocation
		// (finding #5): cap the initial reservation and let the buffer grow
		// as bytes actually arrive (still bounded by max_size below).
		const INITIAL_CAP: u64 = 16 * 1024;
		let mut response_bytes = Vec::with_capacity(len_hint.min(INITIAL_CAP) as usize);
		while let Some(x) = resp.next().await {
			match x {
				Ok(b) => {
					if response_bytes.len() + b.len() > self.config.max_size as usize {
						self.headers.append(
							"X-Proxy-Error",
							format!(
								"length:{}>{}",
								response_bytes.len() + b.len(),
								self.config.max_size
							)
							.parse()
							.unwrap(),
						);
						return Err((axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
							.into_response());
					}
					response_bytes.extend_from_slice(&b);
				}
				Err(e) => {
					self.headers
						.append("X-Proxy-Error", format!("LoadAll:{:?}", e).parse().unwrap());
					return Err((
						axum::http::StatusCode::BAD_GATEWAY,
						self.headers.clone(),
						format!("{:?}", e),
					)
						.into_response());
				}
			}
		}
		self.src_bytes = response_bytes;
		Ok(())
	}
}
struct PreDataStream {
	content_length: Option<u64>,
	head: Option<Result<axum::body::Bytes, reqwest::Error>>,
	last: Pin<
		Box<
			dyn futures::stream::Stream<Item = Result<axum::body::Bytes, reqwest::Error>>
				+ Send
				+ Sync,
		>,
	>,
}
impl PreDataStream {
	async fn new(value: reqwest::Response) -> Self {
		let content_length = value.content_length();
		let mut stream = value.bytes_stream();
		let head = stream.next().await;
		Self {
			content_length,
			head,
			last: Box::pin(stream),
		}
	}
}
impl futures::stream::Stream for PreDataStream {
	type Item = Result<axum::body::Bytes, reqwest::Error>;

	fn poll_next(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Option<Self::Item>> {
		let mut r = self.as_mut();
		if let Some(d) = r.head.take() {
			return std::task::Poll::Ready(Some(d));
		}
		r.last.as_mut().poll_next(cx)
	}
}
