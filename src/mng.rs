//! MNG(Multiple-image Network Graphics)のデコード
//! MNG-LC相当: 埋め込みPNG(IHDR..IEND)をフレームとして抽出しキャンバスへ合成

use crate::img::dimensions_allowed_for;

pub(crate) const SIGNATURE: [u8; 8] = [0x8a, 0x4d, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

pub(crate) struct MngAnimation {
	pub frames: Vec<image::Frame>,
	pub loop_count: u32,
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
			b"MHDR" if len >= 28 && png_start.is_none() => {
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
			b"FRAM" if len >= 1 && png_start.is_none() => {
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
				let Some(start) = png_start.take() else {
					break;
				};
				let canvas = canvas.as_mut().ok_or("IhdrBeforeMhdr")?;
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
				let img = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
					.map_err(|e| format!("EmbeddedPng {:?}", e))?;
				// モード3/4はフレーム毎に背景復元(透明で初期化)
				if framing_mode == 3 || framing_mode == 4 {
					for p in canvas.pixels_mut() {
						*p = image::Rgba([0, 0, 0, 0]);
					}
				}
				image::imageops::overlay(canvas, &img.into_rgba8(), loc_x, loc_y);
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
			// JNG(JPEG系サブストリーム)は未対応
			b"JHDR" => return Err("JngUnsupported".to_owned()),
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
	fn jng_is_err() {
		let mut v = SIGNATURE.to_vec();
		v.extend_from_slice(&mhdr(4, 4, 100));
		v.extend_from_slice(&chunk(b"JHDR", &[0u8; 16]));
		let err = decode(&v, 1_000_000, 1000, false).err().unwrap();
		assert!(err.contains("JngUnsupported"), "{}", err);
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
}
