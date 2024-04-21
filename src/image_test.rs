#[test]
fn encode_decode_png(){
	let dummy=include_bytes!("../asset/dummy.png");
	let img=image::load_from_memory(dummy).expect("load dummy.png");
	let mut buf=vec![];
	img.write_to(&mut std::io::Cursor::new(&mut buf),image::ImageFormat::Png).expect("encode png");
}
#[test]
#[cfg(feature="avif-decoder")]
fn encode_decode_avif(){
	let dummy=include_bytes!("../asset/dummy.png");
	let img=image::load_from_memory(dummy).expect("load dummy.png");
	let mut buf=vec![];
	img.write_to(&mut std::io::Cursor::new(&mut buf),image::ImageFormat::Avif).expect("encode avif");
	//https://github.com/image-rs/image/issues/1930
	//let format=image::guess_format(&buf).expect("guess format");
	let format=image::ImageFormat::Avif;
	image::load_from_memory_with_format(&buf,format).expect("decode avif");
}
#[test]
#[cfg(not(feature="avif-decoder"))]
fn encode_avif(){
	let dummy=include_bytes!("../asset/dummy.png");
	let img=image::load_from_memory(dummy).expect("load dummy.png");
	let mut buf=vec![];
	img.write_to(&mut std::io::Cursor::new(&mut buf),image::ImageFormat::Avif).expect("encode avif");
}
#[test]
fn encode_decode_webp(){
	let dummy=include_bytes!("../asset/dummy.png");
	let img=image::load_from_memory(dummy).expect("load dummy.png");
	let img=img.into_rgba8();
	let encoer=webp::Encoder::from_rgba(img.as_raw(),img.width(),img.height());
	let mut buf=vec![];
	buf.extend_from_slice(&encoer.encode(75f32));
	webp::Decoder::new(&buf).decode().unwrap();
}
