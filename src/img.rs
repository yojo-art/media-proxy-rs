
use axum::response::IntoResponse;
use image::{AnimationDecoder, DynamicImage, GenericImage, ImageDecoder};
use tokio_stream::StreamExt;

use crate::RequestContext;

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
	pub(crate) async fn encode_img(&mut self,resp: reqwest::Response)->axum::response::Response{
		let len_hint=resp.content_length().unwrap_or(2048.min(self.config.max_size));
		if len_hint>self.config.max_size{
			return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response()
		}
		let mut response_bytes=Vec::with_capacity(len_hint as usize);
		let mut stream=resp.bytes_stream();
		while let Some(x) = stream.next().await{
			match x{
				Ok(b)=>{
					if response_bytes.len()+b.len()>self.config.max_size as usize{
						return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response()
					}
					response_bytes.extend_from_slice(&b);
				},
				Err(e)=>{
					return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone(),format!("{:?}",e)).into_response()
				}
			}
		}
		self.src_bytes=response_bytes;
		if self.parms.r#static.is_some(){
			return self.encode_single();
		}
		if self.parms.badge.is_some(){
			return self.encode_single();
		}
		let codec=image::guess_format(&self.src_bytes);
		let codec=match codec{
			Ok(codec) => codec,
			Err(e) => {
				self.headers.append("X-Codec-Error",format!("{:?}",e).parse().unwrap());
				return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
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
		let format=if self.parms.badge.is_some(){
			image::ImageFormat::Png
		}else{
			image::ImageFormat::WebP
		};
		match img.write_to(&mut std::io::Cursor::new(&mut buf),format){
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
