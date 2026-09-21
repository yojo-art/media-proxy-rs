//! AVIFシーケンス(avisブランド)のデコード
//! サンプル列を単一のdav1dデコーダへ順次送信(フレーム間予測対応)
//! 出力ピクチャはRGBAへ変換しアニメとして展開

use crate::img::dimensions_allowed_for;

pub(crate) fn is_avif_sequence(head: &[u8]) -> bool {
	head.len() >= 12 && &head[4..8] == b"ftyp" && &head[8..12] == b"avis"
}

pub(crate) struct AvifSequenceFrame {
	pub image: image::RgbaImage,
	pub duration_ms: u64,
}

pub(crate) struct AvifSequence {
	pub frames: Vec<AvifSequenceFrame>,
}

/// シーケンストラックを持たない場合はOk(None)を返し静止画経路に任せる
pub(crate) fn decode(
	data: &[u8],
	max_decode_pixels: u64,
	frames_limit: u64,
	first_frame_only: bool,
) -> Result<Option<AvifSequence>, String> {
	let mut cursor = std::io::Cursor::new(data);
	let ctx = mp4parse::read_avif(&mut cursor, mp4parse::ParseStrictness::Normal)
		.map_err(|e| format!("Parse {:?}", e))?;
	let Some(sequence) = &ctx.sequence else {
		return Ok(None);
	};
	let Some(color_track) = sequence
		.tracks
		.iter()
		.find(|t| t.track_type == mp4parse::TrackType::Picture)
	else {
		return Ok(None);
	};
	let Some((width, height)) = video_size(color_track) else {
		return Ok(None);
	};
	// デコード前のヘッダのみによるゲート(finding #4)
	if !dimensions_allowed_for(max_decode_pixels, width as u64, height as u64) {
		return Err(format!("DecodeDimensions {}x{} over limit", width, height));
	}
	let color_samples = mp4parse::unstable::create_sample_table(color_track, 0.into())
		.ok_or_else(|| "NoSampleTable".to_owned())?;
	let sample_count = if first_frame_only {
		1.min(color_samples.len())
	} else {
		color_samples.len()
	};
	if sample_count as u64 > frames_limit {
		return Err(format!("FramesLimit {}>{}", sample_count, frames_limit));
	}
	let canvas_pixels = (width as u64).saturating_mul(height as u64);
	let total = canvas_pixels.saturating_mul(sample_count as u64);
	if total > max_decode_pixels {
		return Err(format!("DecodePixels {}>{}", total, max_decode_pixels));
	}
	let frame_size_limit = max_decode_pixels.min(u32::MAX as u64) as u32;
	let color_pictures = decode_track(data, &color_samples, sample_count, frame_size_limit)?;
	// アルファは補助映像トラック, タイムスタンプ(サンプル番号)で対応付け
	let alpha_track = sequence
		.tracks
		.iter()
		.find(|t| t.track_type == mp4parse::TrackType::AuxiliaryVideo);
	let mut alpha_by_timestamp = std::collections::HashMap::new();
	if let Some(track) = alpha_track {
		let samples = mp4parse::unstable::create_sample_table(track, 0.into())
			.ok_or_else(|| "NoAlphaSampleTable".to_owned())?;
		let count = sample_count.min(samples.len());
		for picture in decode_track(data, &samples, count, frame_size_limit)? {
			if let Some(timestamp) = picture.timestamp() {
				alpha_by_timestamp.insert(timestamp, picture);
			}
		}
	}
	let timescale = color_track.timescale.map(|s| s.0).unwrap_or(0).max(1);
	let mut frames = Vec::with_capacity(color_pictures.len());
	for picture in &color_pictures {
		if !dimensions_allowed_for(
			max_decode_pixels,
			picture.width() as u64,
			picture.height() as u64,
		) {
			return Err(format!(
				"DecodeDimensions {}x{} over limit",
				picture.width(),
				picture.height()
			));
		}
		let mut rgba =
			picture_to_rgba(picture).ok_or_else(|| "UnsupportedPixelFormat".to_owned())?;
		if let Some(alpha) = picture.timestamp().and_then(|t| alpha_by_timestamp.get(&t)) {
			apply_alpha(&mut rgba, alpha);
		}
		let duration_ticks = picture
			.timestamp()
			.and_then(|t| usize::try_from(t).ok())
			.and_then(|i| color_samples.get(i))
			.map(|s| (s.end_composition.0 - s.start_composition.0).max(0) as u64)
			.unwrap_or(0);
		let duration_ms = duration_ticks.saturating_mul(1000) / timescale;
		frames.push(AvifSequenceFrame {
			image: rgba,
			duration_ms,
		});
	}
	if frames.is_empty() {
		return Err("NoAvailableFrames".to_owned());
	}
	Ok(Some(AvifSequence { frames }))
}

fn video_size(track: &mp4parse::Track) -> Option<(u16, u16)> {
	for entry in track.stsd.as_ref()?.descriptions.iter() {
		if let mp4parse::SampleEntry::Video(video) = entry {
			if let mp4parse::VideoCodecSpecific::AV1Config(_) = &video.codec_specific {
				return Some((video.width, video.height));
			}
		}
	}
	None
}

fn sample_bytes<'a>(data: &'a [u8], indice: &mp4parse::unstable::Indice) -> Option<&'a [u8]> {
	let start = usize::try_from(indice.start_offset.0).ok()?;
	let end = usize::try_from(indice.end_offset.0).ok()?;
	if start > end || end > data.len() {
		return None;
	}
	Some(&data[start..end])
}

/// タイムスタンプにサンプル番号を載せて順次デコード
fn decode_track(
	data: &[u8],
	samples: &mp4parse::TryVec<mp4parse::unstable::Indice>,
	sample_count: usize,
	frame_size_limit: u32,
) -> Result<Vec<dav1d::Picture>, String> {
	let mut settings = dav1d::Settings::new();
	settings.set_n_threads(1);
	settings.set_max_frame_delay(1);
	settings.set_frame_size_limit(frame_size_limit);
	let mut decoder =
		dav1d::Decoder::with_settings(&settings).map_err(|e| format!("Dav1dInit {:?}", e))?;
	let mut pictures = Vec::new();
	for (index, indice) in samples.iter().take(sample_count).enumerate() {
		let sample = sample_bytes(data, indice)
			.ok_or_else(|| "SampleOutOfRange".to_owned())?
			.to_vec();
		let mut send = decoder.send_data(sample, None, Some(index as i64), None);
		loop {
			match send {
				Ok(()) => break,
				Err(dav1d::Error::Again) => {
					drain_pictures(&mut decoder, &mut pictures, sample_count)?;
					send = decoder.send_pending_data();
				}
				Err(e) => return Err(format!("Dav1dSend {:?}", e)),
			}
		}
		drain_pictures(&mut decoder, &mut pictures, sample_count)?;
		if pictures.len() >= sample_count {
			break;
		}
	}
	drain_pictures(&mut decoder, &mut pictures, sample_count)?;
	Ok(pictures)
}

fn drain_pictures(
	decoder: &mut dav1d::Decoder,
	out: &mut Vec<dav1d::Picture>,
	sample_count: usize,
) -> Result<(), String> {
	loop {
		if out.len() >= sample_count {
			return Ok(());
		}
		match decoder.get_picture() {
			Ok(picture) => out.push(picture),
			Err(dav1d::Error::Again) => return Ok(()),
			Err(e) => return Err(format!("Dav1dGet {:?}", e)),
		}
	}
}

struct PlaneRef<'a> {
	data: &'a [u8],
	stride: usize,
}

impl PlaneRef<'_> {
	fn sample(&self, x: usize, y: usize, depth: u32) -> Option<f32> {
		if depth == 8 {
			self.data.get(y * self.stride + x).map(|v| *v as f32)
		} else {
			let offset = y * self.stride + x * 2;
			let low = *self.data.get(offset)?;
			let high = *self.data.get(offset + 1)?;
			Some(u16::from_ne_bytes([low, high]) as f32)
		}
	}
}

#[derive(Clone, Copy, PartialEq)]
enum Matrix {
	Identity,
	Coefficients { kr: f32, kb: f32 },
}

fn matrix_for(mc: dav1d::pixel::MatrixCoefficients) -> Matrix {
	use dav1d::pixel::MatrixCoefficients;
	match mc {
		MatrixCoefficients::Identity => Matrix::Identity,
		MatrixCoefficients::BT709 => Matrix::Coefficients {
			kr: 0.2126,
			kb: 0.0722,
		},
		MatrixCoefficients::BT470M => Matrix::Coefficients { kr: 0.30, kb: 0.11 },
		MatrixCoefficients::ST240M => Matrix::Coefficients {
			kr: 0.212,
			kb: 0.087,
		},
		MatrixCoefficients::BT2020NonConstantLuminance
		| MatrixCoefficients::BT2020ConstantLuminance => Matrix::Coefficients {
			kr: 0.2627,
			kb: 0.0593,
		},
		// BT470BG/ST170M/Unspecifiedその他はBT.601相当として扱う
		_ => Matrix::Coefficients {
			kr: 0.299,
			kb: 0.114,
		},
	}
}

/// 範囲正規化: 輝度を0..1へ
fn normalize_luma(value: f32, depth: u32, full_range: bool) -> f32 {
	let scale = (1u32 << (depth - 8)) as f32;
	if full_range {
		let max = ((1u32 << depth) - 1) as f32;
		value / max
	} else {
		(value - 16.0 * scale) / (219.0 * scale)
	}
}

/// 範囲正規化: 色差を-0.5..0.5へ
fn normalize_chroma(value: f32, depth: u32, full_range: bool) -> f32 {
	let scale = (1u32 << (depth - 8)) as f32;
	let center = (1u32 << (depth - 1)) as f32;
	if full_range {
		let max = ((1u32 << depth) - 1) as f32;
		(value - center) / max
	} else {
		(value - center) / (224.0 * scale)
	}
}

fn to_u8(value: f32) -> u8 {
	(value * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// YUVプレーンをRGBA(アルファ255)へ変換
#[allow(clippy::too_many_arguments)]
fn yuv_to_rgba(
	width: u32,
	height: u32,
	depth: u32,
	subsampling: Option<(u32, u32)>,
	full_range: bool,
	matrix: Matrix,
	y: PlaneRef,
	u: Option<PlaneRef>,
	v: Option<PlaneRef>,
) -> Option<image::RgbaImage> {
	let mut out = image::RgbaImage::new(width, height);
	for py in 0..height {
		for px in 0..width {
			let luma = y.sample(px as usize, py as usize, depth)?;
			let (r, g, b) = match (&u, &v) {
				(Some(u), Some(v)) => {
					let (ssx, ssy) = subsampling.unwrap_or((0, 0));
					let cx = (px >> ssx) as usize;
					let cy = (py >> ssy) as usize;
					let cb = u.sample(cx, cy, depth)?;
					let cr = v.sample(cx, cy, depth)?;
					match matrix {
						// Identity: G=Y, B=Cb, R=Cr
						Matrix::Identity => (
							normalize_luma(cr, depth, full_range),
							normalize_luma(luma, depth, full_range),
							normalize_luma(cb, depth, full_range),
						),
						Matrix::Coefficients { kr, kb } => {
							let kg = 1.0 - kr - kb;
							let yn = normalize_luma(luma, depth, full_range);
							let cbn = normalize_chroma(cb, depth, full_range);
							let crn = normalize_chroma(cr, depth, full_range);
							(
								yn + 2.0 * (1.0 - kr) * crn,
								yn - 2.0 * (1.0 - kb) * kb / kg * cbn
									- 2.0 * (1.0 - kr) * kr / kg * crn,
								yn + 2.0 * (1.0 - kb) * cbn,
							)
						}
					}
				}
				_ => {
					let yn = normalize_luma(luma, depth, full_range);
					(yn, yn, yn)
				}
			};
			out.put_pixel(px, py, image::Rgba([to_u8(r), to_u8(g), to_u8(b), 255]));
		}
	}
	Some(out)
}

fn picture_to_rgba(picture: &dav1d::Picture) -> Option<image::RgbaImage> {
	let depth = picture.bit_depth() as u32;
	if !(8..=12).contains(&depth) {
		return None;
	}
	let full_range = picture.color_range() == dav1d::pixel::YUVRange::Full;
	let matrix = matrix_for(picture.matrix_coefficients());
	let y_plane = picture.plane(dav1d::PlanarImageComponent::Y);
	let y = PlaneRef {
		data: &y_plane,
		stride: picture.stride(dav1d::PlanarImageComponent::Y) as usize,
	};
	let (subsampling, has_chroma) = match picture.pixel_layout() {
		dav1d::PixelLayout::I400 => (None, false),
		dav1d::PixelLayout::I420 => (Some((1, 1)), true),
		dav1d::PixelLayout::I422 => (Some((1, 0)), true),
		dav1d::PixelLayout::I444 => (Some((0, 0)), true),
	};
	let (u_plane, v_plane) = if has_chroma {
		(
			Some(picture.plane(dav1d::PlanarImageComponent::U)),
			Some(picture.plane(dav1d::PlanarImageComponent::V)),
		)
	} else {
		(None, None)
	};
	let u = u_plane.as_ref().map(|plane| PlaneRef {
		data: plane,
		stride: picture.stride(dav1d::PlanarImageComponent::U) as usize,
	});
	let v = v_plane.as_ref().map(|plane| PlaneRef {
		data: plane,
		stride: picture.stride(dav1d::PlanarImageComponent::V) as usize,
	});
	yuv_to_rgba(
		picture.width(),
		picture.height(),
		depth,
		subsampling,
		full_range,
		matrix,
		y,
		u,
		v,
	)
}

/// アルファトラックの輝度プレーンをAチャネルへ合成
fn apply_alpha(rgba: &mut image::RgbaImage, alpha: &dav1d::Picture) {
	if rgba.width() != alpha.width() || rgba.height() != alpha.height() {
		return;
	}
	let depth = alpha.bit_depth() as u32;
	if !(8..=12).contains(&depth) {
		return;
	}
	let full_range = alpha.color_range() == dav1d::pixel::YUVRange::Full;
	let plane = alpha.plane(dav1d::PlanarImageComponent::Y);
	let y = PlaneRef {
		data: &plane,
		stride: alpha.stride(dav1d::PlanarImageComponent::Y) as usize,
	};
	for py in 0..rgba.height() {
		for px in 0..rgba.width() {
			let Some(value) = y.sample(px as usize, py as usize, depth) else {
				continue;
			};
			rgba.get_pixel_mut(px, py).0[3] = to_u8(normalize_luma(value, depth, full_range));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn non_avis_data_is_not_detected() {
		assert!(!is_avif_sequence(b"\x00\x00\x00\x1cftypavif____"));
		assert!(!is_avif_sequence(b"short"));
	}
	#[test]
	fn avis_brand_is_detected() {
		assert!(is_avif_sequence(b"\x00\x00\x00\x1cftypavis____"));
	}
	#[test]
	fn yuv_identity_full_range_is_gbr() {
		let y = [100u8];
		let u = [200u8];
		let v = [50u8];
		let img = yuv_to_rgba(
			1,
			1,
			8,
			Some((0, 0)),
			true,
			Matrix::Identity,
			PlaneRef {
				data: &y,
				stride: 1,
			},
			Some(PlaneRef {
				data: &u,
				stride: 1,
			}),
			Some(PlaneRef {
				data: &v,
				stride: 1,
			}),
		)
		.unwrap();
		assert_eq!(img.get_pixel(0, 0).0, [50, 100, 200, 255]);
	}
	#[test]
	fn yuv_bt601_limited_white_and_black() {
		// 白(Y=235)と黒(Y=16)、色差は中央値128
		let y = [235u8, 16];
		let u = [128u8, 128];
		let v = [128u8, 128];
		let matrix = matrix_for(dav1d::pixel::MatrixCoefficients::BT470BG);
		let img = yuv_to_rgba(
			2,
			1,
			8,
			Some((0, 0)),
			false,
			matrix,
			PlaneRef {
				data: &y,
				stride: 2,
			},
			Some(PlaneRef {
				data: &u,
				stride: 2,
			}),
			Some(PlaneRef {
				data: &v,
				stride: 2,
			}),
		)
		.unwrap();
		assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
		assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0, 255]);
	}
	#[test]
	fn mono_layout_is_grayscale() {
		let y = [128u8];
		let img = yuv_to_rgba(
			1,
			1,
			8,
			None,
			true,
			matrix_for(dav1d::pixel::MatrixCoefficients::Unspecified),
			PlaneRef {
				data: &y,
				stride: 1,
			},
			None,
			None,
		)
		.unwrap();
		assert_eq!(img.get_pixel(0, 0).0, [128, 128, 128, 255]);
	}
	#[test]
	fn ten_bit_limited_white_is_white() {
		// 10bit白(Y=940)、色差中央値512、ネイティブエンディアン16bit格納
		let y: Vec<u8> = 940u16.to_ne_bytes().to_vec();
		let u: Vec<u8> = 512u16.to_ne_bytes().to_vec();
		let v: Vec<u8> = 512u16.to_ne_bytes().to_vec();
		let matrix = matrix_for(dav1d::pixel::MatrixCoefficients::BT709);
		let img = yuv_to_rgba(
			1,
			1,
			10,
			Some((0, 0)),
			false,
			matrix,
			PlaneRef {
				data: &y,
				stride: 2,
			},
			Some(PlaneRef {
				data: &u,
				stride: 2,
			}),
			Some(PlaneRef {
				data: &v,
				stride: 2,
			}),
		)
		.unwrap();
		assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
	}
}
