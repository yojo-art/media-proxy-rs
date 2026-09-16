use std::sync::Arc;

use image::{DynamicImage, ImageBuffer};
use resvg::usvg;

use crate::RequestContext;

impl RequestContext{
	pub(crate) fn encode_svg(&self,fontdb:Arc<usvg::fontdb::Database>)->Result<DynamicImage,()>{
		let mut options=usvg::Options{
			fontdb:fontdb.clone(),
			..Default::default()
		};
		for f in fontdb.faces(){
			if let Some((name,_))=f.families.get(0){
				//デフォルトフォントに存在する事が確実なフォントを使う
				options.font_family=name.to_owned();
				break;
			}
		}
		let tree=usvg::Tree::from_data(&self.src_bytes,&options);
		let tree=match tree{
			Ok(t)=>t,
			Err(_)=>return Err(())
		};
		let size=size(&tree);
		let hint=self.image_size_hint();
		
		let (width,height,scale)=if size.width()>hint.0 as f32||size.height()>hint.1 as f32{
			let scale = f32::min(hint.0 as f32 / size.width(), hint.1 as f32 / size.height());
			let width=std::cmp::max((size.width() * scale).round() as u32,1);
			let height=std::cmp::max((size.height() * scale).round() as u32,1);
			(width,height,scale)
		}else{
			(size.width() as u32,size.height() as u32,1f32)
		};
	let tf=usvg::Transform::from_scale(scale,scale);
	// Pixel-budget gate before allocation (finding #4).
	if (width as u64).checked_mul(height as u64).map_or(true,|pixels|pixels>self.max_decode_pixels()){
		return Err(());
	}
	// u32 arithmetic can overflow on crafted SVG sizes; never panic (finding #3).
	// A pixel-budget gate is applied separately (finding #4).
	let len=(width as u64).checked_mul(height as u64).and_then(|n|n.checked_mul(4));
	let len=len.and_then(|n|usize::try_from(n).ok());
	let Some(len)=len else{
		return Err(());
	};
	let mut rgba=vec![0;len];
	let Some(mut pxmap)=resvg::tiny_skia::PixmapMut::from_bytes(&mut rgba,width,height) else{
		return Err(());
	};
		resvg::render(&tree,tf,&mut pxmap);
		match ImageBuffer::from_vec(width,height,rgba){
			Some(img)=>{
				Ok(DynamicImage::ImageRgba8(img))
			},
			None=>{
				Err(())
			}
		}
	}
}
fn size(tree:&usvg::Tree)->usvg::Size{
	let bb=tree.root().bounding_box();
	if bb.width()>tree.size().width()||bb.height()>tree.size().height(){
		if let Some(size)=usvg::Size::from_wh(bb.width(),bb.height()){
			return size;
		}
	}
	tree.size()
}
