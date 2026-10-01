use core::str;
use std::{
	io::Write,
	net::SocketAddr,
	path::{Path, PathBuf},
	pin::Pin,
	str::FromStr,
	sync::Arc,
	time::Duration,
};

use axum::{http::HeaderMap, response::IntoResponse, Router};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

pub use media_proxy_rs::FilterType;

mod ssrf;

/// Bounds concurrent fetch+encode work so that per-request memory budgets
/// (#4/#5) cannot be multiplied without limit (finding #6). Auth and
/// per-client rate limiting remain deployment decisions.
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
	/// Defaults to `0666` when unset.
	unix_socket_permissions: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RequestParams {
	url: String,
	#[serde(rename = "static")]
	r#static: Option<String>,
	emoji: Option<String>,
	avatar: Option<String>,
	preview: Option<String>,
	badge: Option<String>,
	fallback: Option<String>,
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
		let default_config = ConfigFile {
			bind_addr: "0.0.0.0:12766".to_owned(),
			timeout: 10000,
			user_agent: "https://github.com/yojo-art/media-proxy-rs".to_owned(),
			max_size: 256 * 1024 * 1024,
			proxy: None,
			filter_type: FilterType::Triangle,
			max_pixels: 2048,
			append_headers: [
				"Content-Security-Policy:default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'".to_owned(),
				"Access-Control-Allow-Origin:*".to_owned(),
			]
			.to_vec(),
			load_system_fonts: true,
			webp_quality: 75f32,
			encode_avif: false,
			allowed_networks: None,
			blocked_networks: None,
			blocked_hosts: None,
			unix_socket_permissions: None,
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
	if let Err(e) = crate::ssrf::validate_network_config(&config) {
		eprintln!("invalid network configuration: {}", e);
		std::process::exit(1);
	}
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
			eprintln!("WARNING: proxy is configured ({}). Connect-time SSRF re-validation does not apply to fetch targets in this mode; ensure the proxy itself enforces an equivalent SSRF policy.", url);
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
	let client = client.redirect(reqwest::redirect::Policy::none());
	let client = client.dns_resolver(std::sync::Arc::new(crate::ssrf::ValidatingResolver::new(
		config.clone(),
	)));
	let timeout_dur = std::time::Duration::from_millis(config.timeout);
	let client = client
		.connect_timeout(timeout_dur)
		.read_timeout(timeout_dur);
	let client = client.build().unwrap();

	let processor_cfg = media_proxy_rs::ProcessorConfig {
		max_pixels: config.max_pixels,
		max_decode_pixels: config.max_size / 4,
		filter_type: config.filter_type,
		webp_quality: config.webp_quality,
		encode_avif: config.encode_avif,
		load_system_fonts: config.load_system_fonts,
		font_dirs: vec![std::path::PathBuf::from("asset/font/")],
		default_font_family: None,
	};
	let processor = Arc::new(media_proxy_rs::Processor::new(processor_cfg));
	processor.preload_fonts();

	let arg_tup = (client, config, dummy_png, processor);
	rt.block_on(async {
		let bind_addr = arg_tup.1.bind_addr.clone();
		let app = Router::new();
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
		let app = app.layer(tower_http::catch_panic::CatchPanicLayer::new());
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

enum BindTarget {
	Tcp(SocketAddr),
	Unix(PathBuf),
}

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

enum CheckUrlError {
	InvalidUrl,
	UnsupportedScheme,
	PolicyDenied,
	ResolveFailed,
}

impl CheckUrlError {
	fn as_header(&self) -> &'static str {
		match self {
			CheckUrlError::InvalidUrl => "InvalidUrl",
			CheckUrlError::UnsupportedScheme => "UnsupportedScheme",
			CheckUrlError::PolicyDenied => "PolicyDenied",
			CheckUrlError::ResolveFailed => "ResolveFailed",
		}
	}
}

async fn check_url(config: &Arc<ConfigFile>, url: impl AsRef<str>) -> Result<(), CheckUrlError> {
	let u = reqwest::Url::from_str(url.as_ref()).map_err(|e| {
		eprintln!("check_url: invalid url {:?}: {:?}", url.as_ref(), e);
		CheckUrlError::InvalidUrl
	})?;
	match u.scheme().to_lowercase().as_str() {
		"http" | "https" => {}
		scheme => {
			eprintln!(
				"check_url: unsupported scheme {:?} for {:?}",
				scheme,
				url.as_ref()
			);
			return Err(CheckUrlError::UnsupportedScheme);
		}
	}
	let host = u.host_str().ok_or(CheckUrlError::InvalidUrl)?;
	if crate::ssrf::is_host_blocked(config.blocked_hosts.as_ref(), host) {
		return Err(CheckUrlError::PolicyDenied);
	}
	let ips = crate::ssrf::cached_lookup_host(host).await.map_err(|e| {
		eprintln!("check_url: dns resolve failed for {:?}: {:?}", host, e);
		CheckUrlError::ResolveFailed
	})?;
	crate::ssrf::validate_resolved_ips(config, &ips).map_err(|e| {
		eprintln!("check_url: policy denied for {:?}: {:?}", host, e);
		CheckUrlError::PolicyDenied
	})
}

async fn get_file(
	_path: Option<axum::extract::Path<String>>,
	client_headers: axum::http::HeaderMap,
	(client, config, dummy_img, processor): (
		reqwest::Client,
		Arc<ConfigFile>,
		Arc<Vec<u8>>,
		Arc<media_proxy_rs::Processor>,
	),
	axum::extract::Query(q): axum::extract::Query<RequestParams>,
) -> Result<(axum::http::StatusCode, HeaderMap, axum::body::Body), axum::response::Response> {
	let mut headers = HeaderMap::new();
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
	let vary = if config.encode_avif {
		"Accept,Range"
	} else {
		"Range"
	};
	headers.append("Vary", vary.parse().unwrap());
	let time = chrono::Utc::now();
	if let Err(e) = check_url(&config, &q.url).await {
		headers.append(
			"X-Proxy-Error",
			reqwest::header::HeaderValue::from_static(e.as_header()),
		);
		if q.fallback.is_some() {
			headers.append("Content-Type", "image/png".parse().unwrap());
			return Err((axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response());
		}
		return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
	}
	println!(
		"check_url {}ms",
		(chrono::Utc::now() - time).num_milliseconds()
	);

	// 変換に進む場合は spawn_blocking の中へ移し、処理が終わるまで持ち続ける。
	// abort() は実行中の blocking の処理を止められないため。
	let permit =
		match tokio::time::timeout(Duration::from_secs(30), FETCH_SEMAPHORE.acquire()).await {
			Ok(Ok(permit)) => permit,
			_ => {
				headers.append("X-Proxy-Error", "Overloaded".parse().unwrap());
				return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, headers).into_response());
			}
		};

	const MAX_REDIRECTS: u8 = 5;
	let mut current_url = q.url.clone();
	let mut redirects: u8 = 0;
	let resp = loop {
		let req = client.get(&current_url);
		let req = req.header("User-Agent", config.user_agent.clone());
		let req = if let Some(range) = client_headers.get("Range") {
			req.header("Range", range.as_bytes())
		} else {
			req
		};
		let resp = match req.send().await {
			Ok(resp) => resp,
			Err(e) => {
				eprintln!("fetch failed for {:?}: {:?}", current_url, e);
				if q.fallback.is_some() {
					headers.append("Content-Type", "image/png".parse().unwrap());
					return Err(
						(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
					);
				}
				headers.append("X-Proxy-Error", "FetchFailed".parse().unwrap());
				return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
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
		if let Err(e) = check_url(&config, &next_str).await {
			headers.append(
				"X-Proxy-Error",
				reqwest::header::HeaderValue::from_static(e.as_header()),
			);
			if q.fallback.is_some() {
				headers.append("Content-Type", "image/png".parse().unwrap());
				return Err(
					(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
				);
			}
			return Err((axum::http::StatusCode::BAD_REQUEST, headers).into_response());
		}
		current_url = next_str;
		redirects += 1;
	};

	let resp = {
		let ct_is_img = resp
			.headers()
			.get("Content-Type")
			.map(|m| {
				String::from_utf8_lossy(m.as_bytes())
					.to_lowercase()
					.starts_with("image/")
			})
			.unwrap_or(false);
		if ct_is_img && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
			match client
				.get(&current_url)
				.header("User-Agent", config.user_agent.clone())
				.send()
				.await
			{
				Ok(full) => full,
				Err(_) => resp,
			}
		} else {
			resp
		}
	};
	let remote_headers = resp.headers().clone();
	add_remote_header("Content-Disposition", &mut headers, &remote_headers);
	add_remote_header("Content-Type", &mut headers, &remote_headers);

	let status = resp.status();
	if !status.is_success() {
		return Err(remote_error_response(status, headers, &q, dummy_img));
	}

	let mut is_accept_avif = false;
	if config.encode_avif {
		if let Some(accept) = client_headers.get("Accept") {
			if let Ok(accept) = std::str::from_utf8(accept.as_bytes()) {
				for e in accept.split(",") {
					if e.trim().eq_ignore_ascii_case("image/avif") {
						is_accept_avif = true;
					}
				}
			}
		}
	}

	headers.append("Cache-Control", "max-age=300".parse().unwrap());
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

	let mut stream = PreDataStream::new(resp).await;
	let head = stream
		.head
		.as_ref()
		.and_then(|h| h.as_ref().ok().map(|b| &b[..]))
		.unwrap_or(&[]);
	let content_type_hint = remote_headers
		.get("Content-Type")
		.and_then(|v| std::str::from_utf8(v.as_bytes()).ok().map(|s| s.to_owned()));
	let content_type_lower = content_type_hint.as_ref().map(|ct| ct.to_lowercase());
	let detected = media_proxy_rs::detect(head, content_type_hint.as_deref());

	let is_unknown_image = matches!(detected, media_proxy_rs::DetectedFormat::Unknown)
		&& content_type_lower
			.as_ref()
			.map_or(false, |ct| ct.starts_with("image/"));

	if matches!(detected, media_proxy_rs::DetectedFormat::Unknown) && !is_unknown_image {
		let is_browsersafe = content_type_lower.as_ref().map_or(false, |ct| {
			media_proxy_rs::browsersafe::FILE_TYPE_BROWSERSAFE.contains(&ct.as_str())
		});
		if is_browsersafe {
			add_remote_header("Content-Length", &mut headers, &remote_headers);
			add_remote_header("Content-Range", &mut headers, &remote_headers);
			add_remote_header("Accept-Ranges", &mut headers, &remote_headers);
			headers.remove("Cache-Control");
			headers.append(
				"Cache-Control",
				"max-age=31536000, immutable".parse().unwrap(),
			);
			let body = axum::body::Body::from_stream(stream);
			if status == reqwest::StatusCode::PARTIAL_CONTENT {
				return Ok((axum::http::StatusCode::PARTIAL_CONTENT, headers, body));
			}
			return Ok((axum::http::StatusCode::OK, headers, body));
		}
		headers.remove("Content-Type");
		headers.remove("Content-Length");
		headers.remove("Content-Range");
		headers.remove("Accept-Ranges");
		headers.append("Content-Type", "image/png".parse().unwrap());
		headers.append("X-Proxy-Error", "NonBrowsersafeType".parse().unwrap());
		return Err((axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response());
	}

	let is_fallback = q.fallback.is_some();
	let src_bytes = match load_all(&mut stream, config.max_size, &mut headers).await {
		Ok(bytes) => bytes,
		Err(resp) => {
			if is_fallback {
				let mut headers = headers.clone();
				headers.remove("Content-Type");
				headers.append("Content-Type", "image/png".parse().unwrap());
				return Err(
					(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
				);
			}
			return Err(resp);
		}
	};
	let opts = media_proxy_rs::ProcessOptions {
		emoji: q.emoji.is_some(),
		avatar: q.avatar.is_some(),
		r#static: q.r#static.is_some(),
		preview: q.preview.is_some(),
		badge: q.badge.is_some(),
		content_type_hint,
		accept_avif: is_accept_avif,
	};
	let processor = processor.clone();
	let task = tokio::runtime::Handle::current().spawn_blocking(move || {
		let _permit = permit;
		processor.process(&src_bytes, &opts)
	});
	let abort_handle = task.abort_handle();
	let result =
		match tokio::time::timeout(Duration::from_millis(config.timeout.max(1)), task).await {
			Ok(Ok(result)) => result,
			Ok(Err(_join_error)) => {
				headers.append("X-Proxy-Error", "ImageEncodeThread".parse().unwrap());
				return Err(if is_fallback {
					headers.remove("Content-Type");
					headers.append("Content-Type", "image/png".parse().unwrap());
					(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
				} else {
					(axum::http::StatusCode::INTERNAL_SERVER_ERROR, headers).into_response()
				});
			}
			Err(_elapsed) => {
				abort_handle.abort();
				headers.append("X-Proxy-Error", "ImageEncodeTimeout".parse().unwrap());
				return Err(if is_fallback {
					headers.remove("Content-Type");
					headers.append("Content-Type", "image/png".parse().unwrap());
					(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
				} else {
					(axum::http::StatusCode::GATEWAY_TIMEOUT, headers).into_response()
				});
			}
		};
	match result {
		Ok(encoded) => {
			headers.remove("Content-Type");
			headers.append("Content-Type", encoded.content_type.parse().unwrap());
			headers.remove("Cache-Control");
			if let Some(warning) = &encoded.warning {
				headers.append(
					"X-Proxy-Error",
					error_header_value(warning.clone(), "AnimEncode"),
				);
			} else {
				headers.append(
					"Cache-Control",
					"max-age=31536000, immutable".parse().unwrap(),
				);
			}
			headers.remove("Content-Length");
			headers.remove("Content-Range");
			headers.remove("Accept-Ranges");
			disposition_ext(&mut headers, encoded.ext);
			return Ok((
				axum::http::StatusCode::OK,
				headers,
				axum::body::Body::from(encoded.bytes),
			));
		}
		Err(e) => {
			headers.append(
				"X-Proxy-Error",
				error_header_value(e.to_string(), e.kind.as_str()),
			);
			let status = match e.kind {
				media_proxy_rs::ProcessErrorKind::BadgeSkipped => axum::http::StatusCode::NOT_FOUND,
				media_proxy_rs::ProcessErrorKind::Unsupported
				| media_proxy_rs::ProcessErrorKind::DecodeLimit
				| media_proxy_rs::ProcessErrorKind::Decode
				| media_proxy_rs::ProcessErrorKind::Encode => axum::http::StatusCode::BAD_GATEWAY,
			};
			return Err(if is_fallback {
				headers.remove("Content-Type");
				headers.append("Content-Type", "image/png".parse().unwrap());
				(axum::http::StatusCode::OK, headers, (*dummy_img).clone()).into_response()
			} else {
				(status, headers).into_response()
			});
		}
	}
}

fn add_remote_header(
	key: &'static str,
	headers: &mut HeaderMap,
	remote_headers: &reqwest::header::HeaderMap,
) {
	if key == "Content-Type" {
		if let Some(v) = remote_headers.get(key) {
			if let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
				headers.append(key, value);
			}
		}
		return;
	}
	for v in remote_headers.get_all(key) {
		if let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
			headers.append(key, value);
		}
	}
}

/// リモートからのエラーはエラーとして返す。
fn remote_error_response(
	status: reqwest::StatusCode,
	mut headers: HeaderMap,
	q: &RequestParams,
	dummy_img: Arc<Vec<u8>>,
) -> axum::response::Response {
	headers.remove("Content-Length");
	headers.remove("Content-Range");
	headers.remove("Accept-Ranges");
	headers.remove("Cache-Control");
	headers.append(
		"X-Proxy-Error",
		format!("status:{}", status.as_u16()).parse().unwrap(),
	);
	if q.fallback.is_some() {
		headers.remove("Content-Type");
		headers.append("Content-Type", "image/png".parse().unwrap());
		(
			axum::http::StatusCode::OK,
			headers.clone(),
			(*dummy_img).clone(),
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
		(status, headers.clone()).into_response()
	}
}

async fn load_all(
	resp: &mut PreDataStream,
	max_size: u64,
	headers: &mut HeaderMap,
) -> Result<Vec<u8>, axum::response::Response> {
	let len_hint = resp.content_length.unwrap_or(2048.min(max_size));
	if len_hint > max_size {
		headers.append(
			"X-Proxy-Error",
			format!("lengthHint:{}>{}", len_hint, max_size)
				.parse()
				.unwrap(),
		);
		return Err((axum::http::StatusCode::BAD_GATEWAY, headers.clone()).into_response());
	}
	const INITIAL_CAP: u64 = 16 * 1024;
	let mut response_bytes = Vec::with_capacity(len_hint.min(INITIAL_CAP) as usize);
	while let Some(x) = resp.next().await {
		match x {
			Ok(b) => {
				if response_bytes.len() + b.len() > max_size as usize {
					headers.append(
						"X-Proxy-Error",
						format!("length:{}>{}", response_bytes.len() + b.len(), max_size)
							.parse()
							.unwrap(),
					);
					return Err(
						(axum::http::StatusCode::BAD_GATEWAY, headers.clone()).into_response()
					);
				}
				response_bytes.extend_from_slice(&b);
			}
			Err(e) => {
				eprintln!("load_all failed: {:?}", e);
				headers.append(
					"X-Proxy-Error",
					error_header_value(format!("LoadAll:{:?}", e), "LoadAll"),
				);
				return Err((axum::http::StatusCode::BAD_GATEWAY, headers.clone()).into_response());
			}
		}
	}
	Ok(response_bytes)
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

fn error_header_value(msg: String, fallback: &'static str) -> reqwest::header::HeaderValue {
	reqwest::header::HeaderValue::from_bytes(msg.as_bytes())
		.unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(fallback))
}

fn disposition_ext(headers: &mut HeaderMap, ext: &str) {
	let k = "Content-Disposition";
	if let Some(cd) = headers.get(k) {
		let s = std::str::from_utf8(cd.as_bytes());
		if let Ok(s) = s {
			let cd = mailparse::parse_content_disposition(s);
			let cd_utf8 = cd.params.get("filename*");
			let mut name = None;
			if let Some(cd_utf8) = cd_utf8 {
				if cd_utf8.len() > 7 && cd_utf8.as_bytes()[..7].eq_ignore_ascii_case(b"UTF-8''") {
					name = urlencoding::decode(&cd_utf8[7..])
						.map(|s| s.to_string())
						.ok();
				}
			}
			if name.is_none() {
				if let Some(filename) = cd.params.get("filename") {
					let m_filename = format!("_:{}", filename);
					let parsed = mailparse::parse_header(m_filename.as_bytes());
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
			if let Ok(v) = content_disposition.parse() {
				headers.append(k, v);
			}
		}
	}
}
