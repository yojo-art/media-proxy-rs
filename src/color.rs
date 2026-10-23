use std::io::Cursor;

use image::{DynamicImage, ImageDecoder};
use lcms2::{ColorSpaceSignature, Flags, Intent, PixelFormat, Profile, Transform};

/// 入力画像に埋め込まれている ICC プロファイルを取り出す。
/// プロファイルが無い場合や、デコーダが対応していない場合は None を返す。
pub(crate) fn extract_raster_icc(src: &[u8], codec: image::ImageFormat) -> Option<Vec<u8>> {
	let reader = image::ImageReader::with_format(Cursor::new(src), codec);
	let mut decoder = reader.into_decoder().ok()?;
	decoder
		.icc_profile()
		.ok()
		.flatten()
		.filter(|profile| !profile.is_empty())
}

/// ICC プロファイルに従ってピクセルを sRGB に変換する。
/// プロファイルが壊れているなど変換できない場合は、渡された画像をそのまま返す。
pub(crate) fn convert_to_srgb(img: DynamicImage, icc: &[u8]) -> Result<DynamicImage, DynamicImage> {
	let input = match Profile::new_icc(icc) {
		Ok(input) => input,
		Err(_) => return Err(img),
	};
	let srgb = Profile::new_srgb();
	// EXIF 回転やアニメーションデコードによって、
	// Gray の画素が RGBA に展開されている場合もある。
	// 入力プロファイルの色空間に合わせて Gray 入力を選択する。
	let use_gray = input.color_space() == ColorSpaceSignature::GrayData;
	let in_format = if use_gray {
		PixelFormat::GRAYA_8
	} else {
		PixelFormat::RGBA_8
	};
	let transform = match Transform::new_flags(
		&input,
		in_format,
		&srgb,
		PixelFormat::RGBA_8,
		Intent::Perceptual,
		Flags::COPY_ALPHA,
	) {
		Ok(transform) => transform,
		Err(_) => return Err(img),
	};

	let (width, height) = (img.width(), img.height());
	let (src, input_bpp) = if use_gray {
		(img.into_luma_alpha8().into_raw(), 2)
	} else {
		(img.into_rgba8().into_raw(), 4)
	};
	let pixels = src.len() / input_bpp;
	let mut dst = vec![0u8; pixels * 4];
	transform.transform_pixels(&src, &mut dst);
	let rgba = image::RgbaImage::from_raw(width, height, dst)
		.expect("converted buffer size matches image dimensions");
	Ok(DynamicImage::ImageRgba8(rgba))
}

/// WebP の RIFF コンテナに ICCP チャンクを埋め込む。
/// 既存の ICCP は置き換える。コンテナとして解釈できない場合は None を返す。
pub(crate) fn embed_webp_icc(data: &[u8], icc: &[u8]) -> Option<Vec<u8>> {
	if icc.is_empty() || data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
		return None;
	}

	let mut chunks: Vec<([u8; 4], &[u8])> = Vec::new();
	let mut offset = 12usize;
	while offset + 8 <= data.len() {
		let id: [u8; 4] = data[offset..offset + 4].try_into().ok()?;
		let size = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().ok()?) as usize;
		let body_start = offset + 8;
		let body_end = body_start.checked_add(size)?;
		if body_end > data.len() {
			return None;
		}
		chunks.push((id, &data[body_start..body_end]));
		offset = body_end + (size & 1);
	}

	let chunks: Vec<([u8; 4], &[u8])> = chunks
		.into_iter()
		.filter(|(id, _)| id.as_slice() != b"ICCP")
		.collect();

	let mut body = Vec::new();
	if let Some(index) = chunks.iter().position(|(id, _)| id.as_slice() == b"VP8X") {
		let vp8x = chunks[index].1;
		if vp8x.len() < 10 {
			return None;
		}
		for (i, (id, data)) in chunks.iter().enumerate() {
			if i == index {
				let mut vp8x = vp8x.to_vec();
				vp8x[0] |= 0x20;
				write_chunk(&mut body, b"VP8X", &vp8x);
				write_chunk(&mut body, b"ICCP", icc);
			} else {
				write_chunk(&mut body, id, data);
			}
		}
	} else {
		let (width, height, has_alpha) = webp_dimensions(&chunks)?;
		let mut vp8x = Vec::with_capacity(10);
		vp8x.push(if has_alpha { 0x30 } else { 0x20 });
		vp8x.extend_from_slice(&[0, 0, 0]);
		vp8x.extend_from_slice(&width.saturating_sub(1).to_le_bytes()[..3]);
		vp8x.extend_from_slice(&height.saturating_sub(1).to_le_bytes()[..3]);
		write_chunk(&mut body, b"VP8X", &vp8x);
		write_chunk(&mut body, b"ICCP", icc);
		for (id, data) in &chunks {
			write_chunk(&mut body, id, data);
		}
	}

	let riff_size = body.len().checked_add(4)?;
	if riff_size > u32::MAX as usize {
		return None;
	}
	let mut out = Vec::with_capacity(body.len() + 8);
	out.extend_from_slice(b"RIFF");
	out.extend_from_slice(&(riff_size as u32).to_le_bytes());
	out.extend_from_slice(b"WEBP");
	out.extend_from_slice(&body);
	Some(out)
}

fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
	out.extend_from_slice(id);
	out.extend_from_slice(&(data.len() as u32).to_le_bytes());
	out.extend_from_slice(data);
	if data.len() & 1 == 1 {
		out.push(0);
	}
}

fn webp_dimensions(chunks: &[([u8; 4], &[u8])]) -> Option<(u32, u32, bool)> {
	for (id, data) in chunks {
		if id.as_slice() == b"VP8L" {
			if data.len() < 5 || data[0] != 0x2f {
				return None;
			}
			let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
			let width = (bits & 0x3fff) + 1;
			let height = ((bits >> 14) & 0x3fff) + 1;
			let has_alpha = (bits >> 28) & 1 == 1;
			return Some((width, height, has_alpha));
		}
		if id.as_slice() == b"VP8 " {
			if data.len() < 10 || &data[3..6] != [0x9d, 0x01, 0x2a] {
				return None;
			}
			let width = u16::from_le_bytes([data[6], data[7]]) as u32 & 0x3fff;
			let height = u16::from_le_bytes([data[8], data[9]]) as u32 & 0x3fff;
			return Some((width, height, false));
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	fn riff(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
		let mut body = Vec::new();
		for (id, data) in chunks {
			write_chunk(&mut body, id, data);
		}
		let mut out = Vec::new();
		out.extend_from_slice(b"RIFF");
		out.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
		out.extend_from_slice(b"WEBP");
		out.extend_from_slice(&body);
		out
	}

	fn parse_chunks(data: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
		let mut chunks = Vec::new();
		let mut offset = 12;
		while offset + 8 <= data.len() {
			let id: [u8; 4] = data[offset..offset + 4].try_into().unwrap();
			let size =
				u32::from_le_bytes(data[offset + 4..offset + 8].try_into().unwrap()) as usize;
			chunks.push((id, data[offset + 8..offset + 8 + size].to_vec()));
			offset += 8 + size + (size & 1);
		}
		chunks
	}

	#[test]
	fn embeds_iccp_into_simple_vp8l() {
		let mut vp8l = vec![0x2f];
		let bits = 7u32 | (5u32 << 14) | (1 << 28);
		vp8l.extend_from_slice(&bits.to_le_bytes());
		let data = riff(&[(b"VP8L", &vp8l)]);
		let out = embed_webp_icc(&data, b"profile").unwrap();
		assert_eq!(&out[0..4], b"RIFF");
		assert_eq!(
			u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize,
			out.len() - 8
		);
		let chunks = parse_chunks(&out);
		assert_eq!(chunks[0].0, *b"VP8X");
		assert_eq!(chunks[0].1[0], 0x30);
		assert_eq!(&chunks[0].1[4..7], &7u32.to_le_bytes()[..3]);
		assert_eq!(&chunks[0].1[7..10], &5u32.to_le_bytes()[..3]);
		assert_eq!(chunks[1].0, *b"ICCP");
		assert_eq!(chunks[1].1, b"profile");
		assert_eq!(chunks[2].0, *b"VP8L");
		assert_eq!(chunks[2].1, vp8l);
	}

	#[test]
	fn replaces_existing_iccp_and_keeps_vp8x_first() {
		let vp8x = [0x10, 0, 0, 0, 7, 0, 0, 5, 0, 0];
		let anim = [0u8; 6];
		let vp8l = [0x2f, 0, 0, 0, 0];
		let data = riff(&[
			(b"VP8X", &vp8x),
			(b"ICCP", b"old"),
			(b"ANIM", &anim),
			(b"VP8L", &vp8l),
		]);
		let out = embed_webp_icc(&data, b"new-profile").unwrap();
		let chunks = parse_chunks(&out);
		assert_eq!(chunks[0].0, *b"VP8X");
		assert_ne!(chunks[0].1[0] & 0x20, 0);
		assert_eq!(chunks[1].0, *b"ICCP");
		assert_eq!(chunks[1].1, b"new-profile");
		assert_eq!(chunks[2].0, *b"ANIM");
		assert_eq!(chunks[3].0, *b"VP8L");
		assert_eq!(chunks.iter().filter(|(id, _)| *id == *b"ICCP").count(), 1);
	}
}
