use core::str;
use std::{io::Write, net::SocketAddr, pin::Pin, str::FromStr, sync::Arc};

use axum::{http::HeaderMap, response::IntoResponse, Router};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

mod img;
mod svg;
mod browsersafe;
mod image_test;

#[derive(Debug,Serialize,Deserialize)]
pub struct ConfigFile{
	bind_addr: String,
	timeout:u64,
	user_agent:String,
	max_size:u64,
	proxy:Option<String>,
	filter_type:FilterType,
	max_pixels:u32,
	append_headers:Vec<String>,
	load_system_fonts:bool,
	webp_quality:f32,
	encode_avif:bool,
	allowed_networks:Option<Vec<String>>,
	blocked_networks:Option<Vec<String>>,
	blocked_hosts:Option<Vec<String>>,
}
#[derive(Debug, Deserialize)]
pub struct RequestParams{
	url: String,
	//#[serde(rename = "static")]
	r#static:Option<String>,
	emoji:Option<String>,
	avatar:Option<String>,
	preview:Option<String>,
	badge:Option<String>,
	fallback:Option<String>,
}
#[derive(Clone, Copy,Debug,Serialize,Deserialize)]
enum FilterType{
	Nearest,
	Triangle,
	CatmullRom,
	Gaussian,
	Lanczos3,
}
impl Into<image::imageops::FilterType> for FilterType{
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
impl Into<fast_image_resize::FilterType> for FilterType{
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
	use tokio::signal;
	use futures::{future::FutureExt,pin_mut};
	let ctrl_c = async {
		signal::ctrl_c()
			.await
			.expect("failed to install Ctrl+C handler");
	}.fuse();

	#[cfg(unix)]
	let terminate = async {
		signal::unix::signal(signal::unix::SignalKind::terminate())
			.expect("failed to install signal handler")
			.recv()
			.await;
	}.fuse();
	#[cfg(not(unix))]
	let terminate = std::future::pending::<()>().fuse();
	pin_mut!(ctrl_c, terminate);
	futures::select!{
		_ = ctrl_c => {},
		_ = terminate => {},
	}
}
fn main() {
	let config_path=match std::env::var("MEDIA_PROXY_CONFIG_PATH"){
		Ok(path)=>{
			if path.is_empty(){
				"config.json".to_owned()
			}else{
				path
			}
		},
		Err(_)=>"config.json".to_owned()
	};
	if !std::path::Path::new(&config_path).exists(){
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
		};
		let default_config=serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path).expect("create default config.json").write_all(default_config.as_bytes()).unwrap();
	}
	let mut config:ConfigFile=serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();
	if let Ok(networks)=std::env::var("MEDIA_PROXY_ALLOWED_NETWORKS"){
		let mut allowed_networks=config.allowed_networks.take().unwrap_or_default();
		for networks in networks.split(","){
			allowed_networks.push(networks.to_owned());
		}
		config.allowed_networks.replace(allowed_networks);
	}
	if let Ok(networks)=std::env::var("MEDIA_PROXY_BLOCKED_NETWORKS"){
		let mut blocked_networks=config.blocked_networks.take().unwrap_or_default();
		for networks in networks.split(","){
			blocked_networks.push(networks.to_owned());
		}
		config.blocked_networks.replace(blocked_networks);
	}
	if let Ok(networks)=std::env::var("MEDIA_PROXY_BLOCKED_HOSTS"){
		let mut blocked_hosts=config.blocked_hosts.take().unwrap_or_default();
		for networks in networks.split(","){
			blocked_hosts.push(networks.to_owned());
		}
		config.blocked_hosts.replace(blocked_hosts);
	}
	let dummy_png=Arc::new(include_bytes!("../asset/dummy.png").to_vec());
	let config=Arc::new(config);
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let client=reqwest::ClientBuilder::new();
	let client=match &config.proxy{
		Some(url)=>client.proxy(reqwest::Proxy::http(url).unwrap()),
		None=>client,
	};
	let client=client.build().unwrap();
	let mut fontdb=resvg::usvg::fontdb::Database::new();
	if config.load_system_fonts{
		fontdb.load_system_fonts();
	}
	if std::path::Path::new("asset/font/").exists(){
		fontdb.load_fonts_dir("asset/font/");
	}
	fontdb.load_font_source(resvg::usvg::fontdb::Source::Binary(Arc::new(include_bytes!("../asset/font/Aileron-Light.otf"))));
	let fontdb=Arc::new(fontdb);
	let arg_tup=(client,config,dummy_png,fontdb);
	rt.block_on(async{
		let http_addr:SocketAddr = arg_tup.1.bind_addr.parse().unwrap();
		let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
		let app = Router::new();
		let arg_tup0=arg_tup.clone();
		let app=app.route("/",axum::routing::get(move|headers,parms|get_file(None,headers,arg_tup0.clone(),parms)));
		let app=app.route("/{*path}",axum::routing::get(move|path,headers,parms|get_file(Some(path),headers,arg_tup.clone(),parms)));
		axum::serve(listener,app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown_signal()).await.unwrap();
	});
}
async fn check_url(config:&Arc<ConfigFile>,url:impl AsRef<str>)->Result<(),String>{
	let u=reqwest::Url::from_str(url.as_ref()).map_err(|e|format!("{:?}",e))?;
	match u.scheme().to_lowercase().as_str(){
		"http"|"https"=>{},
		scheme=>return Err(format!("scheme: {}",scheme))
	}
	let host=u.host_str().ok_or_else(||"no host".to_owned())?;
	if let Some(blocked_hosts)=&config.blocked_hosts{
		if blocked_hosts.contains(&host.to_lowercase()){
			return Err("Blocked address".to_owned());
		}
	}
	use std::net::{SocketAddr, ToSocketAddrs};
	use iprange::IpRange;
	use ipnet::Ipv4Net;
	let ips=format!("{}:{}",host,u.port_or_known_default().unwrap()).to_socket_addrs().map_err(|e|format!("{:?} {}",e,host))?;
	let ipv4_private_range: IpRange<Ipv4Net> = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
		.iter()
		.map(|s| s.parse().unwrap())
		.collect();
	let allow_ips=config.allowed_networks.as_ref().map(|ips|{
		ips.iter()
		.map(|s| s.parse().unwrap())
		.collect::<IpRange<Ipv4Net>>()
	});
	let block_ips=config.blocked_networks.as_ref().map(|ips|{
		ips.iter()
		.map(|s| s.parse().unwrap())
		.collect::<IpRange<Ipv4Net>>()
	});
	for ip in ips{
		match ip{
			SocketAddr::V4(v4) => {
				if let Some(block_ips)=&block_ips{
					if block_ips.contains(v4.ip()){
						return Err("Blocked address".to_owned());
					}
				}
				if ipv4_private_range.contains(v4.ip()){
					let allow=if let Some(allow_ips)=&allow_ips{
						allow_ips.contains(v4.ip())
					}else{
						false
					};
					if !allow{
						return Err("Blocked address".to_owned());
					}
				}
			},
			SocketAddr::V6(v6) => {
				if v6.ip().is_multicast()||v6.ip().is_unicast_link_local(){
					return Err("Blocked address".to_owned());
				}
			},
		}
	}
	Ok(())
}
async fn get_file(
	_path:Option<axum::extract::Path<String>>,
	client_headers:axum::http::HeaderMap,
	(client,config,dummy_img,fontdb):(reqwest::Client,Arc<ConfigFile>,Arc<Vec<u8>>,Arc<resvg::usvg::fontdb::Database>),
	axum::extract::Query(q):axum::extract::Query<RequestParams>,
)->Result<(axum::http::StatusCode,HeaderMap,axum::body::Body),axum::response::Response>{
	println!("{}\t{}\tavatar:{:?}\tpreview:{:?}\tbadge:{:?}\temoji:{:?}\tstatic:{:?}\tfallback:{:?}",
		chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
		q.url,
		q.avatar,
		q.preview,
		q.badge,
		q.emoji,
		q.r#static,
		q.fallback,
	);
	let mut headers=HeaderMap::new();
	if let Ok(url)=q.url.parse(){
		headers.append("X-Remote-Url",url);
	}
	if config.encode_avif{
		headers.append("Vary","Accept,Range".parse().unwrap());
	}
	let time=chrono::Utc::now();
	if let Err(s)=check_url(&config,&q.url).await{
		if let Ok(v)=s.parse(){
			headers.append("X-Proxy-Error",v);
		}
		if q.fallback.is_some(){
			headers.append("Content-Type","image/png".parse().unwrap());
			return Err((axum::http::StatusCode::OK,headers,(*dummy_img).clone()).into_response());
		}
		return Err((axum::http::StatusCode::BAD_REQUEST,headers).into_response())
	};

	println!("check_url {}ms",(chrono::Utc::now()-time).num_milliseconds());
	let req=client.get(&q.url);
	let req=req.timeout(std::time::Duration::from_millis(config.timeout));
	let req=req.header("User-Agent",config.user_agent.clone());
	let req=if let Some(range)=client_headers.get("Range"){
		req.header("Range",range.as_bytes())
	}else{
		req
	};
	let resp=match req.send().await{
		Ok(resp) => resp,
		Err(e) => {
			if q.fallback.is_some(){
				headers.append("Content-Type","image/png".parse().unwrap());
				return Err((axum::http::StatusCode::OK,headers,(*dummy_img).clone()).into_response());
			}
			return Err((axum::http::StatusCode::BAD_REQUEST,headers,format!("{:?}",e)).into_response())
		}
	};
	fn add_remote_header(key:&'static str,headers:&mut HeaderMap,remote_headers:&reqwest::header::HeaderMap){
		for v in remote_headers.get_all(key){
			headers.append(key,String::from_utf8_lossy(v.as_bytes()).parse().unwrap());
		}
	}
	let remote_headers=resp.headers();
	add_remote_header("Content-Disposition",&mut headers,remote_headers);
	add_remote_header("Content-Type",&mut headers,remote_headers);
	let is_img=if let Some(media)=headers.get("Content-Type"){
		let s=String::from_utf8_lossy(media.as_bytes());
		s.starts_with("image/")
	}else{
		false
	};
	if !is_img{
		add_remote_header("Content-Length",&mut headers,remote_headers);
		add_remote_header("Content-Range",&mut headers,remote_headers);
		add_remote_header("Accept-Ranges",&mut headers,remote_headers);
	}
	let mut is_accept_avif=false;
	if !config.encode_avif{
		//force no avif
	}else if let Some(accept)=client_headers.get("Accept"){
		if let Ok(accept)=std::str::from_utf8(accept.as_bytes()){
			for e in accept.split(","){
				if e=="image/avif"{
					is_accept_avif=true;
				}
			}
		}
	}
	headers.append("Cache-Control","max-age=300".parse().unwrap());
	for line in config.append_headers.iter(){
		if let Some(idx)=line.find(":"){
			if idx+1>=line.len(){
				continue;
			}
			if let Ok(k)=axum::http::HeaderName::from_str(&line[0..idx]){
				if let Ok(v)=line[idx+1..].parse(){
					headers.append(k,v);
				}
			}
		}
	}
	RequestContext{
		is_accept_avif,
		headers,
		parms:q,
		src_bytes:Vec::new(),
		config,
		codec:Err(None),
		dummy_img,
		fontdb,
	}.encode(resp,is_img).await
}
struct RequestContext{
	is_accept_avif:bool,
	headers:HeaderMap,
	parms:RequestParams,
	src_bytes:Vec<u8>,
	config:Arc<ConfigFile>,
	codec:Result<image::ImageFormat,Option<image::ImageError>>,
	dummy_img:Arc<Vec<u8>>,
	fontdb:Arc<resvg::usvg::fontdb::Database>,
}
impl RequestContext{
	pub fn disposition_ext(headers:&mut HeaderMap,ext:&str){
		let k="Content-Disposition";
		if let Some(cd)=headers.get(k){
			let s=std::str::from_utf8(cd.as_bytes());
			if let Ok(s)=s{
				let cd=mailparse::parse_content_disposition(s);
				let cd_utf8=cd.params.get("filename*");
				let mut name=None;
				if let Some(cd_utf8)=cd_utf8{
					let cd_utf8=cd_utf8.to_uppercase();
					if cd_utf8.starts_with("UTF-8''")&&cd_utf8.len()>7{
						name=urlencoding::decode(&cd_utf8[7..]).map(|s|s.to_string()).ok();
					}
				}
				if name.is_none(){
					if let Some(filename)=cd.params.get("filename"){
						let m_filename=format!("_:{}",filename);
						let parsed=mailparse::parse_header(&m_filename.as_bytes());
						if let Ok((parsed,_))=&parsed{
							name=Some(parsed.get_value());
						}else if cd.params.get("name").is_none(){
							name=Some(filename.clone());
						}
					}
				}
				let name=name.unwrap_or_else(||cd.params.get("name").map(|s|s.clone()).unwrap_or_else(||"null".to_owned()));
				let mut name_arr:Vec<&str>=name.split('.').collect();
				name_arr.pop();
				let name=name_arr.join(".")+ext;
				let name=urlencoding::encode(&name);
				let content_disposition=format!("inline; filename=\"{}\";filename*=UTF-8''{};",name,name);
				headers.remove(k);
				headers.append(k,content_disposition.parse().unwrap());
			}
		}
	}
}
impl RequestContext{
	async fn encode(mut self,resp: reqwest::Response,mut is_img:bool)->Result<(axum::http::StatusCode,HeaderMap,axum::body::Body),axum::response::Response>{
		let mut is_svg=false;
		let mut content_type=None;
		if let Some(media)=self.headers.get("Content-Type"){
			let s=String::from_utf8_lossy(media.as_bytes());
			if s.as_ref()=="image/svg+xml"{
				is_svg=true;
			}else{
				content_type=Some(s);
			}
		}
		let status=resp.status();
		let resp=PreDataStream::new(resp).await;
		if let Some(Ok(head))=resp.head.as_ref(){
			//utf8にパースできて空白文字を削除した後の先頭部分が<svgの場合はsvg
			if std::str::from_utf8(&head).map(|s|s.trim().starts_with("<svg")).unwrap_or(false){
				is_svg=true;
			}else{
				self.codec=image::guess_format(head).map_err(|e|Some(e));
				if self.codec.is_err(){
					if let Some(content_type)=content_type.as_ref(){
						match content_type.as_ref(){
							"image/x-targa"|"image/x-tga"=>self.codec=Ok(image::ImageFormat::Tga),
							_=>{}
						}
					}
					if head.starts_with(&[0xFF,0x0A])||head.starts_with(&[0x00,0x00,0x00,0x0C,0x4A,0x58,0x4C,0x20,0x0D,0x0A,0x87,0x0A]){
						is_img=true;
						self.headers.remove("Content-Type");
						self.headers.append("Content-Type", "image/jxl".parse().unwrap());
					}
					if head.starts_with(&[0xFF,0x4F,0xFF,0x51])||head.starts_with(&[0x00,0x00,0x00,0x0C,0x6A,0x50,0x20,0x20,0x0D,0x0A,0x87,0x0A]){
						is_img=true;
						self.headers.remove("Content-Type");
						self.headers.append("Content-Type", "image/jp2".parse().unwrap());
					}
					if head.starts_with(&[0x49,0x49,0xBC]){
						is_img=true;
						self.headers.remove("Content-Type");
						self.headers.append("Content-Type", "image/jxr".parse().unwrap());
					}
				}
			}
		}
		if is_svg{
			self.load_all(resp).await?;
			if let Ok(img)=self.encode_svg(self.fontdb.clone()){
				self.headers.remove("Content-Length");
				self.headers.remove("Content-Range");
				self.headers.remove("Accept-Ranges");
				self.headers.remove("Cache-Control");
				self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
				return Err(self.response_img(img));
			}else{
				return Err((axum::http::StatusCode::OK,self.headers.clone(),self.src_bytes.clone()).into_response());
			}
		}else if is_img||self.codec.is_ok(){
			self.headers.remove("Content-Length");
			self.headers.remove("Content-Range");
			self.headers.remove("Accept-Ranges");
			self.load_all(resp).await?;
			let dummy_img=self.dummy_img.clone();
			let is_fallback=self.parms.fallback.is_some();
			let mut header=self.headers.clone();
			let mut handle=self;
			let resp=if let Ok(resp)=tokio::runtime::Handle::current().spawn_blocking(move ||{
				let resp=handle.encode_img();
				resp
			}).await{
				resp
			}else{
				header.append("X-Proxy-Error",format!("ImageEncodeThread").parse().unwrap());
				return Err(if is_fallback{
					header.remove("Content-Type");
					header.append("Content-Type","image/png".parse().unwrap());
					(axum::http::StatusCode::OK,header,(*dummy_img).clone()).into_response()
				}else{
					(axum::http::StatusCode::INTERNAL_SERVER_ERROR,header).into_response()
				});
			};
			if is_fallback{
				return Err(if resp.status()==axum::http::StatusCode::OK{
					resp
				}else{
					header.remove("Content-Type");
					header.append("Content-Type","image/png".parse().unwrap());
					(axum::http::StatusCode::OK,header,(*dummy_img).clone()).into_response()
				});
			}
			return Err(resp);
		}
		if let Some(media)=self.headers.get("Content-Type"){
			let s=String::from_utf8_lossy(media.as_bytes());
			if crate::browsersafe::FILE_TYPE_BROWSERSAFE.contains(&s.as_ref()){

			}else{
				self.headers.remove("Content-Type");
				self.headers.append("Content-Type","octet-stream".parse().unwrap());
				Self::disposition_ext(&mut self.headers,".unknown");
			}
		}
		let body=axum::body::Body::from_stream(resp);
		if status.is_success(){
			self.headers.remove("Cache-Control");
			self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
			if status==reqwest::StatusCode::PARTIAL_CONTENT{
				Ok((axum::http::StatusCode::PARTIAL_CONTENT,self.headers.clone(),body))
			}else{
				Ok((axum::http::StatusCode::OK,self.headers.clone(),body))
			}
		}else{
			self.headers.append("X-Proxy-Error",format!("status:{}",status.as_u16()).parse().unwrap());
			Err(if self.parms.fallback.is_some(){
				self.headers.remove("Content-Type");
				self.headers.append("Content-Type","image/png".parse().unwrap());
				(axum::http::StatusCode::OK,self.headers.clone(),(*self.dummy_img).clone()).into_response()
			}else{
				let status=match status{
					reqwest::StatusCode::BAD_REQUEST=>axum::http::StatusCode::BAD_REQUEST,
					reqwest::StatusCode::FORBIDDEN=>axum::http::StatusCode::FORBIDDEN,
					reqwest::StatusCode::NOT_FOUND=>axum::http::StatusCode::NOT_FOUND,
					reqwest::StatusCode::REQUEST_TIMEOUT=>axum::http::StatusCode::GATEWAY_TIMEOUT,
					reqwest::StatusCode::GONE=>axum::http::StatusCode::GONE,
					reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS=>axum::http::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS,
					_=>axum::http::StatusCode::BAD_GATEWAY,
				};
				(status,self.headers.clone()).into_response()
			})
		}
	}
	async fn load_all(&mut self,mut resp: PreDataStream)->Result<(),axum::response::Response>{
		let len_hint=resp.content_length.unwrap_or(2048.min(self.config.max_size));
		if len_hint>self.config.max_size{
			self.headers.append("X-Proxy-Error",format!("lengthHint:{}>{}",len_hint,self.config.max_size).parse().unwrap());
			return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response())
		}
		let mut response_bytes=Vec::with_capacity(len_hint as usize);
		while let Some(x) = resp.next().await{
			match x{
				Ok(b)=>{
					if response_bytes.len()+b.len()>self.config.max_size as usize{
						self.headers.append("X-Proxy-Error",format!("length:{}>{}",response_bytes.len()+b.len(),self.config.max_size).parse().unwrap());
						return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response())
					}
					response_bytes.extend_from_slice(&b);
				},
				Err(e)=>{
					self.headers.append("X-Proxy-Error",format!("LoadAll:{:?}",e).parse().unwrap());
					return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone(),format!("{:?}",e)).into_response())
				}
			}
		}
		self.src_bytes=response_bytes;
		Ok(())
	}
}
struct PreDataStream{
	content_length:Option<u64>,
	head:Option<Result<axum::body::Bytes, reqwest::Error>>,
	last:Pin<Box<dyn futures::stream::Stream<Item=Result<axum::body::Bytes, reqwest::Error>>+Send+Sync>>,
}
impl  PreDataStream{
	async fn new(value: reqwest::Response) -> Self {
		let content_length=value.content_length();
		let mut stream=value.bytes_stream();
		let head=stream.next().await;
		Self{
			content_length,
			head,
			last: Box::pin(stream)
		}
	}
}
impl futures::stream::Stream for PreDataStream{
	type Item=Result<axum::body::Bytes, reqwest::Error>;

	fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
		let mut r=self.as_mut();
		if let Some(d)=r.head.take(){
			return std::task::Poll::Ready(Some(d));
		}
		r.last.as_mut().poll_next(cx)
	}
}