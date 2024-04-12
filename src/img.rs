
use axum::response::IntoResponse;
use image::{AnimationDecoder, DynamicImage, GenericImage, GenericImageView};

use crate::RequestContext;

impl RequestContext{
	pub(crate) fn image_size_hint(&self)->(u32,u32){
		if self.parms.badge.is_some(){
			return (96,96);
		}
		if self.parms.r#static.is_some(){
			return (498,422);
		}
		if self.parms.emoji.is_some(){
			return (u32::MAX,128);
		}
		if self.parms.preview.is_some(){
			return (200,200);
		}
		if self.parms.avatar.is_some(){
			return (u32::MAX,320);
		}
		(self.config.max_pixels,self.config.max_pixels)
	}
	pub(crate) fn resize(&self,img:DynamicImage)->DynamicImage{
		let (width,height)=self.image_size_hint();
		if self.parms.badge.is_some(){
			let img=if img.dimensions()==(width,height){
				img
			}else{
				img.resize(width,height,self.config.filter_type.into())
			};
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
		let max_width=width.min(img.width());
		let max_height=height.min(img.height());
		let filter=self.config.filter_type.into();
		if img.dimensions()==(max_width,max_height){
			return img;
		}
		let img=img.resize(max_width,max_height,filter);
		img
	}
	pub(crate) fn encode_img(&mut self)->axum::response::Response{
		let codec=image::guess_format(&self.src_bytes);
		self.codec=codec.as_ref().ok().copied();
		if self.parms.r#static.is_some(){
			return self.encode_single();
		}
		if self.parms.badge.is_some(){
			return self.encode_single();
		}
		let codec=match codec{
			Ok(codec) => codec,
			Err(e) => {
				self.headers.append("X-Proxy-Error",format!("CodecError:{:?}",e).parse().unwrap());
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
				match a.apng(){
					Ok(frames)=>self.encode_anim(frames.into_frames()),
					Err(_)=>self.encode_single()
				}
			},
			image::ImageFormat::Gif => {
				match image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&self.src_bytes)){
					Ok(a)=>self.encode_anim(a.into_frames()),
					Err(_)=>self.encode_single()
				}
			},
			image::ImageFormat::WebP => {
				let a=match image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&self.src_bytes)){
					Ok(a)=>a,
					Err(_)=>return self.encode_single()
				};
				if a.has_animation(){
					self.encode_anim(a.into_frames())
				}else{
					self.encode_single()
				}
			},
			_ => {
				self.encode_single()
			},
		}
	}
	fn encode_anim(&self,frames:image::Frames)->axum::response::Response{
		let conf=webp::WebPConfig::new().unwrap();
		let mut image_buffer=vec![];
		let mut size:Option<(u32, u32)>=None;
		{
			let mut timestamp=0;
			for frame in frames{
				if let Ok(frame)=frame{
					timestamp+=std::time::Duration::from(frame.delay()).as_millis() as i32;
					let img=image::DynamicImage::ImageRgba8(frame.into_buffer());
					let img=self.resize(img);
					if let Some(size)=size{
						if size.0==img.width()&&size.1==img.height(){
							//ok
						}else{
							continue;
						}
					}else{
						size=Some((img.width(),img.height()));
					}
					image_buffer.push((img,timestamp));
				}
			}
		}
		let mut headers=self.headers.clone();
		if size.is_none(){
			headers.append("X-Proxy-Error","NoAvailableFrames0".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
		};
		let size=size.unwrap();
		let mut encoder=webp::AnimEncoder::new(size.0,size.1,&conf);
		for (img,timestamp) in &image_buffer{
			let aframe=image_to_frame(img,*timestamp);
			if let Ok(aframe)=aframe{
				encoder.add_frame(aframe);
			}
		}
		if image_buffer.is_empty(){
			headers.append("X-Proxy-Error","NoAvailableFrames".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
		};
		let buf=encoder.encode();
		headers.remove("Content-Type");
		headers.append("Content-Type","image/webp".parse().unwrap());
		headers.remove("Cache-Control");
		headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
		Self::disposition_ext(&mut headers,".webp");
		(axum::http::StatusCode::OK,headers,buf.to_vec()).into_response()
	}
	fn encode_single(&mut self)->axum::response::Response{
		let img=image::load_from_memory(&self.src_bytes);
		let img=match img{
			Ok(img)=>img,
			Err(e)=>{
				self.headers.append("X-Proxy-Error",format!("DecodeError_{:?}",e).parse().unwrap());
				return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
			}
		};
		self.response_img(img)
	}
	pub(crate) fn response_img(&mut self,img:DynamicImage)->axum::response::Response{
		let img=match self.codec{
			Some(image::ImageFormat::Jpeg)|Some(image::ImageFormat::Tiff)=>{
				self.exif_rotate(img)
			},
			_=>img
		};
		let img=self.resize(img);
		let mut buf=vec![];
		self.headers.remove("Content-Type");
		let format=if self.parms.badge.is_some(){
			self.headers.append("Content-Type","image/png".parse().unwrap());
			Self::disposition_ext(&mut self.headers,".png");
			image::ImageFormat::Png
		}else{
			if self.is_accept_avif{
				self.headers.append("Content-Type","image/avif".parse().unwrap());
				Self::disposition_ext(&mut self.headers,".avif");
				image::ImageFormat::Avif
			}else{
				let width=img.width();
				let height=img.height();
				let rgba=img.into_rgba8();
				let encoer=webp::Encoder::from_rgba(rgba.as_raw(),width,height);
				let mut config=webp::WebPConfig::new().unwrap();
				config.quality=self.config.webp_quality;
				return match encoer.encode_advanced(&config){
					Ok(mem) => {
						buf.extend_from_slice(&mem);
						self.headers.append("Content-Type","image/webp".parse().unwrap());
						self.headers.remove("Cache-Control");
						self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
						Self::disposition_ext(&mut self.headers,".webp");
						(axum::http::StatusCode::OK,self.headers.clone(),buf).into_response()
					},
					Err(e) => {
						self.headers.append("X-Proxy-Error",format!("EncodeError_{:?}",e).parse().unwrap());
						(axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response()
					},
				};
			}
		};
		match img.write_to(&mut std::io::Cursor::new(&mut buf),format){
			Ok(_)=>{
				self.headers.remove("Cache-Control");
				self.headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
				(axum::http::StatusCode::OK,self.headers.clone(),buf).into_response()
			},
			Err(e)=>{
				self.headers.append("X-Proxy-Error",format!("EncodeError_{:?}",e).parse().unwrap());
				(axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response()
			}
		}
	}
	pub fn exif_rotate(&self,img:DynamicImage) -> DynamicImage{
		let exifreader = rexif::parse_buffer_quiet(&self.src_bytes);
		if let Ok(exif)=exifreader.0{
			for e in exif.entries{
				match e.tag{
					rexif::ExifTag::Orientation=>{
						return match e.value.to_i64(0).unwrap_or(0){
							2=>DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&img)),
							3=>DynamicImage::ImageRgba8(image::imageops::rotate180(&img)),
							4=>DynamicImage::ImageRgba8(image::imageops::flip_vertical(&img)),
							5=>DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&image::imageops::rotate90(&img))),
							6=>DynamicImage::ImageRgba8(image::imageops::rotate90(&img)),
							7=>DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&image::imageops::rotate270(&img))),
							8=>DynamicImage::ImageRgba8(image::imageops::rotate270(&img)),
							_=>img,
						};
					},
					_=>{}
				}
			}
		}
		img
	}
}

pub fn image_to_frame(image: &DynamicImage, timestamp: i32) -> Result<webp::AnimFrame, &'static str> {
	match image {
		DynamicImage::ImageLuma8(_) => Err("Unimplemented"),
		DynamicImage::ImageLumaA8(_) => Err("Unimplemented"),
		DynamicImage::ImageRgb8(image) => Ok(webp::AnimFrame::from_rgb(
			image.as_ref(),
			image.width(),
			image.height(),
			timestamp,
		)),
		DynamicImage::ImageRgba8(image) => Ok(webp::AnimFrame::from_rgba(
			image.as_ref(),
			image.width(),
			image.height(),
			timestamp,
		)),
		_ => Err("Unimplemented"),
	}
}
