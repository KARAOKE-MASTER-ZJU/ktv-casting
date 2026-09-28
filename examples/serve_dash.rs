//! Desktop integration probe for the same Rust index/cache/HTTP modules used by
//! the app. Binds loopback by default; never controls a discovered DLNA device.
//! cargo run --example serve_dash -- BV1dqj16aECG 18090
use actix_web::{App, HttpRequest, HttpServer, web};
use ktv_casting_lib::{
    bilibili_parser::{BilibiliMedia, get_bilibili_media},
    dash_index,
    song_cache::SongCache,
    virtual_mp4_http,
};
use std::{sync::Arc, time::Duration};

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<_> = std::env::args().collect();
    let bvid = args.get(1).map(String::as_str).unwrap_or("BV1dqj16aECG");
    let port: u16 = args.get(2).map(String::as_str).unwrap_or("18090").parse()?;
    let BilibiliMedia::Dash {
        video_url,
        audio_url,
        ..
    } = get_bilibili_media(bvid, Some(0), 80).await?
    else {
        return Err("expected DASH video".into());
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let started = std::time::Instant::now();
    let prepared = tokio::time::timeout(
        Duration::from_secs(90),
        dash_index::prepare(&client, &video_url, &audio_url),
    )
    .await??;
    let block_bytes: u64 = args
        .get(3)
        .map(String::as_str)
        .unwrap_or("131072")
        .parse()?;
    let cache = Arc::new(SongCache::new_with_block_size(
        &std::env::temp_dir(),
        client,
        prepared.sources,
        256 * 1024 * 1024,
        block_bytes,
    )?);
    let mp4 = Arc::new(prepared.mp4);
    eprintln!(
        "READY http://127.0.0.1:{port}/video.mp4 length={} preparation_ms={}",
        mp4.len,
        started.elapsed().as_millis()
    );
    HttpServer::new(move || {
        let mp4 = mp4.clone();
        let cache = cache.clone();
        App::new().route(
            "/video.mp4",
            web::route().to(move |req: HttpRequest| {
                let response = virtual_mp4_http::serve(&req, mp4.clone(), cache.clone(), 1);
                async move { response }
            }),
        )
    })
    .bind(("127.0.0.1", port))?
    .run()
    .await?;
    Ok(())
}
