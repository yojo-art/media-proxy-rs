use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use media_proxy_rs::{
	Encoded, FilterType, ProcessError, ProcessOptions as LibProcessOptions, Processor,
	ProcessorConfig,
};
use napi::bindgen_prelude::{Buffer, Env, Error, PromiseRaw, Result};
use napi::{JsValue, Status};
use napi_derive::napi;

/// NodeJS 側のローダーが互換性を確認するための定数。
/// `MediaProcessorOptions` / `ProcessOptions` / `ProcessResult` / エラーコード / `preloadFonts` の戻り値を変えたら上げる。
#[napi]
pub const ABI_VERSION: u32 = 1;

/// `MediaProcessor` の構築オプション。
#[napi(object)]
#[derive(Default, Clone)]
pub struct MediaProcessorOptions {
	#[napi(js_name = "maxPixels")]
	pub max_pixels: Option<u32>,
	#[napi(js_name = "maxDecodePixels")]
	pub max_decode_pixels: Option<f64>,
	#[napi(js_name = "webpQuality")]
	pub webp_quality: Option<f64>,
	pub concurrency: Option<u32>,
	#[napi(js_name = "maxQueue")]
	pub max_queue: Option<u32>,
	#[napi(js_name = "timeoutMs")]
	pub timeout_ms: Option<u32>,
	#[napi(js_name = "loadSystemFonts")]
	pub load_system_fonts: Option<bool>,
	#[napi(js_name = "fontDirs")]
	pub font_dirs: Option<Vec<String>>,
	#[napi(js_name = "defaultFontFamily")]
	pub default_font_family: Option<String>,
}

/// `process` の入力オプション。
#[napi(object)]
#[derive(Default, Clone)]
pub struct ProcessOptions {
	pub emoji: Option<bool>,
	pub avatar: Option<bool>,
	#[napi(js_name = "static")]
	pub r#static: Option<bool>,
	pub preview: Option<bool>,
	pub badge: Option<bool>,
	#[napi(js_name = "contentType")]
	pub content_type: Option<String>,
}

/// `process` の結果。
#[napi(object)]
pub struct ProcessResult {
	pub data: Buffer,
	#[napi(js_name = "contentType")]
	pub content_type: String,
	pub ext: String,
}

struct Inner {
	processor: Arc<Processor>,
	semaphore: Arc<tokio::sync::Semaphore>,
	waiting: Arc<AtomicUsize>,
	max_queue: usize,
	timeout: Duration,
}

#[napi]
pub struct MediaProcessor {
	inner: Arc<Inner>,
}

fn invalid_arg(msg: impl ToString) -> Error {
	Error::new(Status::InvalidArg, msg.to_string())
}

#[napi]
impl MediaProcessor {
	/// 新しい `MediaProcessor` を構築する。フォントはここでは読み込まない
	/// (初めて SVG を処理するとき、または `preloadFonts()` で読み込む)。
	#[napi(constructor)]
	pub fn new(opts: Option<MediaProcessorOptions>) -> Result<Self> {
		let opts = opts.unwrap_or_default();

		let max_pixels = opts.max_pixels.unwrap_or(2048);
		if !(1..=16383).contains(&max_pixels) {
			return Err(invalid_arg("maxPixels must be in 1..=16383"));
		}
		let max_decode_pixels = match opts.max_decode_pixels {
			None => 256 * 1024 * 1024 / 4,
			Some(v) if v.is_finite() && v >= 1.0 && v <= u64::MAX as f64 => v as u64,
			Some(_) => {
				return Err(invalid_arg(
					"maxDecodePixels must be a positive finite number",
				))
			}
		};
		let webp_quality = match opts.webp_quality {
			None => 75.0,
			Some(q) if (0.0..=100.0).contains(&q) => q as f32,
			Some(_) => return Err(invalid_arg("webpQuality must be in 0..=100")),
		};
		let concurrency = opts.concurrency.unwrap_or(4).max(1) as usize;
		let max_queue = opts
			.max_queue
			.map(|v| v as usize)
			.unwrap_or(concurrency * 16);
		let timeout = Duration::from_millis(u64::from(opts.timeout_ms.unwrap_or(10_000).max(1)));

		let font_dirs: Vec<PathBuf> = opts
			.font_dirs
			.unwrap_or_default()
			.into_iter()
			.map(PathBuf::from)
			.collect();
		if let Some(dir) = font_dirs.iter().find(|d| !d.is_dir()) {
			return Err(invalid_arg(format!(
				"fontDirs: {} is not a directory",
				dir.display()
			)));
		}

		let cfg = ProcessorConfig {
			max_pixels,
			max_decode_pixels,
			filter_type: FilterType::Triangle,
			webp_quality,
			encode_avif: false,
			load_system_fonts: opts.load_system_fonts.unwrap_or(false),
			font_dirs,
			default_font_family: opts.default_font_family,
		};

		Ok(Self {
			inner: Arc::new(Inner {
				processor: Arc::new(Processor::new(cfg)),
				semaphore: Arc::new(tokio::sync::Semaphore::new(concurrency)),
				waiting: Arc::new(AtomicUsize::new(0)),
				max_queue,
				timeout,
			}),
		})
	}

	/// フォント DB を blocking スレッドで作っておく。起動時に呼べば、最初の SVG の処理が遅くならない。
	/// 実際に使われる既定のフォント名を返す。
	#[napi]
	pub async fn preload_fonts(&self) -> Result<String> {
		let processor = Arc::clone(&self.inner.processor);
		tokio::task::spawn_blocking(move || processor.preload_fonts())
			.await
			.map_err(|e| Error::new(Status::GenericFailure, e.to_string()))
	}

	/// 画像を処理し、エンコード済みデータを返す。
	///
	/// 同時実行数は `concurrency` で制限され、30 秒以上待ちになった場合は
	/// `code` が `"Overloaded"` の Error を投げる。
	/// 処理自体が `timeoutMs` を超えた場合は `code` が `"Timeout"` の Error を投げる。
	///
	/// 入力の `src` はコピーせず blocking スレッドへ渡すため、処理が終わるまで
	/// 書き換えてはならない。`Timeout` 等で Promise が先に reject された後も
	/// 裏側の処理が `src` を読んでいる場合があり、その間に書き換えると
	/// 未定義動作になる。`fs.readFile` の結果を使い捨てる使い方が安全。
	#[napi]
	pub fn process<'env>(
		&self,
		env: &'env Env,
		src: Buffer,
		opts: Option<ProcessOptions>,
	) -> Result<PromiseRaw<'env, ProcessResult>> {
		let opts = opts.unwrap_or_default();
		let lib_opts = LibProcessOptions {
			emoji: opts.emoji.unwrap_or(false),
			avatar: opts.avatar.unwrap_or(false),
			r#static: opts.r#static.unwrap_or(false),
			preview: opts.preview.unwrap_or(false),
			badge: opts.badge.unwrap_or(false),
			content_type_hint: opts.content_type,
			accept_avif: false,
		};

		let inner = self.inner.clone();
		// JS の Buffer を blocking スレッドから直接読むと、reject 後に JS 側が書き換えたときに
		// データ競合になる。デコードに比べればコピーのコストは小さいので、ここで Vec に移す。
		let src = src.to_vec();

		// Future の中では JS の値を作らない (Encoded / ProcessFailure はどちらも Send)。
		// resolve / reject する JS の値は、callback (JS のスレッド) で作る。
		env.spawn_future_with_callback(
			async move { Ok(run(inner, src, lib_opts).await) },
			|env, result| match result {
				Ok(encoded) => Ok(ProcessResult {
					data: encoded.bytes.into(),
					content_type: encoded.content_type.to_owned(),
					ext: encoded.ext.to_owned(),
				}),
				Err(failure) => Err(failure.into_error(env)),
			},
		)
	}
}

/// 待ち行列の上限・permit・spawn_blocking・タイムアウトの処理。
async fn run(
	inner: Arc<Inner>,
	src: Vec<u8>,
	lib_opts: LibProcessOptions,
) -> std::result::Result<Encoded, ProcessFailure> {
	// すぐに permit が取れたら待ち行列に入れない。
	let permit = match Arc::clone(&inner.semaphore).try_acquire_owned() {
		Ok(permit) => permit,
		Err(_) => {
			// permit が取れなかったら待ち行列に入る。future がドロップされても
			// `waiting` を減らすため、ガードを置く。
			let waiting = inner.waiting.fetch_add(1, Ordering::AcqRel) + 1;
			let _guard = DecrementOnDrop(Arc::clone(&inner.waiting));
			if waiting > inner.max_queue {
				return Err(ProcessFailure::Overloaded);
			}
			match tokio::time::timeout(
				Duration::from_secs(30),
				Arc::clone(&inner.semaphore).acquire_owned(),
			)
			.await
			{
				Ok(Ok(permit)) => permit,
				_ => return Err(ProcessFailure::Overloaded),
			}
		}
	};

	let processor = Arc::clone(&inner.processor);
	let timeout = inner.timeout;
	// permit は処理が本当に終わるまで持ち続ける。タイムアウトで JS に先に
	// エラーを返しても、blocking スレッドの処理は止まらないため。
	let task = tokio::task::spawn_blocking(move || {
		let _permit = permit;
		processor.process(&src, &lib_opts)
	});
	match tokio::time::timeout(timeout, task).await {
		Ok(Ok(result)) => result.map_err(ProcessFailure::Process),
		Ok(Err(e)) => Err(ProcessFailure::Task(e)),
		Err(_) => Err(ProcessFailure::Timeout),
	}
}

struct DecrementOnDrop(Arc<AtomicUsize>);

impl Drop for DecrementOnDrop {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::AcqRel);
	}
}

enum ProcessFailure {
	Process(ProcessError),
	Overloaded,
	Timeout,
	Task(tokio::task::JoinError),
}

impl ProcessFailure {
	fn code(&self) -> &'static str {
		match self {
			ProcessFailure::Process(e) => e.kind.as_str(),
			ProcessFailure::Overloaded => "Overloaded",
			ProcessFailure::Timeout => "Timeout",
			ProcessFailure::Task(_) => "Internal",
		}
	}

	fn message(&self) -> String {
		match self {
			ProcessFailure::Process(e) => e.message.clone(),
			ProcessFailure::Overloaded => "server overloaded".to_owned(),
			ProcessFailure::Timeout => "processing timeout".to_owned(),
			ProcessFailure::Task(e) => format!("processing task failed: {e}"),
		}
	}

	/// `code` 付きの JS の Error を作り、reject 用の `Error` に包む。
	/// reject の経路はこの値をそのまま渡すので、JS 側の `err.code` がこの文字列になる
	/// (`deferred_trace` feature が無効のとき。napi 3.13 の既定)。
	fn into_error(self, env: &Env) -> Error {
		let code = self.code();
		let message = self.message();
		let js_error = env
			.create_error(Error::new(Status::GenericFailure, message.clone()))
			.and_then(|mut obj| {
				// create_error は code に Status の文字列 ("GenericFailure") を入れるので上書きする
				obj.set("code", code)?;
				Ok(obj)
			});
		match js_error {
			Ok(obj) => Error::from_unknown_without_coercion(obj.to_unknown()),
			// JS の値を作れなかったときは、code 無しの Error で reject する
			Err(_) => Error::new(Status::GenericFailure, message),
		}
	}
}
