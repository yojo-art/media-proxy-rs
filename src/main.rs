use std::{io::{Read, Write}, net::SocketAddr, str::FromStr, sync::Arc};

use axum::{body::StreamBody, http::HeaderMap, response::IntoResponse, Router};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;

mod img;
mod svg;
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
	load_system_fonts:bool,
	webp_quality:f32,
	encode_avif:bool,
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
			load_system_fonts:true,
			webp_quality: 75f32,
			encode_avif:true,
		};
		let default_config=serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path).expect("create default config.json").write_all(default_config.as_bytes()).unwrap();
	}
	let config:ConfigFile=serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();

	let mut dummy_png=vec![];
	std::fs::File::open("asset/dummy.png").expect("not found dummy.png").read_to_end(&mut dummy_png).expect("load error dummy.png");
	let dummy_png=Arc::new(dummy_png);
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
	fontdb.load_fonts_dir("asset/font/");
	let fontdb=Arc::new(fontdb);
	let arg_tup=(client,config,dummy_png,fontdb);
	rt.block_on(async{
		let http_addr:SocketAddr = arg_tup.1.bind_addr.parse().unwrap();
		let app = Router::new();
		let arg_tup0=arg_tup.clone();
		let app=app.route("/",axum::routing::get(move|path,headers,parms|get_file(path,headers,arg_tup0.clone(),parms)));
		let app=app.route("/*path",axum::routing::get(move|path,headers,parms|get_file(path,headers,arg_tup.clone(),parms)));
		axum::Server::bind(&http_addr).serve(app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
	});
}

async fn get_file(
	axum::extract::Path(_path):axum::extract::Path<String>,
	client_headers:axum::http::HeaderMap,
	(client,config,dummy_img,fontdb):(reqwest::Client,Arc<ConfigFile>,Arc<Vec<u8>>,Arc<resvg::usvg::fontdb::Database>),
	axum::extract::Query(q):axum::extract::Query<RequestParams>,
)->Result<(axum::http::StatusCode,axum::headers::HeaderMap,StreamBody<impl futures::Stream<Item = Result<axum::body::Bytes, reqwest::Error>>>),axum::response::Response>{
	let mut headers=axum::headers::HeaderMap::new();
	headers.append("X-Remote-Url",q.url.parse().unwrap());
	if config.encode_avif{
		headers.append("Vary","Accept,Range".parse().unwrap());
	}
	let req=client.get(&q.url);
	let req=req.timeout(std::time::Duration::from_millis(config.timeout));
	let req=req.header("UserAgent",config.user_agent.clone());
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
	fn add_remote_header(key:&'static str,headers:&mut axum::headers::HeaderMap,remote_headers:&reqwest::header::HeaderMap){
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
			if let Ok(k)=axum::headers::HeaderName::from_str(&line[0..idx]){
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
		codec:None,
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
	codec:Option<image::ImageFormat>,
	dummy_img:Arc<Vec<u8>>,
	fontdb:Arc<resvg::usvg::fontdb::Database>,
}
impl RequestContext{
	pub fn disposition_ext(headers:&mut HeaderMap,ext:&str){
		Self::rename_disposition(headers,|s|{
			let mut last_dot=s.len()-1;
			let mut index=0;
			for c in s.chars(){
				if c=='.'{
					last_dot=index;
				}
				index+=1;
			}
			format!("{}{}",&s[0..last_dot as usize],ext)
		})
	}
	pub fn rename_disposition(headers:&mut HeaderMap,mut f:impl FnMut(&str)->String){
		let k="Content-Disposition";
		if let Some(cd)=headers.get(k){
			let s=std::str::from_utf8(cd.as_bytes());
			if let Ok(s)=s{
				let mut res=String::new();
				for e in s.split("; "){
					if e.starts_with("filename"){
						let mut index=0;
						for e in e.split("="){
							//明示文字コード指定があるか
							let mut is_charset=false;
							if index==0{
								res.push_str(e);
								res.push_str("=");
								if e.ends_with("*"){
									is_charset=true;
								}
							}
							if index==1{
								if is_charset{
									if let Some(i)=e.find("\""){
										res.push_str(&format!("{}\"{}",&e[0..i],f(&e[i..]).as_str()));
									}
								}else if e.starts_with("\"")&&e.ends_with("\""){
									let e=&e[1..e.len()-1];
									if !e.is_empty(){
										res.push_str(&format!("\"{}\"",f(e).as_str()));
									}
								}else{
									if !e.is_empty(){
										res.push_str(f(e).as_str());
									}
								}
							}
							index+=1;
						}
						if index==1{
							res.push_str(f("null").as_str());
						}
					}else{
						res.push_str(e);
					}
					res.push_str("; ");
				}
				headers.remove(k);
				headers.append(k,res.parse().unwrap());
			}
		}
	}
}
impl RequestContext{
	async fn encode(&mut self,resp: reqwest::Response,is_img:bool)->Result<(axum::http::StatusCode,axum::headers::HeaderMap,StreamBody<impl futures::Stream<Item = Result<axum::body::Bytes, reqwest::Error>>>),axum::response::Response>{
		let mut is_svg=false;
		if let Some(media)=self.headers.get("Content-Type"){
			let s=String::from_utf8_lossy(media.as_bytes());
			if s.as_ref()=="image/svg+xml"{
				is_svg=true;
			}
		}
		if !is_svg&&!is_img{
			if let Some(cd)=self.headers.get("Content-Disposition"){
				let s=std::str::from_utf8(cd.as_bytes());
				if let Ok(s)=s{
					for e in s.split(";"){
						if e.starts_with("filename"){
							if e.contains(".svg"){
								is_svg=true;
							}
						}
					}
				}
			}
		}
		if is_svg{
			self.load_all(resp).await?;
			if let Ok(img)=self.encode_svg(&self.fontdb){
				self.headers.remove("Content-Length");
				self.headers.remove("Content-Range");
				self.headers.remove("Accept-Ranges");
				self.headers.remove("Cache-Control");
				self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
				return Err(self.response_img(img));
			}else{
				return Err((axum::http::StatusCode::OK,self.headers.clone(),self.src_bytes.clone()).into_response());
			}
		}else if is_img{
			self.load_all(resp).await?;
			let resp=self.encode_img();
			if self.parms.fallback.is_some(){
				return Err(if resp.status()==axum::http::StatusCode::OK{
					resp
				}else{
					self.headers.remove("Content-Type");
					self.headers.append("Content-Type","image/png".parse().unwrap());
					(axum::http::StatusCode::OK,self.headers.clone(),(*self.dummy_img).clone()).into_response()
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
		let status=resp.status();
		let body=StreamBody::new(resp.bytes_stream());
		if status.is_success(){
			self.headers.remove("Cache-Control");
			self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
			if status==reqwest::StatusCode::PARTIAL_CONTENT{
				Ok((axum::http::StatusCode::PARTIAL_CONTENT,self.headers.clone(),body))
			}else{
				Ok((axum::http::StatusCode::OK,self.headers.clone(),body))
			}
		}else{
			Err(if self.parms.fallback.is_some(){
				self.headers.remove("Content-Type");
				self.headers.append("Content-Type","image/png".parse().unwrap());
				(axum::http::StatusCode::OK,self.headers.clone(),(*self.dummy_img).clone()).into_response()
			}else{
				axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
			})
		}
	}
	async fn load_all(&mut self,resp: reqwest::Response)->Result<(),axum::response::Response>{
		let len_hint=resp.content_length().unwrap_or(2048.min(self.config.max_size));
		if len_hint>self.config.max_size{
			self.headers.append("X-Proxy-Error",format!("lengthHint:{}>{}",len_hint,self.config.max_size).parse().unwrap());
			return Err((axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response())
		}
		let mut response_bytes=Vec::with_capacity(len_hint as usize);
		let mut stream=resp.bytes_stream();
		while let Some(x) = stream.next().await{
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
