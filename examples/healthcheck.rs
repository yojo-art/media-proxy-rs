fn main() {
	let args: Vec<String> = std::env::args().collect();
	let target_url = args.get(1).expect("args[1]=target_url");
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.unwrap();
	let client = reqwest::Client::builder();
	// Never honour HTTP(S)_PROXY/ALL_PROXY: a loopback probe must reach healthz
	// directly instead of being rerouted through any operator-exported proxy.
	let client = client.no_proxy();
	let client = client.timeout(std::time::Duration::from_millis(400));
	let client = client.build().unwrap();
	// Fail fast. This runs as Docker's HEALTHCHECK (see Dockerfile) whose timeout
	// is 3s, so the total budget here must stay well under it. Cold-start
	// readiness is handled by HEALTHCHECK --start-period and by the build-time
	// wait loop, not by a long retry loop in this binary. Budget: 3*400ms + 2*100ms.
	for attempt in 0..3 {
		let target_url = target_url.clone();
		let client = client.clone();
		let status = rt.block_on(async move {
			match client.get(target_url).send().await {
				Ok(s) => s.status().as_u16(),
				Err(_) => 504,
			}
		});
		if status == 200 {
			println!("ok");
			std::process::exit(0);
		}
		if attempt < 2 {
			std::thread::sleep(std::time::Duration::from_millis(100));
		}
	}
	println!("healthcheck failed");
	std::process::exit(1);
}
