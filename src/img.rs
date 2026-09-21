use axum::response::IntoResponse;
use image::{AnimationDecoder, DynamicImage, GenericImage, GenericImageView};

use crate::RequestContext;

/// Header-only dimension probe (no pixel allocation). Returns None for
/// formats the `image` crate cannot guess (JXL/JP2/JXR have per-path checks).
pub(crate) fn probe_dimensions(src: &[u8]) -> Option<(u32, u32)> {
	let reader = image::ImageReader::new(std::io::Cursor::new(src))
		.with_guessed_format()
		.ok()?;
	reader.into_dimensions().ok()
}

/// Shared decode-dimension policy (also used for SVG-embedded rasters, M-01).
pub(crate) fn dimensions_allowed_for(max_decode_pixels: u64, width: u64, height: u64) -> bool {
	if width == 0 || height == 0 {
		return false;
	}
	const MAX_SIDE: u64 = 32768;
	if width > MAX_SIDE || height > MAX_SIDE {
		return false;
	}
	match width.checked_mul(height) {
		Some(pixels) => pixels <= max_decode_pixels,
		None => false,
	}
}

fn le24(b: &[u8]) -> u32 {
	(b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16)
}

/// Shared animation frame cap. Enforced in three independent places
/// (`webp_animation_within_budget`'s pre-scan, `encode_img`'s in-loop
/// defense-in-depth, and `encode_anim`'s frame-collection loop) that must
/// agree on the same policy value (code-review finding: was three separate
/// literals/consts that could drift).
pub(crate) const ANIMATION_FRAMES_LIMIT: u64 = 1000;

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
fn webp_animation_within_budget(data: &[u8], max_decode_pixels: u64) -> Result<(), String> {
	if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
		return Ok(());
	}
	let mut off = 12usize;
	let mut canvas_pixels: Option<u64> = None;
	let mut frames = 0u64;
	while off + 8 <= data.len() {
		let fourcc = &data[off..off + 4];
		let size = u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]])
			as usize;
		let body = off + 8;
		if body.checked_add(size).map_or(true, |end| end > data.len()) {
			break;
		}
		if fourcc == b"VP8X" && size >= 10 {
			// Canvas Width/Height Minus One, 24-bit LE, at payload offsets 4 and 7.
			let w = 1u64 + le24(&data[body + 4..body + 7]) as u64;
			let h = 1u64 + le24(&data[body + 7..body + 10]) as u64;
			canvas_pixels = Some(w.saturating_mul(h));
		}
		if fourcc == b"ANMF" && size >= 16 {
			frames += 1;
			if frames > ANIMATION_FRAMES_LIMIT {
				return Err(format!("FramesLimit {}>{}", frames, ANIMATION_FRAMES_LIMIT));
			}
			// VP8X must precede ANMF per the container spec; if it is somehow
			// missing, fail closed (treat the per-frame cost as unbounded)
			// rather than silently allowing an unmeasured animation through.
			let per_frame = canvas_pixels.unwrap_or(u64::MAX);
			let total = per_frame.saturating_mul(frames);
			if total > max_decode_pixels {
				return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
			}
		}
		off = body + size + (size & 1);
	}
	Ok(())
}

/// APNG事前スキャン(`frames*canvas_pixels`予算、`webp_animation_within_budget`と同じ
/// 考え方):`image`クレートがフレームデータをデコードする前に、`IHDR`/`acTL`チャンク
/// ヘッダのみを使って過大なアニメーションを拒否する。
fn png_apng_within_budget(data: &[u8], max_decode_pixels: u64) -> Result<(), String> {
	const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
	if !data.starts_with(&SIG) {
		return Ok(());
	}
	let mut off = 8usize;
	let mut canvas_pixels: Option<u64> = None;
	while off + 8 <= data.len() {
		let len =
			u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
		let ctype = &data[off + 4..off + 8];
		let body = off + 8;
		// 末尾のCRCのために+4。
		let end = match body.checked_add(len).and_then(|e| e.checked_add(4)) {
			Some(end) if end <= data.len() => end,
			_ => break,
		};
		if ctype == b"IHDR" && len >= 8 {
			let w = u32::from_be_bytes([data[body], data[body + 1], data[body + 2], data[body + 3]])
				as u64;
			let h = u32::from_be_bytes([
				data[body + 4],
				data[body + 5],
				data[body + 6],
				data[body + 7],
			]) as u64;
			canvas_pixels = Some(w.saturating_mul(h));
		}
		if ctype == b"acTL" && len >= 4 {
			let frames =
				u32::from_be_bytes([data[body], data[body + 1], data[body + 2], data[body + 3]])
					as u64;
			// acTLはIDAT/fdATより前に来ることが必須なので、canvas_pixels
			// (常に先頭にあるIHDR由来)はこの時点で既に判明している。
			if frames > ANIMATION_FRAMES_LIMIT {
				return Err(format!("FramesLimit {}>{}", frames, ANIMATION_FRAMES_LIMIT));
			}
			let total = canvas_pixels.unwrap_or(u64::MAX).saturating_mul(frames);
			if total > max_decode_pixels {
				return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
			}
			return Ok(());
		}
		if ctype == b"IDAT" {
			// 最初のIDATの前にacTLが無い:この方法では予算計算できないアニメーション
			// なので、通常のAPNG/PNGデコード経路に任せる。
			break;
		}
		off = end;
	}
	Ok(())
}

/// GIF事前スキャン(`frames*canvas_pixels`予算、`webp_animation_within_budget`と同じ
/// 考え方):GIFには事前のフレーム数が無いため、ブロック構造(拡張ブロックと画像
/// ディスクリプタ)を走査してピクセルデータをデコードせずにフレームを数える。
fn gif_animation_within_budget(data: &[u8], max_decode_pixels: u64) -> Result<(), String> {
	if data.len() < 13 || !(data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
		return Ok(());
	}
	let canvas_pixels = (u16::from_le_bytes([data[6], data[7]]) as u64)
		.saturating_mul(u16::from_le_bytes([data[8], data[9]]) as u64);
	let packed = data[10];
	let mut off = 13usize;
	if packed & 0x80 != 0 {
		let gct_bytes = (2usize << (packed & 0x07)) * 3;
		off = match off.checked_add(gct_bytes) {
			Some(off) if off <= data.len() => off,
			_ => return Ok(()),
		};
	}
	let mut frames = 0u64;
	loop {
		let Some(&tag) = data.get(off) else {
			return Ok(());
		};
		match tag {
			0x21 => {
				// 拡張ブロック:introducer+ラベル、その後に長さ前置きのサブブロックが
				// ゼロ長ブロックで終端される。
				off += 2;
				loop {
					let Some(&block_size) = data.get(off) else {
						return Ok(());
					};
					off += 1;
					if block_size == 0 {
						break;
					}
					off = match off.checked_add(block_size as usize) {
						Some(off) if off <= data.len() => off,
						_ => return Ok(()),
					};
				}
			}
			0x2C => {
				// 画像ディスクリプタ:これが1フレーム。
				frames += 1;
				if frames > ANIMATION_FRAMES_LIMIT {
					return Err(format!("FramesLimit {}>{}", frames, ANIMATION_FRAMES_LIMIT));
				}
				let total = canvas_pixels.saturating_mul(frames);
				if total > max_decode_pixels {
					return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
				}
				if off + 10 > data.len() {
					return Ok(());
				}
				let local_packed = data[off + 9];
				off += 10;
				if local_packed & 0x80 != 0 {
					let lct_bytes = (2usize << (local_packed & 0x07)) * 3;
					off = match off.checked_add(lct_bytes) {
						Some(off) if off <= data.len() => off,
						_ => return Ok(()),
					};
				}
				// LZW最小コードサイズ、その後に長さ前置きの画像サブブロック。
				off += 1;
				loop {
					let Some(&block_size) = data.get(off) else {
						return Ok(());
					};
					off += 1;
					if block_size == 0 {
						break;
					}
					off = match off.checked_add(block_size as usize) {
						Some(off) if off <= data.len() => off,
						_ => return Ok(()),
					};
				}
			}
			_ => return Ok(()), // トレーラー(0x3B)または想定外のタグ:ここで停止。
		}
	}
}

impl RequestContext {
	/// Upper bound on decoded pixels so a small file cannot expand into
	/// gigabytes of RAM (finding #4). Tied to max_size: decoded RGBA must fit
	/// within the same byte budget as the download itself.
	pub(crate) fn max_decode_pixels(&self) -> u64 {
		(self.config.max_size / 4).max(1)
	}
	pub(crate) fn dimensions_allowed(&self, width: u64, height: u64) -> bool {
		dimensions_allowed_for(self.max_decode_pixels(), width, height)
	}
	fn decode_limit_response(&mut self, msg: String) -> axum::response::Response {
		// msg is built from numbers only, so this parse is infallible; never unwrap (finding #3).
		let value = reqwest::header::HeaderValue::from_bytes(msg.as_bytes())
			.unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("DecodeLimit"));
		self.headers.append("X-Proxy-Error", value);
		(axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response()
	}
}

impl RequestContext {
	pub(crate) fn image_size_hint(&self) -> (u32, u32) {
		if self.parms.badge.is_some() {
			return (96, 96);
		}
		if self.parms.r#static.is_some() {
			return (498, 422);
		}
		if self.parms.emoji.is_some() {
			return (u32::MAX, 128);
		}
		if self.parms.preview.is_some() {
			return (200, 200);
		}
		if self.parms.avatar.is_some() {
			return (u32::MAX, 320);
		}
		(self.config.max_pixels, self.config.max_pixels)
	}
	pub(crate) fn resize(&self, img: DynamicImage) -> Option<DynamicImage> {
		let (width, height) = self.image_size_hint();
		if self.parms.badge.is_some() {
			let img = if img.dimensions() == (width, height) {
				img
			} else {
				resize(img, width, height, self.config.filter_type.into())?
			};
			let img = img.into_luma8();
			let mut canvas = image::GrayAlphaImage::new(width, height);
			let x_start = (width - img.width()) / 2;
			let y_start = (height - img.height()) / 2;
			let mut sub_canvas =
				canvas.sub_image(x_start, y_start, width - x_start, height - y_start);
			let mut y = 0;
			for rows in img.rows() {
				let mut x = 0;
				for p in rows {
					let p: image::LumaA<u8> = [p.0[0], p.0[0]].into();
					sub_canvas.put_pixel(x, y, p);
					x += 1;
				}
				y += 1;
			}
			return Some(DynamicImage::ImageLumaA8(canvas));
		}
		let max_width = width.min(img.width());
		let max_height = height.min(img.height());
		let filter = self.config.filter_type.into();
		if img.dimensions() == (max_width, max_height) {
			return Some(img);
		}
		resize(img, max_width, max_height, filter)
	}
	pub(crate) fn encode_img(&mut self) -> axum::response::Response {
		// Pre-decode dimension gate for image-crate formats (finding #4).
		// JXL/JP2/JXR return early below with their own header checks.
		if self.codec.is_ok() {
			if let Some((w, h)) = probe_dimensions(&self.src_bytes) {
				if !self.dimensions_allowed(w as u64, h as u64) {
					return self
						.decode_limit_response(format!("DecodeDimensions {}x{} over limit", w, h));
				}
			}
		}
		if self.parms.r#static.is_some() {
			return self.encode_single();
		}
		if self.parms.badge.is_some() {
			return self.encode_single();
		}
		let codec = match &self.codec {
			Ok(codec) => codec,
			Err(e) => {
				match self
					.headers
					.get("Content-Type")
					.map(|s| std::str::from_utf8(s.as_bytes()))
				{
					Some(Ok("image/jxl")) => {
						// 静止画・動画のJPEG XLは `encode_jxl` に一本化する。
						// アニメヘッダと読み込み済みキーフレーム数で静止画か動画かを判定する
						//（jxl-oxideの `image` 統合はアニメをデコードできないため、
						// `JxlDecoder` の代わりに低レベルAPIの `JxlImage` を使う）。
						return self.encode_jxl();
					}
					Some(Ok("image/jp2")) => {
						// Header-only gate before pixel allocation (finding #4).
						// DumpImage::from_bytes parses headers via read_header without decoding pixels.
						let dims = match jpeg2k::DumpImage::from_bytes(&self.src_bytes) {
							Ok(dump) => Some((dump.img.width(), dump.img.height())),
							Err(_) => None,
						};
						if let Some((w, h)) = dims {
							if !self.dimensions_allowed(w as u64, h as u64) {
								return self.decode_limit_response(format!(
									"DecodeDimensions {}x{} over limit",
									w, h
								));
							}
						}
						let img = jpeg2k::Image::from_bytes(&self.src_bytes)
							.map(|img| DynamicImage::try_from(&img));
						let img = img
							.map(|r| r.map_err(|e| e.to_string()))
							.map_err(|e| e.to_string())
							.unwrap_or_else(|e| Err(e));
						let img = match img {
							Ok(img) => img,
							Err(e) => {
								self.headers.append(
									"X-Proxy-Error",
									format!("Jpeg2000 Error:{:?}", e).parse().unwrap(),
								);
								return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
									.into_response();
							}
						};
						return self.response_img(img);
					}
					Some(Ok("image/jxr")) => {
						let max_pixels = self.max_decode_pixels();
						fn decode_jxr(
							src_bytes: &[u8],
							max_pixels: u64,
						) -> Result<Result<DynamicImage, String>, jpegxr::JXRError> {
							use jpegxr::{ImageDecode, PixelInfo};
							let mut decoder =
								ImageDecode::with_reader(std::io::Cursor::new(src_bytes))?;
							let (width, height) = decoder.get_size()?;
							// Gate before Vec allocation (finding #4).
							let (w, h) = (width as u64, height as u64);
							if !crate::img::dimensions_allowed_for(max_pixels, w, h) {
								return Ok(Err(format!(
									"DecodeDimensions {}x{} over limit",
									width, height
								)));
							}
							let info = PixelInfo::from_format(decoder.get_pixel_format()?);
							let stride = width as usize * info.bits_per_pixel() as usize / 8;
							let size = stride * height as usize;
							let mut buffer = Vec::<u8>::with_capacity(size);
							buffer.resize(size, 0);
							decoder.alpha_mode(info.has_alpha());
							decoder.copy_all(&mut buffer, stride)?;
							let img = jpegxr_img(
								width as u32,
								height as u32,
								stride,
								buffer,
								info.format(),
							);
							Ok(img.ok_or_else(|| {
								format!(
									"color_format={:?}&bgr={}&channels={}&format={:?}",
									info.color_format(),
									info.bgr(),
									info.channels(),
									info.format()
								)
							}))
						}
						match decode_jxr(&self.src_bytes, max_pixels) {
							Ok(Ok(img)) => {
								return self.response_img(img);
							}
							Ok(Err(e)) => {
								self.headers.append(
									"X-Proxy-Error",
									format!("JpegXR decode pixels {:?}", e).parse().unwrap(),
								);
								return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
									.into_response();
							}
							Err(e) => {
								self.headers.append(
									"X-Proxy-Error",
									format!("JpegXR decode bytes {:?}", e).parse().unwrap(),
								);
								return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
									.into_response();
							}
						}
					}
					_ => {
						self.headers.append(
							"X-Proxy-Error",
							format!("CodecError:{:?}", e).parse().unwrap(),
						);
						return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
							.into_response();
					}
				}
			}
		};
		match codec {
			image::ImageFormat::Png => {
				let a = match image::codecs::png::PngDecoder::new(std::io::Cursor::new(
					&self.src_bytes,
				)) {
					Ok(a) => a,
					Err(_) => return self.encode_single(),
				};
				if !a.is_apng().unwrap_or(false) {
					return self.encode_single();
				}
				// デコーダが全フレームを確保する前に、過大なアニメーションを拒否する
				// (上記のWebP経路と同じ考え方)。
				if let Err(e) = png_apng_within_budget(&self.src_bytes, self.max_decode_pixels()) {
					self.headers
						.append("X-Proxy-Error", format!("ApngAnim {}", e).parse().unwrap());
					return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
						.into_response();
				}
				match a.apng() {
					Ok(frames) => {
						let loop_count = 0; //TODO 現在ループ回数を取得するAPIが無いため無限ループ
						self.encode_anim(frames.into_frames(), loop_count)
					}
					Err(_) => self.encode_single(),
				}
			}
			image::ImageFormat::Gif => {
				// デコーダが全フレームを確保する前に、過大なアニメーションを拒否する
				// (上記のWebP経路と同じ考え方)。
				if let Err(e) =
					gif_animation_within_budget(&self.src_bytes, self.max_decode_pixels())
				{
					self.headers
						.append("X-Proxy-Error", format!("GifAnim {}", e).parse().unwrap());
					return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
						.into_response();
				}
				match image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&self.src_bytes)) {
					Ok(a) => {
						let loop_count = 0; //TODO 現在ループ回数を取得するAPIが無いため無限ループ
						self.encode_anim(a.into_frames(), loop_count)
					}
					Err(_) => self.encode_single(),
				}
			}
			image::ImageFormat::WebP => {
				let a = match image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(
					&self.src_bytes,
				)) {
					Ok(a) => a,
					Err(_) => return self.encode_single(),
				};
				if a.has_animation() {
					// Refuse oversized animations before the decoder
					// allocates every frame (M-02).
					if let Err(e) =
						webp_animation_within_budget(&self.src_bytes, self.max_decode_pixels())
					{
						self.headers
							.append("X-Proxy-Error", format!("WebPAnim {}", e).parse().unwrap());
						return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
							.into_response();
					}
					let decoder = webp::AnimDecoder::new(&self.src_bytes);
					if let Ok(mut dec) = decoder.decode() {
						let mut offset = 0;
						let mut frames = vec![];
						dec.sort_by_time_stamp();
						for frame in dec.into_iter() {
							// Defense in depth if the container pre-scan could not
							// parse the file (M-02).
							if frames.len() >= ANIMATION_FRAMES_LIMIT as usize {
								let mut headers = self.headers.clone();
								headers.append(
									"X-Proxy-Error",
									format!("FramesLimit {}", ANIMATION_FRAMES_LIMIT)
										.parse()
										.unwrap(),
								);
								return (axum::http::StatusCode::BAD_GATEWAY, headers)
									.into_response();
							}
							let img = if frame.get_layout().is_alpha() {
								let Some(image) = image::ImageBuffer::from_raw(
									frame.width(),
									frame.height(),
									frame.get_image().to_owned(),
								) else {
									continue;
								};
								image
							} else {
								let Some(image) = image::ImageBuffer::from_raw(
									frame.width(),
									frame.height(),
									frame.get_image().to_owned(),
								) else {
									continue;
								};
								DynamicImage::ImageRgb8(image).into_rgba8()
							};
							let delay = frame.get_time_ms() - offset;
							offset = frame.get_time_ms();
							if delay < 0 {
								continue;
							}
							let delay = std::time::Duration::from_millis(delay as u64);
							let delay = image::Delay::from_saturating_duration(delay);
							let frame = image::Frame::from_parts(img, 0, 0, delay);
							frames.push(Ok(frame));
						}
						let frames = image::Frames::new(Box::new(frames.into_iter()));
						self.encode_anim(frames, dec.loop_count)
					} else {
						self.encode_anim(a.into_frames(), 0)
					}
				} else {
					self.encode_single()
				}
			}
			_ => self.encode_single(),
		}
	}
	/// 静止画・動画のJPEG XLをデコードして再エンコードする。
	///
	/// jxl-oxideの `image` クレート統合（`JxlDecoder`）はキーフレーム0しか
	/// 描画しないためアニメは作れない。代わりに低レベルAPIの `JxlImage`
	/// を使う。`read` はコードストリーム全体をパースするが、保持するのは
	/// 圧縮グループとフレームヘッダのみで `max_size` 以内に収まる
	///（`render_frame` まではフルキャンバスのバッファを確保しない）。
	/// 先にキャンバス寸法・フレーム数・累積ピクセル予算でゲートし、
	/// その後各キーフレームを描画して `encode_anim` に渡す
	///（APNG/GIF/WebPと同じアニメWebP出力先）。
	fn encode_jxl(&mut self) -> axum::response::Response {
		let mut image = match jxl_oxide::JxlImage::builder()
			.read(std::io::Cursor::new(&self.src_bytes))
		{
			Ok(image) => image,
			Err(e) => {
				self.headers.append("X-Proxy-Error", jxl_error_value(e));
				return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response();
			}
		};
		// jxl-oxideはCMYKのカラーマネジメントをしないためsRGBを要求する。
		// こうすると後段の8bitストリームは生CMYK(A)ではなくRGB(A)になる。
		if image.pixel_format().has_black() {
			image.request_color_encoding(jxl_oxide::EnumColourEncoding::srgb(
				jxl_oxide::RenderingIntent::Relative,
			));
		}
		// キーフレーム描画前のヘッダのみによるゲート（finding #4）。
		let (w, h) = (image.width(), image.height());
		if !self.dimensions_allowed(w as u64, h as u64) {
			return self.decode_limit_response(format!("DecodeDimensions {}x{} over limit", w, h));
		}
		let keyframes = image.num_loaded_keyframes();
		// コンテナがアニメ宣言を持ち、かつ実際に2つ以上のキーフレームが
		// デコードできた場合のみアニメとして扱う。
		let animated = image.image_header().metadata.animation.is_some() && keyframes > 1;
		// 切詰めコードストリームでも `read` は読み込めた範囲でOkを返すため、
		// そのprefixを完全なアニメとして扱わない（静止画は後段でフレーム毎に
		// fail-closedするため、このゲートはアニメ分岐のみでよい）。
		if animated && !image.is_loading_done() {
			self.headers
				.append("X-Proxy-Error", "JpegXLTruncated".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response();
		}
		if !animated {
			let render = match image.render_frame(0) {
				Ok(render) => render,
				Err(e) => {
					self.headers.append("X-Proxy-Error", jxl_error_value(e));
					return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
						.into_response();
				}
			};
			let img = match jxl_render_to_image(&render) {
				Some(img) => img,
				None => {
					self.headers.append(
						"X-Proxy-Error",
						"JpegXLUnsupportedPixelFormat".parse().unwrap(),
					);
					return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
						.into_response();
				}
			};
			return self.response_img(img);
		}
		// WebP事前スキャン（M-02）に倣ったアニメ予算。フレーム数と
		// frames*canvas_pixelsがデコード予算に収まること。`render_frame` は
		// フレーム自体の範囲に関わらずキーフレーム毎にフルキャンバスの
		// バッファを返すため、コストは部分矩形の合計ではなく
		// frames*canvas_pixelsで見積もる。
		let max_decode_pixels = self.max_decode_pixels();
		let canvas_pixels = (w as u64).saturating_mul(h as u64);
		let frame_count = keyframes as u64;
		if frame_count > ANIMATION_FRAMES_LIMIT {
			return self.decode_limit_response(format!(
				"FramesLimit {}>{}",
				frame_count, ANIMATION_FRAMES_LIMIT
			));
		}
		let total = canvas_pixels.saturating_mul(frame_count);
		if total > max_decode_pixels {
			return self
				.decode_limit_response(format!("DecodePixels {}>{}", total, max_decode_pixels));
		}
		// TPS（ticks per second）= tps_numerator / tps_denominator なので
		// ms = ticks * tps_denominator * 1000 / tps_numerator。正常なアニメなら
		// tps_numeratorは1以上だが、0への念のためガードを入れる。
		let anim = image.image_header().metadata.animation.as_ref().unwrap();
		let tps_num = (anim.tps_numerator as u64).max(1);
		let tps_den = anim.tps_denominator as u64;
		let loop_count = anim.num_loops;
		let mut collected: Vec<Result<image::Frame, image::ImageError>> =
			Vec::with_capacity(keyframes.min(ANIMATION_FRAMES_LIMIT as usize));
		for keyframe in 0..keyframes {
			// フレーム上限に対する多重防御（M-02）。
			if collected.len() >= ANIMATION_FRAMES_LIMIT as usize {
				return self
					.decode_limit_response(format!("FramesLimit {}", ANIMATION_FRAMES_LIMIT));
			}
			let render = match image.render_frame(keyframe) {
				Ok(render) => render,
				Err(e) => {
					self.headers.append("X-Proxy-Error", jxl_error_value(e));
					return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
						.into_response();
				}
			};
			let img = match jxl_render_to_image(&render) {
				Some(img) => img,
				None => {
					self.headers.append(
						"X-Proxy-Error",
						"JpegXLUnsupportedPixelFormat".parse().unwrap(),
					);
					return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone())
						.into_response();
				}
			};
			let dur_ms = (render.duration() as u64)
				.saturating_mul(tps_den)
				.saturating_mul(1000)
				/ tps_num;
			let delay =
				image::Delay::from_saturating_duration(std::time::Duration::from_millis(dur_ms));
			collected.push(Ok(image::Frame::from_parts(img.into_rgba8(), 0, 0, delay)));
		}
		if collected.is_empty() {
			self.headers
				.append("X-Proxy-Error", "NoAvailableFrames".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response();
		}
		let frames = image::Frames::new(Box::new(collected.into_iter()));
		self.encode_anim(frames, loop_count)
	}
	fn encode_anim(&self, frames: image::Frames, loop_count: u32) -> axum::response::Response {
		let conf = webp::WebPConfig::new().unwrap();
		let mut size: Option<(u32, u32)> = None;
		let mut encoder = None;
		let mut available_frames = 0;
		let mut err = None;
		{
			let mut timestamp = 0;
			const FRAMES_LIMIT: u64 = ANIMATION_FRAMES_LIMIT;
			let mut frame_index: u64 = 0;
			for frame in frames {
				// フレームを消費する前にチェックするので、ちょうどFRAMES_LIMITフレームまで
				// 成功する。これは事前スキャンゲート(webp/jxl)が`frame_count==ANIMATION_FRAMES_LIMIT`
				// を許可する仕様と一致させる(オフバイワン修正)。
				if frame_index >= FRAMES_LIMIT {
					let mut headers = self.headers.clone();
					headers.append(
						"X-Proxy-Error",
						format!("FramesLimit {}", FRAMES_LIMIT).parse().unwrap(),
					);
					return (axum::http::StatusCode::BAD_GATEWAY, headers).into_response();
				}
				frame_index += 1;
				if let Ok(frame) = frame {
					timestamp += std::time::Duration::from(frame.delay()).as_millis() as i32;
					let img = image::DynamicImage::ImageRgba8(frame.into_buffer());
					let img = match self.resize(img) {
						Some(img) => img,
						None => {
							return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
						}
					};
					if let Some(size) = size {
						if size.0 == img.width() && size.1 == img.height() {
							//ok
						} else {
							continue;
						}
					} else {
						size = Some((img.width(), img.height()));
						encoder = Some({
							let mut encoder =
								webp::AnimEncoder::new(img.width(), img.height(), &conf);
							encoder.set_loop_count(loop_count.try_into().unwrap_or_default());
							encoder
						});
					}
					let aframe = image_to_frame(&img, timestamp);
					if let Ok(aframe) = aframe {
						if let Some(encoder) = encoder.as_mut() {
							let res = encoder.add_frame(aframe);
							if let Err(e) = res {
								err = Some(e);
							} else {
								available_frames += 1;
							}
						}
					}
				} else {
					break;
				}
			}
		}
		let mut headers = self.headers.clone();
		if size.is_none() || encoder.is_none() {
			headers.append("X-Proxy-Error", "NoAvailableFrames0".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY, headers).into_response();
		};
		if available_frames == 0 || encoder.is_none() {
			headers.append("X-Proxy-Error", "NoAvailableFrames".parse().unwrap());
			return (axum::http::StatusCode::BAD_GATEWAY, headers).into_response();
		};
		let buf = encoder.unwrap().encode();
		headers.remove("Content-Type");
		headers.append("Content-Type", "image/webp".parse().unwrap());
		headers.remove("Cache-Control");
		if let Some(e) = err {
			if let Ok(value) = format!("{:?}", e).parse() {
				headers.append("X-Proxy-Error", value);
			}
		} else {
			headers.append(
				"Cache-Control",
				"max-age=31536000, immutable".parse().unwrap(),
			);
		}
		Self::disposition_ext(&mut headers, ".webp");
		(axum::http::StatusCode::OK, headers, buf.to_vec()).into_response()
	}
	fn encode_single(&mut self) -> axum::response::Response {
		let img = match &self.codec {
			Ok(codec) => image::load_from_memory_with_format(&self.src_bytes, *codec)
				.map_err(|e| format!("{:?}", e)),
			Err(Some(e)) => Err(format!("{:?}", e)),
			_ => {
				self.headers
					.append("X-Proxy-Error", "Unknown Format".parse().unwrap());
				return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response();
			}
		};
		let img = match img {
			Ok(img) => img,
			Err(e) => {
				self.headers.append(
					"X-Proxy-Error",
					format!("DecodeError_{}", e).parse().unwrap(),
				);
				return (axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response();
			}
		};
		self.response_img(img)
	}
	pub(crate) fn response_img(&mut self, img: DynamicImage) -> axum::response::Response {
		let img = match self.codec {
			Ok(image::ImageFormat::Jpeg) | Ok(image::ImageFormat::Tiff) => self.exif_rotate(img),
			_ => img,
		};
		let img = match self.resize(img) {
			Some(img) => img,
			None => return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
		};
		let mut buf = vec![];
		self.headers.remove("Content-Type");
		let format = if self.parms.badge.is_some() {
			self.headers
				.append("Content-Type", "image/png".parse().unwrap());
			Self::disposition_ext(&mut self.headers, ".png");
			image::ImageFormat::Png
		} else {
			if self.is_accept_avif {
				self.headers
					.append("Content-Type", "image/avif".parse().unwrap());
				Self::disposition_ext(&mut self.headers, ".avif");
				image::ImageFormat::Avif
			} else {
				let width = img.width();
				let height = img.height();
				let rgba = img.into_rgba8();
				let encoer = webp::Encoder::from_rgba(rgba.as_raw(), width, height);
				let mut config = webp::WebPConfig::new().unwrap();
				config.quality = self.config.webp_quality;
				return match encoer.encode_advanced(&config) {
					Ok(mem) => {
						buf.extend_from_slice(&mem);
						self.headers
							.append("Content-Type", "image/webp".parse().unwrap());
						self.headers.remove("Cache-Control");
						self.headers.append(
							"Cache-Control",
							"max-age=31536000, immutable".parse().unwrap(),
						);
						Self::disposition_ext(&mut self.headers, ".webp");
						(axum::http::StatusCode::OK, self.headers.clone(), buf).into_response()
					}
					Err(e) => {
						self.headers.append(
							"X-Proxy-Error",
							format!("EncodeError_{:?}", e).parse().unwrap(),
						);
						(axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response()
					}
				};
			}
		};
		match img.write_to(&mut std::io::Cursor::new(&mut buf), format) {
			Ok(_) => {
				self.headers.remove("Cache-Control");
				self.headers.append(
					"Cache-Control",
					"max-age=31536000, immutable".parse().unwrap(),
				);
				(axum::http::StatusCode::OK, self.headers.clone(), buf).into_response()
			}
			Err(e) => {
				self.headers.append(
					"X-Proxy-Error",
					format!("EncodeError_{:?}", e).parse().unwrap(),
				);
				(axum::http::StatusCode::BAD_GATEWAY, self.headers.clone()).into_response()
			}
		}
	}
	pub fn exif_rotate(&self, img: DynamicImage) -> DynamicImage {
		let exifreader = rexif::parse_buffer_quiet(&self.src_bytes);
		if let Ok(exif) = exifreader.0 {
			for e in exif.entries {
				match e.tag {
					rexif::ExifTag::Orientation => {
						return match e.value.to_i64(0).unwrap_or(0) {
							2 => DynamicImage::ImageRgba8(image::imageops::flip_horizontal(&img)),
							3 => DynamicImage::ImageRgba8(image::imageops::rotate180(&img)),
							4 => DynamicImage::ImageRgba8(image::imageops::flip_vertical(&img)),
							5 => DynamicImage::ImageRgba8(image::imageops::flip_horizontal(
								&image::imageops::rotate90(&img),
							)),
							6 => DynamicImage::ImageRgba8(image::imageops::rotate90(&img)),
							7 => DynamicImage::ImageRgba8(image::imageops::flip_horizontal(
								&image::imageops::rotate270(&img),
							)),
							8 => DynamicImage::ImageRgba8(image::imageops::rotate270(&img)),
							_ => img,
						};
					}
					_ => {}
				}
			}
		}
		img
	}
}

fn jpegxr_img(
	width: u32,
	height: u32,
	stride: usize,
	buffer: Vec<u8>,
	info: jpegxr::PixelFormat,
) -> Option<DynamicImage> {
	match info {
		jpegxr::PixelFormat::PixelFormat8bppGray => {
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageLuma8(i))
		}
		jpegxr::PixelFormat::PixelFormat24bppBGR => {
			let mut buffer = buffer;
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 3;
					let r = buffer[offset];
					buffer[offset] = buffer[offset + 2];
					buffer[offset + 2] = r;
				}
			}
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat24bppRGB => {
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppBGR => {
			let mut raw_img = Vec::with_capacity(width as usize * height as usize * 3);
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 4;
					raw_img.push(buffer[offset + 2]);
					raw_img.push(buffer[offset + 1]);
					raw_img.push(buffer[offset + 0]);
				}
			}
			image::ImageBuffer::from_raw(width, height, raw_img).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppBGRA => {
			let mut buffer = buffer;
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 4;
					let r = buffer[offset];
					buffer[offset] = buffer[offset + 2];
					buffer[offset + 2] = r;
				}
			}
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgba8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppRGB => {
			let mut raw_img = Vec::with_capacity(height as usize * 3);
			for y in 0..height {
				for x in 0..width {
					let offset = y as usize * stride + x as usize * 4;
					raw_img.push(buffer[offset + 0]);
					raw_img.push(buffer[offset + 1]);
					raw_img.push(buffer[offset + 2]);
				}
			}
			image::ImageBuffer::from_raw(width, height, raw_img).map(|i| DynamicImage::ImageRgb8(i))
		}
		jpegxr::PixelFormat::PixelFormat32bppRGBA => {
			image::ImageBuffer::from_raw(width, height, buffer).map(|i| DynamicImage::ImageRgba8(i))
		}
		_ => None,
	}
}

/// jxl-oxideのエラーから `X-Proxy-Error` 値を組み立てる。
///
/// `Debug` 出力は外部由来バイトに基づくためヘッダ不正文字を含む場合が
/// あり、unwrapしてはならない（finding #3）。
fn jxl_error_value(e: impl std::fmt::Debug) -> reqwest::header::HeaderValue {
	reqwest::header::HeaderValue::from_bytes(format!("JpegXL Error:{:?}", e).as_bytes())
		.unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("JpegXLError"))
}

/// jxl-oxideの `Render` 1件を8bitサンプルの `DynamicImage` に変換する。
///
/// ストリームは色＋（任意で）black＋（任意で）alphaチャネルを持ち、
/// orientationと要求した色エンコーディング（CMYK用にsRGB）を適用済み。
/// CMYKは上流の `request_color_encoding` で変換済みのため、ここに届くのは
/// 最大4チャネル。それ以外は未対応として扱う。
fn jxl_render_to_image(render: &jxl_oxide::Render) -> Option<DynamicImage> {
	let mut stream = render.stream();
	let width = stream.width();
	let height = stream.height();
	let channels = stream.channels() as usize;
	let len = (width as usize)
		.checked_mul(height as usize)?
		.checked_mul(channels)?;
	let mut buf = vec![0u8; len];
	stream.write_to_buffer(&mut buf);
	match channels {
		1 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageLuma8),
		2 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageLumaA8),
		3 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageRgb8),
		4 => image::ImageBuffer::from_raw(width, height, buf).map(DynamicImage::ImageRgba8),
		_ => None,
	}
}

pub fn image_to_frame(
	image: &DynamicImage,
	timestamp: i32,
) -> Result<webp::AnimFrame, &'static str> {
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
fn resize(
	img: DynamicImage,
	max_width: u32,
	max_height: u32,
	filter: fast_image_resize::FilterType,
) -> Option<DynamicImage> {
	let scale = f32::min(
		max_width as f32 / img.width() as f32,
		max_height as f32 / img.height() as f32,
	);
	let dst_width = 1.max((img.width() as f32 * scale).round() as u32);
	let dst_height = 1.max((img.height() as f32 * scale).round() as u32);
	let src_image = fast_image_resize::images::Image::from_vec_u8(
		img.width(),
		img.height(),
		img.into_rgba8().into_raw(),
		fast_image_resize::PixelType::U8x4,
	);
	let src_image = src_image.ok()?;
	let mut dst_image =
		fast_image_resize::images::Image::new(dst_width, dst_height, src_image.pixel_type());
	let mut resizer = fast_image_resize::Resizer::new();
	let options = fast_image_resize::ResizeOptions {
		algorithm: fast_image_resize::ResizeAlg::Convolution(filter),
		..Default::default()
	};
	if resizer
		.resize(&src_image, &mut dst_image, &options)
		.is_err()
	{
		return None;
	}
	let rgba =
		image::RgbaImage::from_raw(dst_image.width(), dst_image.height(), dst_image.into_vec());
	Some(DynamicImage::ImageRgba8(rgba?))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{ConfigFile, FilterType, RequestParams};

	fn png_chunk(ctype: &[u8; 4], data: &[u8]) -> Vec<u8> {
		let mut v = Vec::new();
		v.extend_from_slice(&(data.len() as u32).to_be_bytes());
		v.extend_from_slice(ctype);
		v.extend_from_slice(data);
		v.extend_from_slice(&[0, 0, 0, 0]); //CRCはpng_apng_within_budgetで検証されない
		v
	}
	fn build_apng(width: u32, height: u32, frames: u32) -> Vec<u8> {
		let mut v = vec![137, 80, 78, 71, 13, 10, 26, 10];
		let mut ihdr = Vec::new();
		ihdr.extend_from_slice(&width.to_be_bytes());
		ihdr.extend_from_slice(&height.to_be_bytes());
		ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
		v.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
		let mut actl = Vec::new();
		actl.extend_from_slice(&frames.to_be_bytes());
		actl.extend_from_slice(&0u32.to_be_bytes());
		v.extend_from_slice(&png_chunk(b"acTL", &actl));
		v
	}
	fn build_png_without_actl(width: u32, height: u32) -> Vec<u8> {
		let mut v = vec![137, 80, 78, 71, 13, 10, 26, 10];
		let mut ihdr = Vec::new();
		ihdr.extend_from_slice(&width.to_be_bytes());
		ihdr.extend_from_slice(&height.to_be_bytes());
		ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
		v.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
		v.extend_from_slice(&png_chunk(b"IDAT", &[0, 1, 2, 3]));
		v
	}

	#[test]
	fn apng_non_png_data_is_ok() {
		assert!(png_apng_within_budget(b"not a png at all", 1_000_000).is_ok());
	}
	#[test]
	fn apng_within_budget_is_ok() {
		let data = build_apng(10, 10, 5);
		assert!(png_apng_within_budget(&data, 10_000).is_ok());
	}
	#[test]
	fn apng_frames_at_limit_is_ok() {
		// ちょうどANIMATION_FRAMES_LIMITフレームは許可される(オフバイワン境界)。
		let data = build_apng(1, 1, ANIMATION_FRAMES_LIMIT as u32);
		assert!(png_apng_within_budget(&data, ANIMATION_FRAMES_LIMIT).is_ok());
	}
	#[test]
	fn apng_frames_over_limit_is_err() {
		let data = build_apng(1, 1, ANIMATION_FRAMES_LIMIT as u32 + 1);
		let err = png_apng_within_budget(&data, ANIMATION_FRAMES_LIMIT + 1).unwrap_err();
		assert!(err.contains("FramesLimit"), "{}", err);
	}
	#[test]
	fn apng_decode_pixels_over_budget_is_err() {
		let data = build_apng(100_000, 100_000, 2);
		let err = png_apng_within_budget(&data, 1_000).unwrap_err();
		assert!(err.contains("DecodePixels"), "{}", err);
	}
	#[test]
	fn png_without_actl_is_ok() {
		// acTLの無い通常PNGはIDATで走査を止め、通常のPNGデコード経路に任せる。
		let data = build_png_without_actl(10, 10);
		assert!(png_apng_within_budget(&data, 1_000_000).is_ok());
	}
	#[test]
	fn apng_truncated_chunk_is_ok() {
		// 長さが宣言長より短い壊れたチャンクではpanicせず、走査を止めて
		// 通常のデコード経路にfail-openする。
		let mut data = vec![137, 80, 78, 71, 13, 10, 26, 10];
		data.extend_from_slice(&[0, 0, 0, 20]);
		data.extend_from_slice(b"IHDR");
		assert!(png_apng_within_budget(&data, 1_000_000).is_ok());
	}

	fn gif_frame(width: u16, height: u16) -> Vec<u8> {
		let mut v = vec![0x2C];
		v.extend_from_slice(&0u16.to_le_bytes()); //left
		v.extend_from_slice(&0u16.to_le_bytes()); //top
		v.extend_from_slice(&width.to_le_bytes());
		v.extend_from_slice(&height.to_le_bytes());
		v.push(0); //packed:LCT無し
		v.push(2); //LZW最小コードサイズ
		v.push(1); //サブブロック長
		v.push(0); //画像データ1バイト
		v.push(0); //ゼロ長ブロックで終端
		v
	}
	fn gif_ext() -> Vec<u8> {
		vec![0x21, 0xF9, 4, 0, 0, 0, 0, 0]
	}
	fn build_gif(canvas_w: u16, canvas_h: u16, frames: &[Vec<u8>]) -> Vec<u8> {
		let mut v = Vec::new();
		v.extend_from_slice(b"GIF89a");
		v.extend_from_slice(&canvas_w.to_le_bytes());
		v.extend_from_slice(&canvas_h.to_le_bytes());
		v.push(0); //packed:GCT無し
		v.push(0); //背景色インデックス
		v.push(0); //画素比
		for f in frames {
			v.extend_from_slice(f);
		}
		v.push(0x3B); //トレーラー
		v
	}

	#[test]
	fn gif_non_gif_data_is_ok() {
		assert!(gif_animation_within_budget(b"not a gif", 1_000_000).is_ok());
	}
	#[test]
	fn gif_single_frame_is_ok() {
		let data = build_gif(10, 10, &[gif_frame(10, 10)]);
		assert!(gif_animation_within_budget(&data, 1_000).is_ok());
	}
	#[test]
	fn gif_frames_at_limit_is_ok() {
		let frames: Vec<Vec<u8>> = (0..ANIMATION_FRAMES_LIMIT)
			.map(|_| gif_frame(1, 1))
			.collect();
		let data = build_gif(1, 1, &frames);
		assert!(gif_animation_within_budget(&data, ANIMATION_FRAMES_LIMIT).is_ok());
	}
	#[test]
	fn gif_frames_over_limit_is_err() {
		let frames: Vec<Vec<u8>> = (0..ANIMATION_FRAMES_LIMIT + 1)
			.map(|_| gif_frame(1, 1))
			.collect();
		let data = build_gif(1, 1, &frames);
		let err = gif_animation_within_budget(&data, ANIMATION_FRAMES_LIMIT + 1).unwrap_err();
		assert!(err.contains("FramesLimit"), "{}", err);
	}
	#[test]
	fn gif_decode_pixels_over_budget_is_err() {
		let data = build_gif(65000, 65000, &[gif_frame(1, 1)]);
		let err = gif_animation_within_budget(&data, 1_000).unwrap_err();
		assert!(err.contains("DecodePixels"), "{}", err);
	}
	#[test]
	fn gif_extension_blocks_are_not_counted_as_frames() {
		// 拡張ブロックをANIMATION_FRAMES_LIMITを超える数だけ挟んでも、実フレームは
		// 1枚だけなのでフレーム数予算には影響しない。
		let mut frames: Vec<Vec<u8>> = (0..ANIMATION_FRAMES_LIMIT + 1).map(|_| gif_ext()).collect();
		frames.push(gif_frame(10, 10));
		let data = build_gif(10, 10, &frames);
		assert!(gif_animation_within_budget(&data, 1_000).is_ok());
	}

	fn test_request_context(max_size: u64) -> RequestContext {
		RequestContext {
			is_accept_avif: false,
			headers: axum::http::HeaderMap::new(),
			parms: RequestParams {
				url: String::new(),
				r#static: None,
				emoji: None,
				avatar: None,
				preview: None,
				badge: None,
				fallback: None,
			},
			src_bytes: Vec::new(),
			config: std::sync::Arc::new(ConfigFile {
				bind_addr: "0.0.0.0:0".to_owned(),
				timeout: 1000,
				user_agent: "test".to_owned(),
				max_size,
				proxy: None,
				filter_type: FilterType::Triangle,
				max_pixels: 2048,
				append_headers: vec![],
				load_system_fonts: false,
				webp_quality: 75.0,
				encode_avif: false,
				allowed_networks: None,
				blocked_networks: None,
				blocked_hosts: None,
				unix_socket_permissions: None,
			}),
			codec: Err(None),
			dummy_img: std::sync::Arc::new(Vec::new()),
			fontdb: std::sync::Arc::new(resvg::usvg::fontdb::Database::new()),
		}
	}
	fn make_frame() -> Result<image::Frame, image::ImageError> {
		let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
		Ok(image::Frame::from_parts(
			img,
			0,
			0,
			image::Delay::from_saturating_duration(std::time::Duration::from_millis(10)),
		))
	}

	#[test]
	fn encode_anim_allows_exactly_frame_limit() {
		// オフバイワン修正の回帰テスト:ちょうどANIMATION_FRAMES_LIMIT枚は成功する
		// (修正前は`allow_frames`が1000枚目で0になり誤ってFramesLimitエラーになっていた)。
		let frames: Vec<_> = (0..ANIMATION_FRAMES_LIMIT).map(|_| make_frame()).collect();
		let frames = image::Frames::new(Box::new(frames.into_iter()));
		let ctx = test_request_context(1_000_000);
		let resp = ctx.encode_anim(frames, 0);
		assert_eq!(resp.status(), axum::http::StatusCode::OK);
	}
	#[test]
	fn encode_anim_rejects_over_frame_limit() {
		let frames: Vec<_> = (0..ANIMATION_FRAMES_LIMIT + 1)
			.map(|_| make_frame())
			.collect();
		let frames = image::Frames::new(Box::new(frames.into_iter()));
		let ctx = test_request_context(1_000_000);
		let resp = ctx.encode_anim(frames, 0);
		assert_eq!(resp.status(), axum::http::StatusCode::BAD_GATEWAY);
		let err = resp
			.headers()
			.get("X-Proxy-Error")
			.unwrap()
			.to_str()
			.unwrap();
		assert!(
			err.contains(format!("FramesLimit {}", ANIMATION_FRAMES_LIMIT).as_str()),
			"{}",
			err
		);
	}
}
