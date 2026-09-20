use std::sync::Arc;

use image::{DynamicImage, ImageBuffer};
use resvg::usvg;

/// `href` resolution policy: data URIs only. The default `resolve_string`
/// treats the href as a local file path and reads it with `std::fs::read`,
/// so an attacker SVG could exfiltrate local files or read `/dev/zero`
/// forever (H-01). Data URIs are additionally dimension-checked before resvg
/// decodes them, because the canvas gate below does not cover embedded
/// rasters (M-01).
fn image_href_resolver(max_decode_pixels: u64) -> usvg::ImageHrefResolver<'static> {
	let resolve_data = usvg::ImageHrefResolver::default_data_resolver();
	usvg::ImageHrefResolver {
		resolve_data: Box::new(move |mime, data, opts| {
			// Only the declared dimensions are read here; unprobeable
			// payloads fall through and are rejected by resvg if invalid.
			if let Some((w, h)) = crate::img::probe_dimensions(&data[..]) {
				if !crate::img::dimensions_allowed_for(max_decode_pixels, w as u64, h as u64) {
					return None;
				}
			}
			resolve_data(mime, data, opts)
		}),
		resolve_string: Box::new(|_, _| None),
	}
}

/// Renders an SVG to RGBA. This is CPU-bound and works on attacker-controlled
/// bytes, so call it via [`render_svg_blocking`] from async code (H-01).
pub(crate) fn render_svg(
	src_bytes: &[u8],
	fontdb: Arc<usvg::fontdb::Database>,
	size_hint: (u32, u32),
	max_decode_pixels: u64,
) -> Result<DynamicImage, ()> {
	let mut options = usvg::Options {
		fontdb: fontdb.clone(),
		image_href_resolver: image_href_resolver(max_decode_pixels),
		..Default::default()
	};
	for f in fontdb.faces() {
		if let Some((name, _)) = f.families.get(0) {
			//デフォルトフォントに存在する事が確実なフォントを使う
			options.font_family = name.to_owned();
			break;
		}
	}
	let tree = usvg::Tree::from_data(src_bytes, &options);
	let tree = match tree {
		Ok(t) => t,
		Err(_) => return Err(()),
	};
	let size = size(&tree);
	let hint = size_hint;

	let (width, height, scale) = if size.width() > hint.0 as f32 || size.height() > hint.1 as f32 {
		let scale = f32::min(hint.0 as f32 / size.width(), hint.1 as f32 / size.height());
		let width = std::cmp::max((size.width() * scale).round() as u32, 1);
		let height = std::cmp::max((size.height() * scale).round() as u32, 1);
		(width, height, scale)
	} else {
		(size.width() as u32, size.height() as u32, 1f32)
	};
	let tf = usvg::Transform::from_scale(scale, scale);
	// Pixel-budget gate before allocation (finding #4).
	if (width as u64)
		.checked_mul(height as u64)
		.map_or(true, |pixels| pixels > max_decode_pixels)
	{
		return Err(());
	}
	// u32 arithmetic can overflow on crafted SVG sizes; never panic (finding #3).
	// A pixel-budget gate is applied separately (finding #4).
	let len = (width as u64)
		.checked_mul(height as u64)
		.and_then(|n| n.checked_mul(4));
	let len = len.and_then(|n| usize::try_from(n).ok());
	let Some(len) = len else {
		return Err(());
	};
	let mut rgba = vec![0; len];
	let Some(mut pxmap) = resvg::tiny_skia::PixmapMut::from_bytes(&mut rgba, width, height) else {
		return Err(());
	};
	resvg::render(&tree, tf, &mut pxmap);
	match ImageBuffer::from_vec(width, height, rgba) {
		Some(img) => Ok(DynamicImage::ImageRgba8(img)),
		None => Err(()),
	}
}
/// Runs [`render_svg`] on the blocking pool with a deadline (H-01). On
/// timeout the caller stops waiting and releases its semaphore permit; file
/// hrefs are disabled, so the remaining work is bounded by the SVG itself.
///
/// Caveat (code-review finding): `tokio::time::timeout` only stops the
/// *caller* from awaiting -- a blocking-pool closure that has already started
/// running cannot be preempted, so a render already in flight when the
/// deadline fires keeps consuming CPU/memory on its own thread until it
/// finishes. `abort()` is called anyway because it is not a no-op: for a
/// render that is still *queued* (not yet picked up by a worker thread, e.g.
/// because the blocking pool is under pressure from many concurrent slow
/// renders), it prevents that queued closure from ever starting at all,
/// which is exactly the pile-up scenario a deadline is meant to cap.
pub(crate) async fn render_svg_blocking(
	src_bytes: Vec<u8>,
	fontdb: Arc<usvg::fontdb::Database>,
	size_hint: (u32, u32),
	max_decode_pixels: u64,
	timeout_ms: u64,
) -> Result<DynamicImage, ()> {
	let task = tokio::task::spawn_blocking(move || {
		render_svg(&src_bytes, fontdb, size_hint, max_decode_pixels)
	});
	let abort_handle = task.abort_handle();
	match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms.max(1)), task).await {
		Ok(Ok(result)) => result,
		Ok(Err(_join_error)) => Err(()),
		Err(_elapsed) => {
			abort_handle.abort();
			Err(())
		}
	}
}
fn size(tree: &usvg::Tree) -> usvg::Size {
	let bb = tree.root().bounding_box();
	if bb.width() > tree.size().width() || bb.height() > tree.size().height() {
		if let Some(size) = usvg::Size::from_wh(bb.width(), bb.height()) {
			return size;
		}
	}
	tree.size()
}
