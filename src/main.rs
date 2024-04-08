use std::{io::Write, net::SocketAddr, str::FromStr, sync::Arc};

use axum::{http::HeaderMap, response::IntoResponse, Router};
use futures::StreamExt;
use image::{AnimationDecoder, DynamicImage, GenericImage, ImageDecoder};
use serde::{Deserialize, Serialize};

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
)->axum::response::Response{
	let mut headers=axum::headers::HeaderMap::new();
	headers.append("X-Remote-Url",q.url.parse().unwrap());
	let req=client.get(&q.url);
	let req=req.timeout(std::time::Duration::from_millis(config.timeout));
	let req=req.header("UserAgent",config.user_agent.clone());
	let resp=match req.send().await{
		Ok(resp) => resp,
		Err(e) => return (axum::http::StatusCode::BAD_REQUEST,headers,format!("{:?}",e)).into_response(),
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
	let len_hint=resp.content_length().unwrap_or(2048.min(config.max_size));
	if len_hint>config.max_size{
		return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
	}
	let mut response_bytes=Vec::with_capacity(len_hint as usize);
	let mut stream=resp.bytes_stream();
	while let Some(x) = stream.next().await{
		match x{
			Ok(b)=>{
				if response_bytes.len()+b.len()>config.max_size as usize{
					return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
				}
				response_bytes.extend_from_slice(&b);
			},
			Err(e)=>{
				return (axum::http::StatusCode::BAD_GATEWAY,headers,format!("{:?}",e)).into_response()
			}
		}
	}
	RequestContext{
		headers,
		src_bytes:response_bytes,
		parms:q,
		config,
	}.encode()
}
struct RequestContext{
	headers:HeaderMap,
	src_bytes:Vec<u8>,
	parms:RequestParams,
	config:Arc<ConfigFile>,
}
impl RequestContext{
	fn resize(&self,img:DynamicImage)->DynamicImage{
		if self.parms.badge.is_some(){
			let width=96;
			let height=96;
			let img=img.resize(width,height,self.config.filter_type.into());
			let img=img.into_luma8();
			let mut canvas=image::GrayAlphaImage::new(width,height);
			let x_start=(width-img.width())/2;
			let y_start=(height-img.height())/2;
			let mut sub_canvas=canvas.sub_image(x_start,y_start,width-x_start,height-y_start);
			let mut y=0;
			for rows in img.rows(){
				let mut x=0;
				for p in rows{
					let p:image::LumaA<u8>=[p.0[0],p.0[0]].into();
					sub_canvas.put_pixel(x,y,p);
					x+=1;
				}
				y+=1;
			}
			return DynamicImage::ImageLumaA8(canvas);
		}
		let mut max_width=self.config.max_pixels;
		let mut max_height=self.config.max_pixels;
		if self.parms.r#static.is_some(){
			max_width=498;
			max_height=422;
		}
		if self.parms.emoji.is_some(){
			max_height=128;
		}
		if self.parms.preview.is_some(){
			max_width=200;
			max_height=200;
		}
		if self.parms.avatar.is_some(){
			max_height=320;
		}
		let max_width=max_width.min(img.width());
		let max_height=max_height.min(img.height());
		let filter=self.config.filter_type.into();
		let img=img.resize(max_width,max_height,filter);
		img
	}
	fn encode(mut self)->axum::response::Response{
		if self.parms.r#static.is_some(){
			return self.encode_single();
		}
		let codec=image::guess_format(&self.src_bytes);
		let codec=match codec{
			Ok(codec) => codec,
			Err(e) => {
				self.headers.append("X-Codec-Error",format!("{:?}",e).parse().unwrap());
				return (axum::http::StatusCode::BAD_GATEWAY,self.headers).into_response();
			},
		};
		match codec{
			image::ImageFormat::Png => {
				let a=match image::codecs::png::PngDecoder::new(std::io::Cursor::new(&self.src_bytes)){
					Ok(a)=>a,
					Err(_)=>return self.encode_single()
				};
				if !a.is_apng().unwrap(){
					return self.encode_single();
				}
				let size=a.dimensions();
				match a.apng(){
					Ok(frames)=>self.encode_anim(size,frames.into_frames()),
					Err(_)=>self.encode_single()
				}
			},
			image::ImageFormat::Gif => {
				match image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&self.src_bytes)){
					Ok(a)=>self.encode_anim(a.dimensions(),a.into_frames()),
					Err(_)=>self.encode_single()
				}
			},
			image::ImageFormat::WebP => {
				let a=match image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&self.src_bytes)){
					Ok(a)=>a,
					Err(_)=>return self.encode_single()
				};
				if a.has_animation(){
					self.encode_anim(a.dimensions(),a.into_frames())
				}else{
					self.encode_single()
				}
			},
			_ => {
				self.encode_single()
			},
		}
	}
	fn encode_anim(&self,size:(u32,u32),frames:image::Frames)->axum::response::Response{
		let conf=webp::WebPConfig::new().unwrap();
		let mut encoder=webp::AnimEncoder::new(size.0,size.1,&conf);
		let mut image_buffer=vec![];
		{
			let mut timestamp=0;
			for frame in frames{
				if let Ok(frame)=frame{
					timestamp+=std::time::Duration::from(frame.delay()).as_millis() as i32;
					let img=image::DynamicImage::ImageRgba8(frame.into_buffer());
					let img=self.resize(img);
					image_buffer.push((img,timestamp));
				}
			}
		}
		for (img,timestamp) in &image_buffer{
			let aframe=webp::AnimFrame::from_image(img,*timestamp);
			if let Ok(aframe)=aframe{
				encoder.add_frame(aframe);
			}
		}
		let mut headers=self.headers.clone();
		if image_buffer.is_empty(){
			headers.append("X-Proxy-Error","NoAvailableFrames".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
		};
		let buf=encoder.encode();
		headers.remove("Content-Type");
		headers.append("Content-Type","image/webp".parse().unwrap());
		headers.remove("Cache-Control");
		headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
		(axum::http::StatusCode::OK,headers,buf.to_vec()).into_response()
	}
	fn encode_single(&self)->axum::response::Response{
		let mut headers=self.headers.clone();
		let img=image::load_from_memory(&self.src_bytes);
		let img=match img{
			Ok(img)=>img,
			Err(e)=>{
				headers.append("X-Proxy-Error",format!("DecodeError_{:?}",e).parse().unwrap());
				return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
			}
		};
		let img=self.resize(img);
		let mut buf=vec![];
		match img.write_to(&mut std::io::Cursor::new(&mut buf),image::ImageFormat::WebP){
			Ok(_)=>{
				headers.remove("Content-Type");
				headers.append("Content-Type","image/webp".parse().unwrap());
				headers.remove("Cache-Control");
				headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
				(axum::http::StatusCode::OK,headers,buf).into_response()
			},
			Err(e)=>{
				headers.append("X-Proxy-Error",format!("EncodeError_{:?}",e).parse().unwrap());
				(axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
			}
		}
	}
}
