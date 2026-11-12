//! Hold a headless connection to a `lynxrdp-session` and report how much of
//! the picture it receives is black.
//!
//! Used to reproduce the black desktop a GNOME session can come up with when
//! a client's first resize lands while the shell is starting, and to show
//! that a fix clears it. Not a test: it prints, it does not judge.
//!
//! ```text
//! headless_probe 127.0.0.1:PORT WIDTH HEIGHT SECONDS
//! ```

use std::time::{Duration, Instant};

use lynxrdp_client::connection::{Client, ClientEvent, ConnectOptions};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: headless_probe ADDR WIDTH HEIGHT SECONDS");
        std::process::exit(2);
    }
    let addr: std::net::SocketAddr = args[1].parse().expect("address");
    let width: u16 = args[2].parse().expect("width");
    let height: u16 = args[3].parse().expect("height");
    let seconds: u64 = args[4].parse().expect("seconds");
    let opts = ConnectOptions {
        size: Some((width, height)),
        ..Default::default()
    };
    let started = Instant::now();
    let mut client = Client::connect(addr, &opts, None).expect("connect");
    println!(
        "{:>6.2}s connected: server {}x{}",
        started.elapsed().as_secs_f64(),
        client.size().0,
        client.size().1
    );
    let mut pushbacks = 0u32;
    // PROBE_BURST_MS: that long after connecting, ask for a slightly
    // different size and 150 ms later for the original one again -- a root
    // resize burst timed to land while the shell is bringing its stage up.
    let mut burst: Option<Instant> = std::env::var("PROBE_BURST_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|ms| started + Duration::from_millis(ms));
    let mut burst_back: Option<Instant> = None;
    let mut next_report = started + Duration::from_secs(1);
    let deadline = started + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        match client.poll_event(Duration::from_millis(50)) {
            Ok(Some(ClientEvent::Resized {
                width: w,
                height: h,
            })) => {
                println!("{:>6.2}s resized to {w}x{h}", started.elapsed().as_secs_f64());
                // The real client's fullscreen viewport does not change size
                // because the desktop applied a mode of its own: it asks for
                // its own size back, which is the burst being reproduced.
                if (w, h) != (u32::from(width), u32::from(height)) && pushbacks < 5 {
                    pushbacks += 1;
                    println!(
                        "{:>6.2}s asking for {width}x{height} again",
                        started.elapsed().as_secs_f64()
                    );
                    let _ = client.request_resize(width, height);
                }
            }
            Ok(_) => {}
            Err(e) => {
                println!("{:>6.2}s error: {e:#}", started.elapsed().as_secs_f64());
                break;
            }
        }
        if let Some(reason) = client.closed() {
            println!("{:>6.2}s closed: {reason}", started.elapsed().as_secs_f64());
            break;
        }
        if burst.is_some_and(|at| Instant::now() >= at) {
            burst = None;
            println!(
                "{:>6.2}s burst: asking for {}x{}",
                started.elapsed().as_secs_f64(),
                width - 128,
                height - 84
            );
            let _ = client.request_resize(width - 128, height - 84);
            burst_back = Some(Instant::now() + Duration::from_millis(150));
        }
        if burst_back.is_some_and(|at| Instant::now() >= at) {
            burst_back = None;
            println!(
                "{:>6.2}s burst: asking for {width}x{height} back",
                started.elapsed().as_secs_f64()
            );
            let _ = client.request_resize(width, height);
        }
        if Instant::now() >= next_report {
            next_report += Duration::from_secs(1);
            let fb = client.framebuffer();
            let pixels = fb.pixels();
            let lit = pixels.iter().filter(|&&p| p & 0xff_ff_ff != 0).count();
            println!(
                "{:>6.2}s {}x{} frames={} bytes={} lit={:.1}%",
                started.elapsed().as_secs_f64(),
                fb.width(),
                fb.height(),
                client.frames_received(),
                client.bytes_received(),
                100.0 * lit as f64 / pixels.len().max(1) as f64
            );
        }
    }
    let fb = client.framebuffer();
    let lit = fb.pixels().iter().filter(|&&p| p & 0xff_ff_ff != 0).count();
    println!(
        "FINAL lit={:.1}% frames={}",
        100.0 * lit as f64 / fb.pixels().len().max(1) as f64,
        client.frames_received()
    );
}
