//! Benchmark helper: serves a deterministic synthetic torrent from a *separate process*, so
//! the downloader's CPU/RAM can be measured without the seeder's cost mixed in.
//!
//! usage: bench_seeder <size_mib> <torrent_out_path> [piece_kib=256] [seeders=1]
//! Prints one `LISTEN <ip:port>` line per seeder once ready, then serves until killed.

#[path = "../tests/common/seeder.rs"]
mod seeder;

use std::sync::Arc;

use seeder::{build_single, Behavior, PatternSource, Seeder};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: bench_seeder <size_mib> <torrent_out_path> [piece_kib=256]");
        std::process::exit(2);
    }
    let size = args[1].parse::<u64>().expect("size_mib") << 20;
    let piece_kib: u32 = args.get(3).map_or(256, |s| s.parse().expect("piece_kib"));
    let src = Arc::new(PatternSource(size));
    let spec = build_single("bench.bin", &*src, piece_kib * 1024);
    std::fs::write(&args[2], &spec.torrent_file).expect("write torrent");
    let n: usize = args.get(4).map_or(1, |s| s.parse().expect("seeders"));
    let mut seeders = Vec::new();
    for _ in 0..n {
        let s = Seeder::start(&spec, src.clone(), Behavior::default()).await;
        println!("LISTEN {}", s.addr);
        seeders.push(s);
    }
    std::future::pending::<()>().await;
}
