
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
	pub(crate) fn resize(&self,img:DynamicImage)->Option<DynamicImage>{
		let (width,height)=self.image_size_hint();
		if self.parms.badge.is_some(){
			let img=if img.dimensions()==(width,height){
				img
			}else{
				resize(img,width,height,self.config.filter_type.into())?
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
			return Some(DynamicImage::ImageLumaA8(canvas));
		}
		let max_width=width.min(img.width());
		let max_height=height.min(img.height());
		let filter=self.config.filter_type.into();
		if img.dimensions()==(max_width,max_height){
			return Some(img);
		}
		resize(img,max_width,max_height,filter)
	}
	pub(crate) fn encode_img(&mut self)->axum::response::Response{
		if self.parms.r#static.is_some(){
			return self.encode_single();
		}
		if self.parms.badge.is_some(){
			return self.encode_single();
		}
		let codec=match &self.codec{
			Ok(codec) => codec,
			Err(e) => {
				match self.headers.get("Content-Type").map(|s|std::str::from_utf8(s.as_bytes())){
					Some(Ok("image/jxl"))=>{
						let decoder = jxl_oxide::integration::JxlDecoder::new(std::io::Cursor::new(&self.src_bytes));
						let img=decoder.map(|decoder|DynamicImage::from_decoder(decoder)).unwrap_or_else(|e|Err(e));
						let img=match img{
							Ok(img) => img,
							Err(e) => {
								self.headers.append("X-Proxy-Error",format!("JpegXL Error:{:?}",e).parse().unwrap());
								return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
							},
						};
						return self.response_img(img);
					}
					Some(Ok("image/jp2"))=>{
						let img=jpeg2k::Image::from_bytes(&self.src_bytes).map(|img|DynamicImage::try_from(&img));
						let img=img.map(|r|r.map_err(|e|e.to_string())).map_err(|e|e.to_string()).unwrap_or_else(|e|Err(e));
						let img=match img{
							Ok(img) => img,
							Err(e) => {
								self.headers.append("X-Proxy-Error",format!("Jpeg2000 Error:{:?}",e).parse().unwrap());
								return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
							},
						};
						return self.response_img(img);
					},
					Some(Ok("image/jxr"))=>{
						fn decode_jxr(src_bytes:&[u8])->Result<Result<DynamicImage,String>, jpegxr::JXRError>{
							use jpegxr::{ImageDecode, PixelInfo};
							let mut decoder = ImageDecode::with_reader(std::io::Cursor::new(src_bytes))?;
							let (width, height) = decoder.get_size()?;
							let info = PixelInfo::from_format(decoder.get_pixel_format()?);
							let stride = width as usize * info.bits_per_pixel() as usize/8;
							let size = stride * height as usize;
							let mut buffer = Vec::<u8>::with_capacity(size);
							buffer.resize(size, 0);
							decoder.alpha_mode(info.has_alpha());
							decoder.copy_all(&mut buffer, stride)?;
							let img=jpegxr_img(width as u32,height as u32,stride,buffer,info.format());
							Ok(img.ok_or_else(||format!("color_format={:?}&bgr={}&channels={}&format={:?}",info.color_format(),info.bgr(),info.channels(),info.format())))
						}
						match decode_jxr(&self.src_bytes){
							Ok(Ok(img))=>{
								return self.response_img(img);
							},
							Ok(Err(e))=>{
								self.headers.append("X-Proxy-Error",format!("JpegXR decode pixels {:?}",e).parse().unwrap());
								return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
							}
							Err(e)=>{
								self.headers.append("X-Proxy-Error",format!("JpegXR decode bytes {:?}",e).parse().unwrap());
								return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
							}
						}
					}
					_=>{
						self.headers.append("X-Proxy-Error",format!("CodecError:{:?}",e).parse().unwrap());
						return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
					}
				}
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
					let decoder=webp::AnimDecoder::new(&self.src_bytes);
					if let Ok(mut dec)=decoder.decode(){
						let mut offset=0;
						let mut frames=vec![];
						dec.sort_by_time_stamp();
						for frame in dec.into_iter(){
							let img=if frame.get_layout().is_alpha() {
								let image =
									image::ImageBuffer::from_raw(frame.width(), frame.height(), frame.get_image().to_owned())
										.expect("ImageBuffer couldn't be created");
								image
							} else {
								let image =
									image::ImageBuffer::from_raw(frame.width(), frame.height(), frame.get_image().to_owned())
										.expect("ImageBuffer couldn't be created");
								DynamicImage::ImageRgb8(image).into_rgba8()
							};
							let delay=frame.get_time_ms()-offset;
							offset=frame.get_time_ms();
							if delay<0{
								continue;
							}
							let delay=std::time::Duration::from_millis(delay as u64);
							let delay=image::Delay::from_saturating_duration(delay);
							let frame=image::Frame::from_parts(img,0,0,delay);
							frames.push(Ok(frame));
						}
						let frames=image::Frames::new(Box::new(frames.into_iter()));
						self.encode_anim(frames)
					}else{
						self.encode_anim(a.into_frames())
					}
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
		let mut size:Option<(u32, u32)>=None;
		let mut encoder=None;
		let mut available_frames=0;
		let mut err=None;
		{
			let mut timestamp=0;
			const FRAMES_LIMIT:u32=1000;
			let mut allow_frames=FRAMES_LIMIT;
			for frame in frames{
				allow_frames-=1;
				if allow_frames==0{
					let mut headers=self.headers.clone();
					headers.append("X-Proxy-Error",format!("FramesLimit {}",FRAMES_LIMIT).parse().unwrap());
					return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
				}
				if let Ok(frame)=frame{
					timestamp+=std::time::Duration::from(frame.delay()).as_millis() as i32;
					let img=image::DynamicImage::ImageRgba8(frame.into_buffer());
					let img=match self.resize(img){
						Some(img)=>img,
						None=>return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
					};
					if let Some(size)=size{
						if size.0==img.width()&&size.1==img.height(){
							//ok
						}else{
							continue;
						}
					}else{
						size=Some((img.width(),img.height()));
						encoder=Some(webp::AnimEncoder::new(img.width(),img.height(),&conf));
					}
					let aframe=image_to_frame(&img,timestamp);
					if let Ok(aframe)=aframe{
						let res=encoder.as_mut().unwrap().add_frame(aframe);
						if let Err(e)=res{
							err=Some(e);
						}else{
							available_frames+=1;
						}
					}
				}else{
					break;
				}
			}
		}
		let mut headers=self.headers.clone();
		if size.is_none()||encoder.is_none(){
			headers.append("X-Proxy-Error","NoAvailableFrames0".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
		};
		if available_frames==0||encoder.is_none(){
			headers.append("X-Proxy-Error","NoAvailableFrames".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
		};
		let buf=encoder.unwrap().encode();
		headers.remove("Content-Type");
		headers.append("Content-Type","image/webp".parse().unwrap());
		headers.remove("Cache-Control");
		if let Some(e)=err{
			if let Ok(value)=format!("{:?}",e).parse(){
				headers.append("X-Proxy-Error",value);
			}
		}else{
			headers.append("Cache-Control","max-age=31536000, immutable".parse().unwrap());
		}
		Self::disposition_ext(&mut headers,".webp");
		(axum::http::StatusCode::OK,headers,buf.to_vec()).into_response()
	}
	fn encode_single(&mut self)->axum::response::Response{
		let img=match &self.codec{
			Ok(codec)=>image::load_from_memory_with_format(&self.src_bytes,*codec).map_err(|e|format!("{:?}",e)),
			Err(Some(e))=>Err(format!("{:?}",e)),
			_=>{
				self.headers.append("X-Proxy-Error","Unknown Format".parse().unwrap());
				return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
			}
		};
		let img=match img{
			Ok(img)=>img,
			Err(e)=>{
				self.headers.append("X-Proxy-Error",format!("DecodeError_{}",e).parse().unwrap());
				return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
			}
		};
		self.response_img(img)
	}
	pub(crate) fn response_img(&mut self,img:DynamicImage)->axum::response::Response{
		let img=match self.codec{
			Ok(image::ImageFormat::Jpeg)|Ok(image::ImageFormat::Tiff)=>{
				self.exif_rotate(img)
			},
			_=>img
		};
		let img=match self.resize(img){
			Some(img)=>img,
			None=>return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
		};
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

fn jpegxr_img(width:u32,height:u32,stride:usize,buffer:Vec<u8>,info:jpegxr::PixelFormat)->Option<DynamicImage>{
	match info{
		jpegxr::PixelFormat::PixelFormat8bppGray => {
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageLuma8(i))
		},
		jpegxr::PixelFormat::PixelFormat24bppBGR => {
			let mut buffer=buffer;
			for y in 0..height{
				for x in 0..width{
					let offset=y as usize*stride+x as usize*3;
					let r=buffer[offset];
					buffer[y as usize*stride+x as usize*4]=buffer[offset+2];
					buffer[offset+2]=r;
				}
			}
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageRgb8(i))
		},
		jpegxr::PixelFormat::PixelFormat24bppRGB => {
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageRgb8(i))
		},
		jpegxr::PixelFormat::PixelFormat32bppBGR => {
			let mut raw_img=Vec::with_capacity(height as usize*3);
			for y in 0..height{
				for x in 0..width{
					raw_img.push(buffer[y as usize*stride+x as usize*4+2]);
					raw_img.push(buffer[y as usize*stride+x as usize*4+1]);
					raw_img.push(buffer[y as usize*stride+x as usize*4+0]);
				}
			}
			image::ImageBuffer::from_raw(width,height,raw_img).map(|i|DynamicImage::ImageRgb8(i))
		},
		jpegxr::PixelFormat::PixelFormat32bppBGRA => {
			let mut buffer=buffer;
			for y in 0..height{
				for x in 0..width{
					let offset=y as usize*stride+x as usize*4;
					let r=buffer[offset];
					buffer[y as usize*stride+x as usize*4]=buffer[offset+2];
					buffer[offset+2]=r;
				}
			}
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageRgba8(i))
		},
		jpegxr::PixelFormat::PixelFormat32bppRGB => {
			let mut raw_img=Vec::with_capacity(height as usize*3);
			for y in 0..height{
				for x in 0..width{
					raw_img.push(buffer[y as usize*stride+x as usize*4+0]);
					raw_img.push(buffer[y as usize*stride+x as usize*4+1]);
					raw_img.push(buffer[y as usize*stride+x as usize*4+2]);
				}
			}
			image::ImageBuffer::from_raw(width,height,raw_img).map(|i|DynamicImage::ImageRgb8(i))
		},
		jpegxr::PixelFormat::PixelFormat32bppRGBA => {
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageRgba8(i))
		},
		_ => None,
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
fn resize(img:DynamicImage,max_width:u32,max_height:u32,filter:fast_image_resize::FilterType)->Option<DynamicImage>{
	let scale = f32::min(max_width as f32 / img.width() as f32,max_height as f32 / img.height() as f32);
	let dst_width=1.max((img.width() as f32 * scale).round() as u32);
	let dst_height=1.max((img.height() as f32 * scale).round() as u32);
	let src_image=fast_image_resize::images::Image::from_vec_u8(img.width(),img.height(),img.into_rgba8().into_raw(),fast_image_resize::PixelType::U8x4);
	let src_image=src_image.ok()?;
	let mut dst_image = fast_image_resize::images::Image::new(dst_width,dst_height,src_image.pixel_type());
	let mut resizer = fast_image_resize::Resizer::new();
	let options=fast_image_resize::ResizeOptions{
		algorithm:fast_image_resize::ResizeAlg::Convolution(filter),
		..Default::default()
	};
	resizer.resize(&src_image, &mut dst_image, &options).unwrap();
	let rgba=image::RgbaImage::from_raw(dst_image.width(),dst_image.height(),dst_image.into_vec());
	Some(DynamicImage::ImageRgba8(rgba?))
}
