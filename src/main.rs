use std::{io::Write, net::SocketAddr, str::FromStr, sync::Arc};

use axum::{body::StreamBody, http::HeaderMap, response::IntoResponse, Router};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

mod img;
mod browsersafe;

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
fn main() {
	let config_path=match std::env::var("FILES_PROXY_CONFIG_PATH"){
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
			timeout:1000,
			user_agent: "https://github.com/yojo-art/media-proxy-rs".to_owned(),
			max_size:256*1024*1024,
			proxy:None,
			filter_type:FilterType::Triangle,
			max_pixels:2048,
			append_headers:[
				"Content-Security-Policy:default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'".to_owned(),
				"Access-Control-Allow-Origin:*".to_owned(),
			].to_vec(),
		};
		let default_config=serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path).expect("create default config.json").write_all(default_config.as_bytes()).unwrap();
	}
	let config:ConfigFile=serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();

	let config=Arc::new(config);
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let client=reqwest::ClientBuilder::new();
	let client=match &config.proxy{
		Some(url)=>client.proxy(reqwest::Proxy::http(url).unwrap()),
		None=>client,
	};
	let client=client.build().unwrap();
	rt.block_on(async{
		let http_addr:SocketAddr = config.bind_addr.parse().unwrap();
		let app = Router::new();
		let client0=client.clone();
		let config0=config.clone();
		let app=app.route("/",axum::routing::get(move|path,headers,parms|get_file(path,headers,client0.clone(),parms,config0.clone())));
		let app=app.route("/*path",axum::routing::get(move|path,headers,parms|get_file(path,headers,client.clone(),parms,config.clone())));
		axum::Server::bind(&http_addr).serve(app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
	});
}

async fn get_file(
	axum::extract::Path(_path):axum::extract::Path<String>,
	headers:axum::http::HeaderMap,
	client:reqwest::Client,
	axum::extract::Query(q):axum::extract::Query<RequestParams>,
	config:Arc<ConfigFile>,
)->Result<(axum::http::StatusCode,axum::headers::HeaderMap,StreamBody<impl futures::Stream<Item = Result<axum::body::Bytes, reqwest::Error>>>),axum::response::Response>{
	let mut headers=axum::headers::HeaderMap::new();
	headers.append("X-Remote-Url",q.url.parse().unwrap());
	let req=client.get(&q.url);
	let req=req.timeout(std::time::Duration::from_millis(config.timeout));
	let req=req.header("UserAgent",config.user_agent.clone());
	let resp=match req.send().await{
		Ok(resp) => resp,
		Err(e) => return Err((axum::http::StatusCode::BAD_REQUEST,headers,format!("{:?}",e)).into_response()),
	};
	fn add_remote_header(key:&'static str,headers:&mut axum::headers::HeaderMap,remote_headers:&reqwest::header::HeaderMap){
		for v in remote_headers.get_all(key){
			headers.append(key,String::from_utf8_lossy(v.as_bytes()).parse().unwrap());
		}
	}
	let remote_headers=resp.headers();
	add_remote_header("Content-Disposition",&mut headers,remote_headers);
	add_remote_header("Content-Type",&mut headers,remote_headers);
	headers.append("Cache-Control","max-age=300".parse().unwrap());
	for line in config.append_headers.iter(){
		if let Some(idx)=line.find(":"){
			if idx+1>=line.len(){
				continue;
			}
			if let Ok(k)=axum::headers::HeaderName::from_str(&line[0..idx]){
				if let Ok(v)=line[idx+1..].parse(){
					headers.append(k,v);
				}
			}
		}
	}
	RequestContext{
		headers,
		parms:q,
		src_bytes:Vec::new(),
		config,
	}.encode(resp).await
}
struct RequestContext{
	headers:HeaderMap,
	parms:RequestParams,
	src_bytes:Vec<u8>,
	config:Arc<ConfigFile>,
}
impl RequestContext{
	async fn encode(&mut self,resp: reqwest::Response)->Result<(axum::http::StatusCode,axum::headers::HeaderMap,StreamBody<impl futures::Stream<Item = Result<axum::body::Bytes, reqwest::Error>>>),axum::response::Response>{
		if let Some(media)=self.headers.get("Content-Type"){
			let s=String::from_utf8_lossy(media.as_bytes());
			if s.starts_with("image/"){
				return Err(self.encode_img(resp).await);
			}
			if crate::browsersafe::FILE_TYPE_BROWSERSAFE.contains(&s.as_ref()){

			}else{
				self.headers.remove("Content-Type");
				self.headers.append("Content-Type","octet-stream".parse().unwrap());
				if let Some(cd)=self.headers.remove("Content-Disposition"){
					let s=String::from_utf8_lossy(cd.as_bytes());
					self.headers.append("Content-Type",format!("{}.unknown",s).parse().unwrap());
				}
			}
		}
		let status=resp.status();
		let body=StreamBody::new(resp.bytes_stream());
		if status.is_success(){
			Ok((axum::http::StatusCode::OK,self.headers.clone(),body))
		}else{
			Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response())
		}
	}
}
