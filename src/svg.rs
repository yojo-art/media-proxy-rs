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
/// bytes, so call it from `spawn_blocking` from async code (H-01).
pub(crate) fn render_svg(
	src_bytes: &[u8],
	fontdb: Arc<usvg::fontdb::Database>,
	default_family: &str,
	size_hint: (u32, u32),
	max_decode_pixels: u64,
) -> Result<DynamicImage, ()> {
	let options = usvg::Options {
		fontdb,
		font_family: default_family.to_owned(),
		image_href_resolver: image_href_resolver(max_decode_pixels),
		..Default::default()
	};
	let tree = usvg::Tree::from_data(src_bytes, &options);
	let tree = match tree {
		Ok(t) => t,
		Err(_) => return Err(()),
	};
	let size = svg_size(&tree);
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
	if (width as u64)
		.checked_mul(height as u64)
		.map_or(true, |pixels| pixels > max_decode_pixels)
	{
		return Err(());
	}
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

fn svg_size(tree: &usvg::Tree) -> usvg::Size {
	let bb = tree.root().bounding_box();
	if bb.width() > tree.size().width() || bb.height() > tree.size().height() {
		if let Some(size) = usvg::Size::from_wh(bb.width(), bb.height()) {
			return size;
		}
	}
	tree.size()
}
