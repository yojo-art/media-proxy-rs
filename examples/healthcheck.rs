fn main() {
	let args:Vec<String>=std::env::args().collect();
	let target_url=args.get(1).expect("args[1]=target_url");
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	let client=reqwest::Client::builder();
	let client=client.timeout(std::time::Duration::from_millis(500));
	let client=client.build().unwrap();
	// Only asks the proxy whether it is serving. Must not fetch through the
	// proxy: the SSRF policy denies loopback, which is where the container's
	// own healthcheck can reach.
	for _ in 0..20{
		let target_url=target_url.clone();
		let client=client.clone();
		let status=rt.block_on(async move{
			match client.get(target_url).send().await{
				Ok(s)=>s.status().as_u16(),
				Err(_)=>504,
			}
		});
		if status==200{
			println!("ok");
			std::process::exit(0);
		}
		std::thread::sleep(std::time::Duration::from_millis(250));
	}
	println!("healthcheck failed");
	std::process::exit(1);
}
