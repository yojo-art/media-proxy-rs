use std::sync::{Arc, OnceLock};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "avif-decoder")]
mod avif_seq;
#[cfg(feature = "server")]
pub mod browsersafe;
mod image_test;
pub(crate) mod img;
mod mng;
mod svg;

/// 画像処理の設定。
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum FilterType {
	Nearest,
	Triangle,
	CatmullRom,
	Gaussian,
	Lanczos3,
}

impl From<FilterType> for image::imageops::FilterType {
	fn from(value: FilterType) -> Self {
		match value {
			FilterType::Nearest => image::imageops::Nearest,
			FilterType::Triangle => image::imageops::Triangle,
			FilterType::CatmullRom => image::imageops::CatmullRom,
			FilterType::Gaussian => image::imageops::Gaussian,
			FilterType::Lanczos3 => image::imageops::Lanczos3,
		}
	}
}

impl From<FilterType> for fast_image_resize::FilterType {
	fn from(value: FilterType) -> Self {
		match value {
			FilterType::Nearest => fast_image_resize::FilterType::Box,
			FilterType::Triangle => fast_image_resize::FilterType::Bilinear,
			FilterType::CatmullRom => fast_image_resize::FilterType::CatmullRom,
			FilterType::Gaussian => fast_image_resize::FilterType::Mitchell,
			FilterType::Lanczos3 => fast_image_resize::FilterType::Lanczos3,
		}
	}
}

#[derive(Clone, Debug)]
pub struct ProcessorConfig {
	pub max_pixels: u32,
	pub max_decode_pixels: u64,
	pub filter_type: FilterType,
	pub webp_quality: f32,
	pub encode_avif: bool,
	pub load_system_fonts: bool,
	/// 追加で読み込むフォントのディレクトリ。存在しないものは読み飛ばす。
	pub font_dirs: Vec<std::path::PathBuf>,
	/// SVG の font-family が見つからないときと、総称フォント名 (sans-serif など)
	/// に使うフォント。None か DB に無い名前なら、埋め込みの Aileron を使う。
	pub default_font_family: Option<String>,
}

impl Default for ProcessorConfig {
	fn default() -> Self {
		Self {
			max_pixels: 2048,
			max_decode_pixels: 256 * 1024 * 1024 / 4,
			filter_type: FilterType::Triangle,
			webp_quality: 75.0,
			encode_avif: false,
			load_system_fonts: true,
			font_dirs: Vec::new(),
			default_font_family: None,
		}
	}
}

#[derive(Clone, Debug, Default)]
pub struct ProcessOptions {
	pub emoji: bool,
	pub avatar: bool,
	pub r#static: bool,
	pub preview: bool,
	pub badge: bool,
	pub content_type_hint: Option<String>,
	pub accept_avif: bool,
}

#[derive(Clone, Debug)]
pub struct Encoded {
	pub bytes: Vec<u8>,
	pub content_type: &'static str,
	pub ext: &'static str,
	/// 一部のフレームを落とすなど、結果が完全ではないときの理由。
	/// Some のときは長期キャッシュさせないこと。
	pub warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessErrorKind {
	Unsupported,
	DecodeLimit,
	Decode,
	Encode,
	BadgeSkipped,
}

impl ProcessErrorKind {
	pub fn as_str(&self) -> &'static str {
		match self {
			ProcessErrorKind::Unsupported => "Unsupported",
			ProcessErrorKind::DecodeLimit => "DecodeLimit",
			ProcessErrorKind::Decode => "Decode",
			ProcessErrorKind::Encode => "Encode",
			ProcessErrorKind::BadgeSkipped => "BadgeSkipped",
		}
	}
}

#[derive(Debug)]
pub struct ProcessError {
	pub kind: ProcessErrorKind,
	pub message: String,
}

impl ProcessError {
	pub fn new(kind: ProcessErrorKind, message: impl Into<String>) -> Self {
		Self {
			kind,
			message: message.into(),
		}
	}
}

impl std::fmt::Display for ProcessError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}: {}", self.kind.as_str(), self.message)
	}
}

impl std::error::Error for ProcessError {}

#[derive(Clone, Debug)]
pub enum DetectedFormat {
	Svg,
	Raster(image::ImageFormat),
	Jxl,
	Jp2,
	Jxr,
	Mng,
	AvifSeq,
	Pdf,
	Unknown,
}

/// 先頭バイトと `Content-Type` ヒントから形式を判定する。
pub fn detect(head: &[u8], content_type_hint: Option<&str>) -> DetectedFormat {
	let media_type = content_type_hint
		.and_then(|s| s.split(';').next())
		.map(|s| s.trim().to_lowercase());

	if media_type.as_deref() == Some("image/svg+xml") {
		return DetectedFormat::Svg;
	}
	if std::str::from_utf8(head)
		.map(|s| s.trim().starts_with("<svg"))
		.unwrap_or(false)
	{
		return DetectedFormat::Svg;
	}

	if let Ok(codec) = image::guess_format(head) {
		return DetectedFormat::Raster(codec);
	}

	if head.starts_with(&[0xFF, 0x0A])
		|| head.starts_with(&[
			0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
		]) {
		return DetectedFormat::Jxl;
	}
	if head.starts_with(&[0xFF, 0x4F, 0xFF, 0x51])
		|| head.starts_with(&[
			0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
		]) {
		return DetectedFormat::Jp2;
	}
	if head.starts_with(&[0x49, 0x49, 0xBC]) {
		return DetectedFormat::Jxr;
	}
	if head.starts_with(&crate::mng::SIGNATURE) || head.starts_with(&crate::mng::JNG_SIGNATURE) {
		return DetectedFormat::Mng;
	}
	#[cfg(feature = "avif-decoder")]
	if crate::avif_seq::is_avif_sequence(head) {
		return DetectedFormat::AvifSeq;
	}

	if let Some(mt) = media_type.as_deref() {
		if mt == "image/x-targa" || mt == "image/x-tga" {
			return DetectedFormat::Raster(image::ImageFormat::Tga);
		}
		if mt == "application/pdf" {
			return DetectedFormat::Pdf;
		}
	}

	DetectedFormat::Unknown
}

pub(crate) struct Fonts {
	pub(crate) db: Arc<resvg::usvg::fontdb::Database>,
	pub(crate) default_family: String,
}

pub struct Processor {
	cfg: ProcessorConfig,
	fonts: OnceLock<Fonts>,
}

impl Processor {
	/// フォントはここでは読み込まない。最初に SVG を処理するとき
	/// (= blocking スレッドの上) で読み込む。
	pub fn new(cfg: ProcessorConfig) -> Self {
		Self {
			cfg,
			fonts: OnceLock::new(),
		}
	}

	/// 呼び出し側で作ったフォント DB を使う。総称フォント名の割り当ては呼び出し側で済ませておくこと。
	pub fn with_fontdb(cfg: ProcessorConfig, fontdb: Arc<resvg::usvg::fontdb::Database>) -> Self {
		let default_family = cfg
			.default_font_family
			.clone()
			.or_else(|| {
				fontdb
					.faces()
					.find_map(|f| f.families.first().map(|(name, _)| name.clone()))
			})
			.unwrap_or_default();
		let fonts = OnceLock::new();
		let _ = fonts.set(Fonts {
			db: fontdb,
			default_family,
		});
		Self { cfg, fonts }
	}

	/// フォント DB を今すぐ作る。起動時に済ませて、最初の SVG の処理を遅くしたくない場合に使う。
	/// フォントファイルを読むので、非同期ランタイムからは blocking スレッドで呼ぶこと。
	/// 実際に使われる既定のフォント名を返す。
	pub fn preload_fonts(&self) -> String {
		self.fonts().default_family.clone()
	}

	fn fonts(&self) -> &Fonts {
		self.fonts.get_or_init(|| build_fonts(&self.cfg))
	}

	/// 画像を同期で処理する。デコード・リサイズ・エンコードは CPU バウンドなため、
	/// 非同期ランタイム上で呼び出す場合は `tokio::task::spawn_blocking` などの
	/// ブロッキング用スレッドで実行すること。
	///
	/// # Errors
	/// 形式非対応・デコード/エンコード失敗・デコード上限超過などでエラーを返す。
	pub fn process(&self, src: &[u8], opts: &ProcessOptions) -> Result<Encoded, ProcessError> {
		match detect(src, opts.content_type_hint.as_deref()) {
			DetectedFormat::Svg => self.encode_svg(src, opts),
			DetectedFormat::Raster(codec) => crate::img::encode_raster(src, opts, &self.cfg, codec),
			DetectedFormat::Jxl => crate::img::encode_jxl(src, opts, &self.cfg),
			DetectedFormat::Jp2 => crate::img::encode_jp2(src, opts, &self.cfg),
			DetectedFormat::Jxr => crate::img::encode_jxr(src, opts, &self.cfg),
			DetectedFormat::Mng => crate::img::encode_mng(src, opts, &self.cfg),
			#[cfg(feature = "avif-decoder")]
			DetectedFormat::AvifSeq => crate::img::encode_avif_seq(src, opts, &self.cfg),
			#[cfg(not(feature = "avif-decoder"))]
			DetectedFormat::AvifSeq => Err(ProcessError::new(
				ProcessErrorKind::Unsupported,
				"avif-decoder feature disabled",
			)),
			DetectedFormat::Pdf => Err(ProcessError::new(
				ProcessErrorKind::Unsupported,
				"pdf not supported",
			)),
			DetectedFormat::Unknown => Err(ProcessError::new(
				ProcessErrorKind::Unsupported,
				"unknown format",
			)),
		}
	}

	fn encode_svg(&self, src: &[u8], opts: &ProcessOptions) -> Result<Encoded, ProcessError> {
		let size_hint = crate::img::image_size_hint(opts, &self.cfg);
		let fonts = self.fonts();
		let img = crate::svg::render_svg(
			src,
			fonts.db.clone(),
			&fonts.default_family,
			size_hint,
			self.cfg.max_decode_pixels,
		)
		.map_err(|()| ProcessError::new(ProcessErrorKind::Decode, "svg render failed"))?;
		crate::img::response_img(img, src, None, opts, &self.cfg)
	}
}

fn build_fonts(cfg: &ProcessorConfig) -> Fonts {
	use resvg::usvg::fontdb;
	let mut db = fontdb::Database::new();
	if cfg.load_system_fonts {
		db.load_system_fonts();
	}
	for dir in &cfg.font_dirs {
		if dir.is_dir() {
			db.load_fonts_dir(dir);
		}
	}
	let embedded = db.load_font_source(fontdb::Source::Binary(Arc::new(include_bytes!(
		"../asset/font/Aileron-Light.otf"
	))));
	let embedded_family = embedded
		.first()
		.and_then(|id| db.face(*id))
		.and_then(|face| face.families.first())
		.map(|(name, _)| name.clone())
		.unwrap_or_default();
	// DB の並び順 (= 読み込んだ順) に頼らず、既定のフォントを決める。
	let default_family = cfg
		.default_font_family
		.clone()
		.filter(|name| {
			db.faces()
				.any(|face| face.families.iter().any(|(n, _)| n == name))
		})
		.unwrap_or(embedded_family);
	// fontdb は総称フォント名を Arial / Times New Roman / Courier New に割り当てていて、
	// Linux ではほぼ見つからない。実際に入っているフォントに割り当て直す。
	db.set_sans_serif_family(default_family.clone());
	db.set_serif_family(default_family.clone());
	db.set_monospace_family(default_family.clone());
	Fonts {
		db: Arc::new(db),
		default_family,
	}
}

#[cfg(test)]
mod tests {
	use std::io::Cursor;

	use image::DynamicImage;

	use super::*;

	#[test]
	fn process_dummy_png_avatar() {
		let src = include_bytes!("../asset/dummy.png");
		let processor = Processor::new(ProcessorConfig::default());
		let opts = ProcessOptions {
			avatar: true,
			..Default::default()
		};
		let encoded = processor.process(src, &opts).unwrap();
		assert_eq!(encoded.content_type, "image/webp");
		assert_eq!(encoded.ext, ".webp");
		assert!(!encoded.bytes.is_empty());
	}

	#[test]
	fn process_badge_high_entropy() {
		let mut img = image::RgbaImage::new(100, 100);
		for (x, y, p) in img.enumerate_pixels_mut() {
			*p = image::Rgba([x as u8, y as u8, 128, 255]);
		}
		let mut src = vec![];
		DynamicImage::ImageRgba8(img)
			.write_to(&mut Cursor::new(&mut src), image::ImageFormat::Png)
			.unwrap();
		let processor = Processor::new(ProcessorConfig::default());
		let opts = ProcessOptions {
			badge: true,
			..Default::default()
		};
		let encoded = processor.process(&src, &opts).unwrap();
		assert_eq!(encoded.content_type, "image/png");
		assert_eq!(encoded.ext, ".png");
		assert!(!encoded.bytes.is_empty());
	}

	#[test]
	fn process_badge_low_entropy_is_skipped() {
		let img = image::RgbaImage::from_pixel(96, 96, image::Rgba([128, 128, 128, 255]));
		let mut src = vec![];
		DynamicImage::ImageRgba8(img)
			.write_to(&mut Cursor::new(&mut src), image::ImageFormat::Png)
			.unwrap();
		let processor = Processor::new(ProcessorConfig::default());
		let opts = ProcessOptions {
			badge: true,
			..Default::default()
		};
		let err = processor.process(&src, &opts).unwrap_err();
		assert_eq!(err.kind, ProcessErrorKind::BadgeSkipped);
	}

	#[test]
	fn process_badge_sparse_shape_is_not_skipped() {
		// 透明な背景に約 9% の白い四角形を置く。
		// entropy は約 0.47 となり、sharp と同じくスキップされない。
		let mut img = image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 0]));
		for x in 35..65 {
			for y in 35..65 {
				img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
			}
		}
		let mut src = vec![];
		DynamicImage::ImageRgba8(img)
			.write_to(&mut Cursor::new(&mut src), image::ImageFormat::Png)
			.unwrap();
		let processor = Processor::new(ProcessorConfig::default());
		let opts = ProcessOptions {
			badge: true,
			..Default::default()
		};
		let encoded = processor.process(&src, &opts).unwrap();
		assert_eq!(encoded.content_type, "image/png");
		assert_eq!(encoded.ext, ".png");
		assert!(!encoded.bytes.is_empty());
		// 透過情報が保持されていることも確認する
		let decoded = image::load_from_memory_with_format(&encoded.bytes, image::ImageFormat::Png)
			.unwrap()
			.into_luma_alpha8();
		let mut opaque = false;
		let mut transparent = false;
		for p in decoded.pixels() {
			if p.0[1] > 0 {
				opaque = true;
			} else {
				transparent = true;
			}
		}
		assert!(opaque, "badge should contain opaque pixels");
		assert!(transparent, "badge should contain transparent pixels");
	}

	#[test]
	fn detect_png() {
		let src = include_bytes!("../asset/dummy.png");
		assert!(matches!(
			detect(src, None),
			DetectedFormat::Raster(image::ImageFormat::Png)
		));
	}

	#[test]
	fn default_font_family_does_not_depend_on_db_order() {
		let cfg = ProcessorConfig {
			load_system_fonts: false,
			..Default::default()
		};
		let fonts = build_fonts(&cfg);
		assert!(!fonts.default_family.is_empty());
		assert!(fonts
			.db
			.faces()
			.any(|f| f.families.iter().any(|(n, _)| *n == fonts.default_family)));
	}

	#[test]
	fn svg_text_renders_without_system_fonts() {
		let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><text x="0" y="20" font-family="sans-serif">Ab</text></svg>"#;
		let processor = Processor::new(ProcessorConfig {
			load_system_fonts: false,
			..Default::default()
		});
		let encoded = processor.process(svg, &ProcessOptions::default()).unwrap();
		assert_eq!(encoded.content_type, "image/webp");
	}
}
