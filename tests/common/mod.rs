#![allow(dead_code)]

pub mod seeder;

use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use wisplet::config::Config;
use wisplet::engine::{self, Source, Status};
use wisplet::{Error, Result};
use tokio::task::LocalSet;

pub fn test_config(peers: Vec<std::net::SocketAddr>) -> Config {
    Config {
        enable_dht: false,
        extra_peers: peers,
        connect_timeout: Duration::from_secs(3),
        metadata_timeout: Duration::from_secs(10),
        ..Config::default()
    }
}

/// Run the engine to completion (or `limit`), returning the result and final status.
pub async fn run_download(
    source: Source,
    cfg: Config,
    dir: &Path,
    limit: Duration,
) -> (Result<()>, Rc<Status>) {
    run_download_with_hook(source, cfg, dir, limit, &|_, _| {}).await
}

pub async fn run_download_with_hook(
    source: Source,
    cfg: Config,
    dir: &Path,
    limit: Duration,
    hook: &dyn Fn(&wisplet::metainfo::Torrent, &[u8]),
) -> (Result<()>, Rc<Status>) {
    let status = Status::new([0; 20], "test");
    let local = LocalSet::new();
    let res = local
        .run_until(async {
            tokio::time::timeout(
                limit,
                engine::run(source, Rc::new(cfg), dir.to_path_buf(), status.clone(), hook),
            )
            .await
        })
        .await;
    let res = match res {
        Ok(r) => r,
        Err(_) => Err(Error::Timeout("test time limit")),
    };
    (res, status)
}
