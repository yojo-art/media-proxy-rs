//! MNG(Multiple-image Network Graphics)のデコード
//! MNG-LC相当: 埋め込みPNG(IHDR..IEND)をフレームとして抽出しキャンバスへ合成
//! JNG相当: 埋め込みJPEG(JHDR..IEND, JDAT/IDAT/JDAA)をフレームとして抽出しキャンバスへ合成

use crate::img::dimensions_allowed_for;

pub(crate) const SIGNATURE: [u8; 8] = [0x8a, 0x4d, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

pub(crate) struct MngAnimation {
	pub frames: Vec<image::Frame>,
	pub loop_count: u32,
}

/// 収集中のJNG(埋め込みJPEG)1オブジェクト
struct JngPending {
	width: u32,
	height: u32,
	/// JHDR alpha_compression_method (0=IDATのPNGグレースケール, 8=JDAAのJPEGグレースケール)
	alpha_method: u8,
	/// JHDR alpha_sample_depth
	alpha_depth: u8,
	/// JDATチャンクを連結した8bit JPEGストリーム(JSEP以降は12bitなので無視)
	jpeg: Vec<u8>,
	/// IDATチャンクのペイロードを連結したもの(alpha_method=0のアルファマスク)
	alpha_png: Vec<u8>,
	/// JDAAチャンクを連結したJPEGグレースケール(alpha_method=8のアルファマスク)
	alpha_jpeg: Vec<u8>,
	after_jsep: bool,
}

fn be32(data: &[u8], offset: usize) -> u32 {
	u32::from_be_bytes([
		data[offset],
		data[offset + 1],
		data[offset + 2],
		data[offset + 3],
	])
}

pub(crate) fn decode(
	data: &[u8],
	max_decode_pixels: u64,
	frames_limit: u64,
	first_frame_only: bool,
) -> Result<MngAnimation, String> {
	if !data.starts_with(&SIGNATURE) {
		return Err("BadSignature".to_owned());
	}
	let mut off = 8usize;
	let mut canvas: Option<image::RgbaImage> = None;
	let mut canvas_pixels = 0u64;
	let mut tps = 1u64;
	let mut loop_count = 0u32;
	// FRAMで変更されるまでの既定値はモード1, 遅延1tick
	let mut framing_mode = 1u8;
	let mut default_delay_ticks = 1u64;
	let mut next_delay_ticks = 1u64;
	let mut loc_x = 0i64;
	let mut loc_y = 0i64;
	let mut frames: Vec<image::Frame> = Vec::new();
	// 収集中の埋め込みPNGの開始オフセット(IHDRチャンク先頭)
	let mut png_start: Option<usize> = None;
	// 収集中の埋め込みJNG(JPEG)オブジェクト
	let mut jng: Option<JngPending> = None;
	while off + 8 <= data.len() {
		let len = be32(data, off) as usize;
		let ctype = [data[off + 4], data[off + 5], data[off + 6], data[off + 7]];
		let body = off + 8;
		// 末尾のCRCのために+4
		let end = match body.checked_add(len).and_then(|e| e.checked_add(4)) {
			Some(end) if end <= data.len() => end,
			// 壊れたチャンクは走査を止め、取得済みフレームで判定
			_ => break,
		};
		match &ctype {
			b"MHDR" if len >= 28 && png_start.is_none() && jng.is_none() => {
				let width = be32(data, body);
				let height = be32(data, body + 4);
				if !dimensions_allowed_for(max_decode_pixels, width as u64, height as u64) {
					return Err(format!("DecodeDimensions {}x{} over limit", width, height));
				}
				tps = be32(data, body + 8) as u64;
				canvas_pixels = (width as u64).saturating_mul(height as u64);
				canvas = Some(image::RgbaImage::new(width, height));
			}
			b"TERM" if len >= 10 => {
				// action=3(繰り返し)の時のみ反復回数あり
				if data[body] == 3 {
					let iteration_max = be32(data, body + 6);
					// 0x7fffffff以上は無限ループ(webpのloop_count=0)
					loop_count = if iteration_max >= 0x7fffffff {
						0
					} else {
						iteration_max
					};
				}
			}
			b"FRAM" if len >= 1 && png_start.is_none() && jng.is_none() => {
				let mode = data[body];
				if mode != 0 {
					framing_mode = mode;
				}
				// サブフレーム名(0終端)の後に変更フラグ4バイトと変更値
				let mut p = body + 1;
				while p < body + len && data[p] != 0 {
					p += 1;
				}
				p += 1;
				if p + 4 <= body + len {
					let change_delay = data[p];
					let value = p + 4;
					if (change_delay == 1 || change_delay == 2) && value + 4 <= body + len {
						let ticks = be32(data, value) as u64;
						next_delay_ticks = ticks;
						if change_delay == 2 {
							default_delay_ticks = ticks;
						}
					}
				}
			}
			b"DEFI" if len >= 12 && png_start.is_none() => {
				loc_x = i32::from_be_bytes([
					data[body + 4],
					data[body + 5],
					data[body + 6],
					data[body + 7],
				]) as i64;
				loc_y = i32::from_be_bytes([
					data[body + 8],
					data[body + 9],
					data[body + 10],
					data[body + 11],
				]) as i64;
			}
			b"IHDR" if png_start.is_none() => {
				if len >= 8 {
					let width = be32(data, body);
					let height = be32(data, body + 4);
					if !dimensions_allowed_for(max_decode_pixels, width as u64, height as u64) {
						return Err(format!("DecodeDimensions {}x{} over limit", width, height));
					}
				}
				png_start = Some(off);
			}
			b"IEND" => {
				let img: image::RgbaImage = if let Some(start) = png_start.take() {
					canvas.as_ref().ok_or("IhdrBeforeMhdr")?;
					let frame_count = frames.len() as u64 + 1;
					if frame_count > frames_limit {
						return Err(format!("FramesLimit {}>{}", frame_count, frames_limit));
					}
					let total = canvas_pixels.saturating_mul(frame_count);
					if total > max_decode_pixels {
						return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
					}
					let mut png_bytes = Vec::with_capacity(8 + (end - start));
					png_bytes.extend_from_slice(&PNG_SIGNATURE);
					png_bytes.extend_from_slice(&data[start..end]);
					image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
						.map_err(|e| format!("EmbeddedPng {:?}", e))?
						.into_rgba8()
				} else if let Some(j) = jng.take() {
					canvas.as_ref().ok_or("JhdrBeforeMhdr")?;
					let frame_count = frames.len() as u64 + 1;
					if frame_count > frames_limit {
						return Err(format!("FramesLimit {}>{}", frame_count, frames_limit));
					}
					let total = canvas_pixels.saturating_mul(frame_count);
					if total > max_decode_pixels {
						return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
					}
					jng_to_rgba(j)?
				} else {
					break;
				};
				let canvas = canvas.as_mut().ok_or("NoCanvas")?;
				// モード3/4はフレーム毎に背景復元(透明で初期化)
				if framing_mode == 3 || framing_mode == 4 {
					for p in canvas.pixels_mut() {
						*p = image::Rgba([0, 0, 0, 0]);
					}
				}
				image::imageops::overlay(canvas, &img, loc_x, loc_y);
				let ticks = next_delay_ticks;
				next_delay_ticks = default_delay_ticks;
				let ms = ticks.saturating_mul(1000) / tps.max(1);
				let delay =
					image::Delay::from_saturating_duration(std::time::Duration::from_millis(ms));
				frames.push(image::Frame::from_parts(canvas.clone(), 0, 0, delay));
				if first_frame_only {
					break;
				}
			}
			// JNG(JPEG系サブストリーム)。JHDRはMNG/PNGメンバーと排他。
			b"JHDR" if len >= 16 && png_start.is_none() && jng.is_none() => {
				let width = be32(data, body);
				let height = be32(data, body + 4);
				if !dimensions_allowed_for(max_decode_pixels, width as u64, height as u64) {
					return Err(format!("DecodeDimensions {}x{} over limit", width, height));
				}
				// color_type: 8=Gray, 10=Color, 12=Gray-alpha, 14=Color-alpha
				let color_type = data[body + 8];
				if !matches!(color_type, 8 | 10 | 12 | 14) {
					return Err(format!("JngColorType {}", color_type));
				}
				// image_compression_method は 8 (baseline JPEG) のみ対応
				if data[body + 10] != 8 {
					return Err(format!("JngCompression {}", data[body + 10]));
				}
				// alpha_compression_method: 0=IDAT(PNG), 8=JDAA(JPEG)。alpha_sample_depth は 8bit のみ
				let alpha_method = data[body + 13];
				let alpha_ok = !matches!(color_type, 12 | 14)
					|| (matches!(alpha_method, 0 | 8) && data[body + 12] == 8);
				if !alpha_ok {
					return Err(format!(
						"JngAlpha depth={} method={}",
						data[body + 12], alpha_method
					));
				}
				jng = Some(JngPending {
					width,
					height,
					alpha_method,
					alpha_depth: data[body + 12],
					jpeg: Vec::new(),
					alpha_png: Vec::new(),
					alpha_jpeg: Vec::new(),
					after_jsep: false,
				});
			}
			b"JDAT" if jng.is_some() => {
				let j = jng.as_mut().unwrap();
				if !j.after_jsep {
					j.jpeg.extend_from_slice(&data[body..body + len]);
				}
			}
			b"JDAA" if jng.is_some() => {
				let j = jng.as_mut().unwrap();
				if !j.after_jsep {
					j.alpha_jpeg.extend_from_slice(&data[body..body + len]);
				}
			}
			b"IDAT" if jng.is_some() => {
				let j = jng.as_mut().unwrap();
				if !j.after_jsep {
					j.alpha_png.extend_from_slice(&data[body..body + len]);
				}
			}
			b"JSEP" => {
				// 8bit/12bit画像の分離子。以降のJDAT/JDAA/IDATは12bit列なので無視。
				if let Some(j) = jng.as_mut() {
					j.after_jsep = true;
				}
			}
			b"MEND" => break,
			_ => {}
		}
		off = end;
	}
	if frames.is_empty() {
		return Err("NoAvailableFrames".to_owned());
	}
	Ok(MngAnimation { frames, loop_count })
}

/// CRC-32 (PNG/MNG chunk と同じ多項式 0xEDB88320, 反射)
fn crc32(data: &[u8]) -> u32 {
	let mut crc = !0u32;
	for &b in data {
		crc ^= b as u32;
		for _ in 0..8 {
			let mask = (crc & 1).wrapping_neg();
			crc = (crc >> 1) ^ (0xEDB88320 & mask);
		}
	}
	!crc
}

/// PNGチャンク(長さ+型+データ+CRC)を合成
fn png_chunk(ctype: &[u8; 4], body: &[u8]) -> Vec<u8> {
	let mut v = Vec::with_capacity(12 + body.len());
	v.extend_from_slice(&(body.len() as u32).to_be_bytes());
	v.extend_from_slice(ctype);
	v.extend_from_slice(body);
	v.extend_from_slice(&crc32(&v[4..]).to_be_bytes());
	v
}

/// 収集済みのJNGをRgbaImage化。JPEGをデコードし、アルファマスク(IDAT/JDAA)があれば適用。
fn jng_to_rgba(j: JngPending) -> Result<image::RgbaImage, String> {
	let img = image::load_from_memory_with_format(&j.jpeg, image::ImageFormat::Jpeg)
		.map_err(|e| format!("JngJpeg {:?}", e))?;
	let mut rgba = img.into_rgba8();
	let alpha = match j.alpha_method {
		0 => decode_alpha_png(&j)?,
		8 => image::load_from_memory_with_format(&j.alpha_jpeg, image::ImageFormat::Jpeg)
			.map(|i| Some(i.into_luma8()))
			.map_err(|e| format!("JngAlphaJpeg {:?}", e))?,
		_ => None,
	};
	if let Some(alpha) = alpha {
		if alpha.width() != rgba.width() || alpha.height() != rgba.height() {
			return Err(format!(
				"JngAlphaDims {}x{} != {}x{}",
				alpha.width(),
				alpha.height(),
				rgba.width(),
				rgba.height()
			));
		}
		for (p, a) in rgba.pixels_mut().zip(alpha.pixels()) {
			p.0[3] = a.0[0];
		}
	}
	Ok(rgba)
}

/// JNGのIDAT(PNGグレースケール)をアルファマスクとしてデコード。
/// IDATは圧縮スキャンラインのみなので、IHDR(グレースケール)とIENDを合成して
/// 有効なPNGストリームを作りCRCを再計算してからデコードする。
fn decode_alpha_png(j: &JngPending) -> Result<Option<image::GrayImage>, String> {
	if j.alpha_png.is_empty() {
		return Ok(None);
	}
	let mut ihdr = Vec::with_capacity(13);
	ihdr.extend_from_slice(&j.width.to_be_bytes());
	ihdr.extend_from_slice(&j.height.to_be_bytes());
	ihdr.push(j.alpha_depth); // bit_depth
	ihdr.push(0); // color_type=0 grayscale
	ihdr.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
	let mut png = Vec::with_capacity(PNG_SIGNATURE.len() + ihdr.len() + j.alpha_png.len() + 24);
	png.extend_from_slice(&PNG_SIGNATURE);
	png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
	png.extend_from_slice(&png_chunk(b"IDAT", &j.alpha_png));
	png.extend_from_slice(&png_chunk(b"IEND", &[]));
	let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
		.map_err(|e| format!("JngAlphaPng {:?}", e))?;
	Ok(Some(img.into_luma8()))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn chunk(ctype: &[u8; 4], body: &[u8]) -> Vec<u8> {
		let mut v = Vec::new();
		v.extend_from_slice(&(body.len() as u32).to_be_bytes());
		v.extend_from_slice(ctype);
		v.extend_from_slice(body);
		v.extend_from_slice(&[0, 0, 0, 0]); //CRCは検証されない
		v
	}
	fn mhdr(width: u32, height: u32, tps: u32) -> Vec<u8> {
		let mut body = Vec::new();
		body.extend_from_slice(&width.to_be_bytes());
		body.extend_from_slice(&height.to_be_bytes());
		body.extend_from_slice(&tps.to_be_bytes());
		body.extend_from_slice(&[0u8; 16]);
		chunk(b"MHDR", &body)
	}
	fn embedded_png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
		let img = image::RgbaImage::from_pixel(width, height, image::Rgba(color));
		let mut buf = Vec::new();
		image::DynamicImage::ImageRgba8(img)
			.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
			.unwrap();
		//PNGシグネチャを除いたチャンク列
		buf[8..].to_vec()
	}
	fn build_mng(width: u32, height: u32, frame_colors: &[[u8; 4]]) -> Vec<u8> {
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(width, height, 100));
		for color in frame_colors {
			v.extend_from_slice(&embedded_png(width, height, *color));
		}
		v.extend_from_slice(&chunk(b"MEND", &[]));
		v
	}

	#[test]
	fn non_mng_data_is_err() {
		assert!(decode(b"not a mng", 1_000_000, 1000, false).is_err());
	}
	#[test]
	fn two_frames_decode() {
		let data = build_mng(4, 4, &[[255, 0, 0, 255], [0, 255, 0, 255]]);
		let anim = decode(&data, 1_000_000, 1000, false).unwrap();
		assert_eq!(anim.frames.len(), 2);
		assert_eq!(anim.frames[1].buffer().get_pixel(0, 0).0, [0, 255, 0, 255]);
	}
	#[test]
	fn first_frame_only_stops_after_one() {
		let data = build_mng(4, 4, &[[255, 0, 0, 255], [0, 255, 0, 255]]);
		let anim = decode(&data, 1_000_000, 1000, true).unwrap();
		assert_eq!(anim.frames.len(), 1);
		assert_eq!(anim.frames[0].buffer().get_pixel(0, 0).0, [255, 0, 0, 255]);
	}
	#[test]
	fn canvas_over_budget_is_err() {
		let data = build_mng(100, 100, &[[0, 0, 0, 255]]);
		let err = decode(&data, 100, 1000, false).err().unwrap();
		assert!(err.contains("DecodeDimensions"), "{}", err);
	}
	#[test]
	fn frames_over_limit_is_err() {
		let colors = vec![[0u8, 0, 0, 255]; 3];
		let data = build_mng(2, 2, &colors);
		let err = decode(&data, 1_000_000, 2, false).err().unwrap();
		assert!(err.contains("FramesLimit"), "{}", err);
	}
	#[test]
	fn cumulative_pixels_over_budget_is_err() {
		let colors = vec![[0u8, 0, 0, 255]; 4];
		let data = build_mng(10, 10, &colors);
		let err = decode(&data, 250, 1000, false).err().unwrap();
		assert!(err.contains("DecodePixels"), "{}", err);
	}
	#[test]
	fn fram_delay_applies() {
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(2, 2, 100));
		//モード1, 空名, 全サブフレームの遅延を50tickへ変更
		let mut fram = vec![1u8, 0, 2, 0, 0, 0];
		fram.extend_from_slice(&50u32.to_be_bytes());
		v.extend_from_slice(&chunk(b"FRAM", &fram));
		v.extend_from_slice(&embedded_png(2, 2, [0, 0, 255, 255]));
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let anim = decode(&v, 1_000_000, 1000, false).unwrap();
		let ms: std::time::Duration = anim.frames[0].delay().into();
		assert_eq!(ms.as_millis(), 500);
	}

	// --- JNG ---

	fn jpeg_bytes(img: &image::RgbImage) -> Vec<u8> {
		let mut buf = Vec::new();
		let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90);
		enc.encode(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8)
			.unwrap();
		buf
	}
	fn gray_jpeg_bytes(img: &image::GrayImage) -> Vec<u8> {
		let mut buf = Vec::new();
		let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90);
		enc.encode(
			img.as_raw(),
			img.width(),
			img.height(),
			image::ExtendedColorType::L8,
		)
		.unwrap();
		buf
	}
	/// idat_alpha=true のとき PNGグレースケール(8bit)のIDATペイロードを返す
	fn alpha_idat(gray: &image::GrayImage) -> Vec<u8> {
		let mut buf = Vec::new();
		image::DynamicImage::ImageLuma8(gray.clone())
			.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
			.unwrap();
		let mut idat = Vec::new();
		let mut off = 8usize;
		while off + 8 <= buf.len() {
			let len = be32(&buf, off) as usize;
			if &buf[off + 4..off + 8] == b"IDAT" {
				idat.extend_from_slice(&buf[off + 8..off + 8 + len]);
			}
			off += 12 + len;
		}
		idat
	}
	fn jhdr(width: u32, height: u32, color_type: u8, alpha_depth: u8, alpha_method: u8) -> Vec<u8> {
		let mut body = Vec::new();
		body.extend_from_slice(&width.to_be_bytes());
		body.extend_from_slice(&height.to_be_bytes());
		body.push(color_type);
		body.push(8); // image sample depth
		body.push(8); // compression = baseline JPEG
		body.push(0); // interlace
		body.push(alpha_depth);
		body.push(alpha_method);
		body.push(0);
		body.push(0);
		chunk(b"JHDR", &body)
	}
	fn jng_object(
		width: u32,
		height: u32,
		jpeg: &[u8],
		alpha_idat: &[u8],
		alpha_jpeg: &[u8],
	) -> Vec<u8> {
		let color_type = if alpha_idat.is_empty() && alpha_jpeg.is_empty() {
			10
		} else {
			14
		};
		let alpha_method = if alpha_jpeg.is_empty() { 0 } else { 8 };
		let mut v = jhdr(width, height, color_type, 8, alpha_method);
		v.extend_from_slice(&chunk(b"JDAT", jpeg));
		if !alpha_idat.is_empty() {
			v.extend_from_slice(&chunk(b"IDAT", alpha_idat));
		}
		if !alpha_jpeg.is_empty() {
			v.extend_from_slice(&chunk(b"JDAA", alpha_jpeg));
		}
		v.extend_from_slice(&chunk(b"IEND", &[]));
		v
	}
	fn expected_jpeg_pixel(jpeg: &[u8], x: u32, y: u32) -> [u8; 4] {
		let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
			.unwrap()
			.into_rgba8();
		let p = img.get_pixel(x, y).0;
		let mut a = [0u8; 4];
		a.copy_from_slice(&p);
		a
	}

	#[test]
	fn jng_color_frame_decodes() {
		let jpeg = jpeg_bytes(&image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0])));
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		v.extend_from_slice(&jng_object(4, 4, &jpeg, &[], &[]));
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let anim = decode(&v, 1_000_000, 1000, false).unwrap();
		assert_eq!(anim.frames.len(), 1);
		let got = anim.frames[0].buffer().get_pixel(0, 0).0;
		let want = expected_jpeg_pixel(&jpeg, 0, 0);
		for i in 0..3 {
			assert!((got[i] as i32 - want[i] as i32).abs() <= 1, "{:?} vs {:?}", got, want);
		}
		assert_eq!(got[3], 255);
	}
	#[test]
	fn jng_idat_alpha_applies() {
		let jpeg = jpeg_bytes(&image::RgbImage::from_pixel(4, 4, image::Rgb([0, 255, 0])));
		let alpha = image::GrayImage::from_pixel(4, 4, image::Luma([128u8]));
		let idat = alpha_idat(&alpha);
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		v.extend_from_slice(&jng_object(4, 4, &jpeg, &idat, &[]));
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let anim = decode(&v, 1_000_000, 1000, false).unwrap();
		let got = anim.frames[0].buffer().get_pixel(0, 0).0;
		assert_eq!(got[3], 128);
	}
	#[test]
	fn jng_jdaa_alpha_applies() {
		let jpeg = jpeg_bytes(&image::RgbImage::from_pixel(4, 4, image::Rgb([0, 0, 255])));
		let alpha = image::GrayImage::from_pixel(4, 4, image::Luma([200u8]));
		let alpha_jpeg = gray_jpeg_bytes(&alpha);
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		v.extend_from_slice(&jng_object(4, 4, &jpeg, &[], &alpha_jpeg));
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let anim = decode(&v, 1_000_000, 1000, false).unwrap();
		let got = anim.frames[0].buffer().get_pixel(0, 0).0;
		// JPEGは可逆でないため許容差
		assert!((got[3] as i32 - 200).abs() <= 2, "{}", got[3]);
	}
	#[test]
	fn jng_jsep_ignores_12bit_stream() {
		let jpeg = jpeg_bytes(&image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3])));
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		v.extend_from_slice(&jhdr(4, 4, 14, 8, 0));
		v.extend_from_slice(&chunk(b"JDAT", &jpeg));
		v.extend_from_slice(&chunk(b"IDAT", &alpha_idat(&image::GrayImage::from_pixel(4, 4, image::Luma([255u8])))));
		v.extend_from_slice(&chunk(b"JSEP", &[0u8; 2]));
		// 12bit列は完全に無視されること(壊れたJPEGでも読まない)
		v.extend_from_slice(&chunk(b"JDAT", b"not jpeg at all"));
		v.extend_from_slice(&chunk(b"IDAT", b"garbage"));
		v.extend_from_slice(&chunk(b"IEND", &[]));
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let anim = decode(&v, 1_000_000, 1000, false).unwrap();
		assert_eq!(anim.frames.len(), 1);
		assert_eq!(anim.frames[0].buffer().get_pixel(0, 0).0[3], 255);
	}
	#[test]
	fn png_and_jng_mixed_frames() {
		let jpeg = jpeg_bytes(&image::RgbImage::from_pixel(4, 4, image::Rgb([0, 255, 0])));
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		v.extend_from_slice(&embedded_png(4, 4, [255, 0, 0, 255]));
		v.extend_from_slice(&jng_object(4, 4, &jpeg, &[], &[]));
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let anim = decode(&v, 1_000_000, 1000, false).unwrap();
		assert_eq!(anim.frames.len(), 2);
		assert_eq!(anim.frames[0].buffer().get_pixel(0, 0).0, [255, 0, 0, 255]);
		let want = expected_jpeg_pixel(&jpeg, 0, 0);
		let got = anim.frames[1].buffer().get_pixel(0, 0).0;
		for i in 0..3 {
			assert!((got[i] as i32 - want[i] as i32).abs() <= 1, "{:?} vs {:?}", got, want);
		}
	}
	#[test]
	fn jng_defi_positions_frame() {
		let jpeg = jpeg_bytes(&image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0])));
		let mut defi = vec![0u8; 4];
		defi.extend_from_slice(&1i32.to_be_bytes());
		defi.extend_from_slice(&1i32.to_be_bytes());
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		v.extend_from_slice(&chunk(b"DEFI", &defi));
		v.extend_from_slice(&jng_object(2, 2, &jpeg, &[], &[]));
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let anim = decode(&v, 1_000_000, 1000, false).unwrap();
		let canvas = anim.frames[0].buffer();
		assert_eq!(canvas.get_pixel(0, 0).0, [0, 0, 0, 0]);
		let want = expected_jpeg_pixel(&jpeg, 0, 0);
		let got = canvas.get_pixel(1, 1).0;
		for i in 0..3 {
			assert!((got[i] as i32 - want[i] as i32).abs() <= 1, "{:?} vs {:?}", got, want);
		}
		assert_eq!(got[3], 255);
	}
	#[test]
	fn crc32_matches_zlib() {
		// zlib.crc32(b"IHDR" + ihdr(4x4 gray8)) = 2358952354
		let mut ihdr = Vec::new();
		ihdr.extend_from_slice(&4u32.to_be_bytes());
		ihdr.extend_from_slice(&4u32.to_be_bytes());
		ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
		let mut t = b"IHDR".to_vec();
		t.extend_from_slice(&ihdr);
		assert_eq!(crc32(&t), 2358952354);
	}
	#[test]
	fn jng_bad_compression_is_err() {
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		let mut body = vec![0u8; 16];
		body[..4].copy_from_slice(&4u32.to_be_bytes());
		body[4..8].copy_from_slice(&4u32.to_be_bytes());
		body[8] = 10; // color_type
		body[10] = 4; // compression != 8
		v.extend_from_slice(&chunk(b"JHDR", &body));
		let err = decode(&v, 1_000_000, 1000, false).err().unwrap();
		assert!(err.contains("JngCompression"), "{}", err);
	}
	#[test]
	fn jng_bad_color_type_is_err() {
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		let mut body = vec![0u8; 16];
		body[..4].copy_from_slice(&4u32.to_be_bytes());
		body[4..8].copy_from_slice(&4u32.to_be_bytes());
		body[8] = 6; // color_typeは 8,10,12,14 のみ
		v.extend_from_slice(&chunk(b"JHDR", &body));
		let err = decode(&v, 1_000_000, 1000, false).err().unwrap();
		assert!(err.contains("JngColorType"), "{}", err);
	}
	#[test]
	fn jng_frames_over_limit_is_err() {
		let jpeg = jpeg_bytes(&image::RgbImage::from_pixel(2, 2, image::Rgb([0, 0, 0])));
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(2, 2, 100));
		for _ in 0..3 {
			v.extend_from_slice(&jng_object(2, 2, &jpeg, &[], &[]));
		}
		v.extend_from_slice(&chunk(b"MEND", &[]));
		let err = decode(&v, 1_000_000, 2, false).err().unwrap();
		assert!(err.contains("FramesLimit"), "{}", err);
	}
}
