
use axum::response::IntoResponse;
use image::{AnimationDecoder, DynamicImage, GenericImage, GenericImageView};

use crate::RequestContext;

/// Header-only dimension probe (no pixel allocation). Returns None for
/// formats the `image` crate cannot guess (JXL/JP2/JXR have per-path checks).
pub(crate) fn probe_dimensions(src:&[u8])->Option<(u32,u32)>{
	let reader=image::ImageReader::new(std::io::Cursor::new(src)).with_guessed_format().ok()?;
	reader.into_dimensions().ok()
}

/// Shared decode-dimension policy (also used for SVG-embedded rasters, M-01).
pub(crate) fn dimensions_allowed_for(max_decode_pixels:u64,width:u64,height:u64)->bool{
	if width==0||height==0{
		return false;
	}
	const MAX_SIDE:u64=32768;
	if width>MAX_SIDE||height>MAX_SIDE{
		return false;
	}
	match width.checked_mul(height){
		Some(pixels)=>pixels<=max_decode_pixels,
		None=>false,
	}
}

fn le24(b:&[u8])->u32{
	(b[0] as u32)|((b[1] as u32)<<8)|((b[2] as u32)<<16)
}

/// Shared animation frame cap. Enforced in three independent places
/// (`webp_animation_within_budget`'s pre-scan, `encode_img`'s in-loop
/// defense-in-depth, and `encode_anim`'s frame-collection loop) that must
/// agree on the same policy value (code-review finding: was three separate
/// literals/consts that could drift).
pub(crate) const ANIMATION_FRAMES_LIMIT:u64=1000;

/// RIFF/WebP animation pre-scan (M-02). Walk the container and reject
/// oversized animations (frame count and cumulative per-frame pixels) before
/// decoding.
///
/// The forked `AnimDecoder` materializes every frame before
/// `ANIMATION_FRAMES_LIMIT` is applied in `encode_img`'s loop, so a
/// few-hundred-KiB file can expand to gigabytes. Crucially, each frame it
/// returns is a *full VP8X-canvas-sized* buffer (`WebPAnimDecoderGetNext`
/// always hands back the fully composited canvas), regardless of that
/// frame's own declared ANMF sub-rectangle -- a spec-legal partial update can
/// declare a 1x1 rectangle while the decoder still allocates a full-canvas
/// buffer for it. So the budget below is `frames * canvas_pixels`, not a sum
/// of ANMF sub-rectangle areas (code-review finding: the previous version
/// summed sub-rectangle areas, which a crafted file could keep near zero
/// while still declaring thousands of frames against a large canvas,
/// defeating this check entirely).
fn webp_animation_within_budget(data:&[u8],max_decode_pixels:u64)->Result<(),String>{
	if data.len()<12||&data[0..4]!=b"RIFF"||&data[8..12]!=b"WEBP"{
		return Ok(());
	}
	let mut off=12usize;
	let mut canvas_pixels:Option<u64>=None;
	let mut frames=0u64;
	while off+8<=data.len(){
		let fourcc=&data[off..off+4];
		let size=u32::from_le_bytes([data[off+4],data[off+5],data[off+6],data[off+7]]) as usize;
		let body=off+8;
		if body.checked_add(size).map_or(true,|end|end>data.len()){
			break;
		}
		if fourcc==b"VP8X"&&size>=10{
			// Canvas Width/Height Minus One, 24-bit LE, at payload offsets 4 and 7.
			let w=1u64+le24(&data[body+4..body+7]) as u64;
			let h=1u64+le24(&data[body+7..body+10]) as u64;
			canvas_pixels=Some(w.saturating_mul(h));
		}
		if fourcc==b"ANMF"&&size>=16{
			frames+=1;
			if frames>ANIMATION_FRAMES_LIMIT{
				return Err(format!("FramesLimit {}>{}",frames,ANIMATION_FRAMES_LIMIT));
			}
			// VP8X must precede ANMF per the container spec; if it is somehow
			// missing, fail closed (treat the per-frame cost as unbounded)
			// rather than silently allowing an unmeasured animation through.
			let per_frame=canvas_pixels.unwrap_or(u64::MAX);
			let total=per_frame.saturating_mul(frames);
			if total>max_decode_pixels{
				return Err(format!("DecodePixels {}>{}",total,max_decode_pixels));
			}
		}
		off=body+size+(size&1);
	}
	Ok(())
}

impl RequestContext{
	/// Upper bound on decoded pixels so a small file cannot expand into
	/// gigabytes of RAM (finding #4). Tied to max_size: decoded RGBA must fit
	/// within the same byte budget as the download itself.
	pub(crate) fn max_decode_pixels(&self)->u64{
		(self.config.max_size/4).max(1)
	}
	pub(crate) fn dimensions_allowed(&self,width:u64,height:u64)->bool{
		dimensions_allowed_for(self.max_decode_pixels(),width,height)
	}
	fn decode_limit_response(&mut self,msg:String)->axum::response::Response{
		// msg is built from numbers only, so this parse is infallible; never unwrap (finding #3).
		let value=reqwest::header::HeaderValue::from_bytes(msg.as_bytes()).unwrap_or_else(|_|reqwest::header::HeaderValue::from_static("DecodeLimit"));
		self.headers.append("X-Proxy-Error",value);
		(axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response()
	}
}

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
		// Pre-decode dimension gate for image-crate formats (finding #4).
		// JXL/JP2/JXR return early below with their own header checks.
		if self.codec.is_ok(){
			if let Some((w,h))=probe_dimensions(&self.src_bytes){
				if !self.dimensions_allowed(w as u64,h as u64){
					return self.decode_limit_response(format!("DecodeDimensions {}x{} over limit",w,h));
				}
			}
		}
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
					// Animated and still JPEG XL share one path; `encode_jxl`
					// decides which based on the animation header and the number
					// of loaded keyframes (jxl-oxide's `image` integration does
					// not decode animation, so the low-level `JxlImage` API is
					// used here instead of `JxlDecoder`).
					return self.encode_jxl();
				}
				Some(Ok("image/jp2"))=>{
					// Header-only gate before pixel allocation (finding #4).
					// DumpImage::from_bytes parses headers via read_header without decoding pixels.
					let dims=match jpeg2k::DumpImage::from_bytes(&self.src_bytes){
						Ok(dump)=>Some((dump.img.width(),dump.img.height())),
						Err(_)=>None,
					};
					if let Some((w,h))=dims{
						if !self.dimensions_allowed(w as u64,h as u64){
							return self.decode_limit_response(format!("DecodeDimensions {}x{} over limit",w,h));
						}
					}
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
					let max_pixels=self.max_decode_pixels();
					fn decode_jxr(src_bytes:&[u8],max_pixels:u64)->Result<Result<DynamicImage,String>, jpegxr::JXRError>{
						use jpegxr::{ImageDecode, PixelInfo};
						let mut decoder = ImageDecode::with_reader(std::io::Cursor::new(src_bytes))?;
						let (width, height) = decoder.get_size()?;
						// Gate before Vec allocation (finding #4).
						let (w,h)=(width as u64,height as u64);
						if !crate::img::dimensions_allowed_for(max_pixels,w,h){
							return Ok(Err(format!("DecodeDimensions {}x{} over limit",width,height)));
						}
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
						match decode_jxr(&self.src_bytes,max_pixels){
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
			if !a.is_apng().unwrap_or(false){
				return self.encode_single();
			}
				match a.apng(){
					Ok(frames)=>{
						let loop_count=0;//TODO 現在ループ回数を取得するAPIが無いため無限ループ
						self.encode_anim(frames.into_frames(),loop_count)
					},
					Err(_)=>self.encode_single()
				}
			},
			image::ImageFormat::Gif => {
				match image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&self.src_bytes)){
					Ok(a)=>{
						let loop_count=0;//TODO 現在ループ回数を取得するAPIが無いため無限ループ
						self.encode_anim(a.into_frames(),loop_count)
					},
					Err(_)=>self.encode_single()
				}
			},
			image::ImageFormat::WebP => {
				let a=match image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&self.src_bytes)){
					Ok(a)=>a,
					Err(_)=>return self.encode_single()
				};
				if a.has_animation(){
					// Refuse oversized animations before the decoder
					// allocates every frame (M-02).
					if let Err(e)=webp_animation_within_budget(&self.src_bytes,self.max_decode_pixels()){
						self.headers.append("X-Proxy-Error",format!("WebPAnim {}",e).parse().unwrap());
						return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
					}
					let decoder=webp::AnimDecoder::new(&self.src_bytes);
					if let Ok(mut dec)=decoder.decode(){
						let mut offset=0;
						let mut frames=vec![];
						dec.sort_by_time_stamp();
					for frame in dec.into_iter(){
						// Defense in depth if the container pre-scan could not
						// parse the file (M-02).
						if frames.len()>=ANIMATION_FRAMES_LIMIT as usize{
							let mut headers=self.headers.clone();
							headers.append("X-Proxy-Error",format!("FramesLimit {}",ANIMATION_FRAMES_LIMIT).parse().unwrap());
							return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
						}
						let img=if frame.get_layout().is_alpha() {
							let Some(image)=
								image::ImageBuffer::from_raw(frame.width(), frame.height(), frame.get_image().to_owned())
							else{
								continue;
							};
							image
						} else {
							let Some(image)=
								image::ImageBuffer::from_raw(frame.width(), frame.height(), frame.get_image().to_owned())
							else{
								continue;
							};
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
						self.encode_anim(frames,dec.loop_count)
					}else{
						self.encode_anim(a.into_frames(),0)
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
	/// Decode + re-encode JPEG XL, handling both still and animated files.
	///
	/// jxl-oxide's `image`-crate integration (`JxlDecoder`) only ever renders
	/// keyframe 0, so animation cannot be produced through it. Instead the
	/// low-level `JxlImage` API is used: `read` parses the whole codestream
	/// (bounded by `max_size`, since it only stores compressed groups and frame
	/// headers -- no full-canvas buffers are allocated until `render_frame`),
	/// the canvas dimensions / frame count / cumulative-pixel budget are gated
	/// first, and only then is each keyframe rendered and fed to `encode_anim`
	/// (the same animated-WebP sink used for APNG/GIF/WebP).
	fn encode_jxl(&mut self)->axum::response::Response{
		let mut image=match jxl_oxide::JxlImage::builder().read(std::io::Cursor::new(&self.src_bytes)){
			Ok(image)=>image,
			Err(e)=>{
				// Debug output derives from external bytes and may contain
				// header-invalid characters; never unwrap (finding #3).
				let value=reqwest::header::HeaderValue::from_bytes(format!("JpegXL Error:{:?}",e).as_bytes()).unwrap_or_else(|_|reqwest::header::HeaderValue::from_static("JpegXLError"));
				self.headers.append("X-Proxy-Error",value);
				return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
			},
		};
		// jxl-oxide does not color-manage CMYK itself; request sRGB so the 8-bit
		// stream below yields RGB(A) rather than raw CMYK(A) samples.
		if image.pixel_format().has_black(){
			image.request_color_encoding(jxl_oxide::EnumColourEncoding::srgb(jxl_oxide::RenderingIntent::Relative));
		}
		// Header-only gate before any keyframe is rendered (finding #4).
		let (w,h)=(image.width(),image.height());
		if !self.dimensions_allowed(w as u64,h as u64){
			return self.decode_limit_response(format!("DecodeDimensions {}x{} over limit",w,h));
		}
		let keyframes=image.num_loaded_keyframes();
		// Only treat as animated when the container declares an animation and
		// more than one keyframe was actually decoded.
		let animated=image.image_header().metadata.animation.is_some()&&keyframes>1;
		if !animated{
			let render=match image.render_frame(0){
				Ok(render)=>render,
				Err(e)=>{
					let value=reqwest::header::HeaderValue::from_bytes(format!("JpegXL Error:{:?}",e).as_bytes()).unwrap_or_else(|_|reqwest::header::HeaderValue::from_static("JpegXLError"));
					self.headers.append("X-Proxy-Error",value);
					return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
				},
			};
			let img=match jxl_render_to_image(&render){
				Some(img)=>img,
				None=>{
					self.headers.append("X-Proxy-Error","JpegXLUnsupportedPixelFormat".parse().unwrap());
					return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
				},
			};
			return self.response_img(img);
		}
		// Animation budget, mirroring the WebP pre-scan (M-02): frame count and
		// frames*canvas_pixels must fit the same decode budget. `render_frame`
		// hands back a full-canvas buffer per keyframe regardless of that frame's
		// own extent, so the cost is frames*canvas_pixels, not a sum of sub-rects.
		let max_decode_pixels=self.max_decode_pixels();
		let canvas_pixels=(w as u64).saturating_mul(h as u64);
		let frame_count=keyframes as u64;
		if frame_count>ANIMATION_FRAMES_LIMIT{
			return self.decode_limit_response(format!("FramesLimit {}>{}",frame_count,ANIMATION_FRAMES_LIMIT));
		}
		let total=canvas_pixels.saturating_mul(frame_count);
		if total>max_decode_pixels{
			return self.decode_limit_response(format!("DecodePixels {}>{}",total,max_decode_pixels));
		}
		// TPS (ticks per second) = tps_numerator / tps_denominator, so
		// ms = ticks * tps_denominator * 1000 / tps_numerator. tps_numerator is
		// >=1 for a well-formed animation; guard against 0 anyway.
		let anim=image.image_header().metadata.animation.as_ref().unwrap();
		let tps_num=(anim.tps_numerator as u64).max(1);
		let tps_den=anim.tps_denominator as u64;
		let loop_count=anim.num_loops;
		let mut collected:Vec<Result<image::Frame,image::ImageError>>=Vec::with_capacity(keyframes.min(ANIMATION_FRAMES_LIMIT as usize));
		for keyframe in 0..keyframes{
			// Defense in depth against the frame cap (M-02).
			if collected.len()>=ANIMATION_FRAMES_LIMIT as usize{
				return self.decode_limit_response(format!("FramesLimit {}",ANIMATION_FRAMES_LIMIT));
			}
			let render=match image.render_frame(keyframe){
				Ok(render)=>render,
				Err(_)=>break,
			};
			let img=match jxl_render_to_image(&render){
				Some(img)=>img,
				None=>break,
			};
			let dur_ms=(render.duration() as u64).saturating_mul(tps_den).saturating_mul(1000)/tps_num;
			let delay=image::Delay::from_saturating_duration(std::time::Duration::from_millis(dur_ms));
			collected.push(Ok(image::Frame::from_parts(img.into_rgba8(),0,0,delay)));
		}
		if collected.is_empty(){
			self.headers.append("X-Proxy-Error","NoAvailableFrames".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY,self.headers.clone()).into_response();
		}
		let frames=image::Frames::new(Box::new(collected.into_iter()));
		self.encode_anim(frames,loop_count)
	}
	fn encode_anim(&self,frames:image::Frames,loop_count:u32)->axum::response::Response{
		let conf=webp::WebPConfig::new().unwrap();
		let mut size:Option<(u32, u32)>=None;
		let mut encoder=None;
		let mut available_frames=0;
		let mut err=None;
		{
			let mut timestamp=0;
			const FRAMES_LIMIT:u32=ANIMATION_FRAMES_LIMIT as u32;
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
						encoder=Some({
							let mut encoder=webp::AnimEncoder::new(img.width(),img.height(),&conf);
							encoder.set_loop_count(loop_count.try_into().unwrap_or_default());
							encoder
						});
					}
				let aframe=image_to_frame(&img,timestamp);
				if let Ok(aframe)=aframe{
					if let Some(encoder)=encoder.as_mut(){
						let res=encoder.add_frame(aframe);
						if let Err(e)=res{
							err=Some(e);
						}else{
							available_frames+=1;
						}
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
					buffer[offset]=buffer[offset+2];
					buffer[offset+2]=r;
				}
			}
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageRgb8(i))
		},
		jpegxr::PixelFormat::PixelFormat24bppRGB => {
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageRgb8(i))
		},
		jpegxr::PixelFormat::PixelFormat32bppBGR => {
			let mut raw_img=Vec::with_capacity(width as usize*height as usize*3);
			for y in 0..height{
				for x in 0..width{
					let offset=y as usize*stride+x as usize*4;
					raw_img.push(buffer[offset+2]);
					raw_img.push(buffer[offset+1]);
					raw_img.push(buffer[offset+0]);
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
					buffer[offset]=buffer[offset+2];
					buffer[offset+2]=r;
				}
			}
			image::ImageBuffer::from_raw(width,height,buffer).map(|i|DynamicImage::ImageRgba8(i))
		},
		jpegxr::PixelFormat::PixelFormat32bppRGB => {
			let mut raw_img=Vec::with_capacity(height as usize*3);
			for y in 0..height{
				for x in 0..width{
					let offset=y as usize*stride+x as usize*4;
					raw_img.push(buffer[offset+0]);
					raw_img.push(buffer[offset+1]);
					raw_img.push(buffer[offset+2]);
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

/// Convert one jxl-oxide `Render` into a `DynamicImage` of 8-bit samples.
///
/// The stream carries color + (optional) black + (optional) alpha channels,
/// with orientation and the requested color encoding (sRGB for CMYK) applied.
/// CMYK is converted upstream via `request_color_encoding`, so at most 4
/// channels reach here; anything else is treated as unsupported.
fn jxl_render_to_image(render:&jxl_oxide::Render)->Option<DynamicImage>{
	let mut stream=render.stream();
	let width=stream.width();
	let height=stream.height();
	let channels=stream.channels() as usize;
	let len=(width as usize).checked_mul(height as usize)?.checked_mul(channels)?;
	let mut buf=vec![0u8;len];
	stream.write_to_buffer(&mut buf);
	match channels{
		1=>image::ImageBuffer::from_raw(width,height,buf).map(DynamicImage::ImageLuma8),
		2=>image::ImageBuffer::from_raw(width,height,buf).map(DynamicImage::ImageLumaA8),
		3=>image::ImageBuffer::from_raw(width,height,buf).map(DynamicImage::ImageRgb8),
		4=>image::ImageBuffer::from_raw(width,height,buf).map(DynamicImage::ImageRgba8),
		_=>None,
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
	if resizer.resize(&src_image, &mut dst_image, &options).is_err(){
		return None;
	}
	let rgba=image::RgbaImage::from_raw(dst_image.width(),dst_image.height(),dst_image.into_vec());
	Some(DynamicImage::ImageRgba8(rgba?))
}
