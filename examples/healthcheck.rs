use std::{io::Read, net::SocketAddr, str::FromStr};

use axum::{response::IntoResponse, Router};

fn main() {
	let args:Vec<String>=std::env::args().collect();
	let bind_port=args.get(1).expect("args[1]=bind_port");
	let target_url=args.get(2).expect("args[2]=target_url");
	let http_addr:SocketAddr = SocketAddr::new("127.0.0.1".parse().unwrap(),bind_port.parse().expect("bind_port parse"));
	let self_url=reqwest::Url::from_str(&format!("http://{}:{}/dummy.png",http_addr.ip().to_string(),http_addr.port())).unwrap();
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	rt.spawn(async move{
		let app = Router::new();
		let app=app.route("/dummy.png",axum::routing::get(||async{
			let mut dummy_png=vec![];
			std::fs::File::open("asset/dummy.png").expect("not found dummy.png").read_to_end(&mut dummy_png).expect("load error dummy.png");
			(axum::http::StatusCode::OK,dummy_png).into_response()
		}));
		axum::Server::bind(&http_addr).serve(app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
	});
	let mut local_ok=false;
	for _ in 0..20{
		std::thread::sleep(std::time::Duration::from_millis(50));
		let self_url=self_url.clone();
		let status=rt.block_on(async move{
			if let Ok(s)=reqwest::get(self_url).await{
				s.status().as_u16()
			}else{
				504
			}
		});
		if status==200{
			local_ok=true;
			break;
		}
		std::thread::sleep(std::time::Duration::from_millis(50));
	}
	if !local_ok{
		println!("test server bind error");
		std::process::exit(1);
	}
	for _ in 0..5{
		let self_url=self_url.to_string();
		let status=rt.block_on(async move{
			if let Ok(s)=reqwest::get(format!("{}?url={}",target_url,self_url)).await{
				s.status().as_u16()
			}else{
				504
			}
		});
		if status==200{
			println!("ok");
			std::process::exit(0);
		}
		std::thread::sleep(std::time::Duration::from_millis(500));
	}
	std::process::exit(2);
}
