//! Controlled DLNA probe: requires an explicit renderer URL. Never selects or
//! changes an arbitrary discovered device. Reports command/progress, not AV latency.
use ktv_casting_lib::{
    cast::{Caster, Quality, SongRef, dlna_caster::DlnaCaster},
    connect_dlna_device, set_dlna_quality,
};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(120), run()).await?
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<_> = std::env::args().collect();
    let device = args
        .get(1)
        .ok_or("usage: probe_seek DEVICE_URL [BVID-page0] [seconds,...]")?;
    let song = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("BV1dqj16aECG-page0");
    let positions: Vec<u32> = args
        .get(3)
        .map(String::as_str)
        .unwrap_or("185,75,140,35,280")
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()?;
    let (controller, device, ip, port, _, shared) =
        connect_dlna_device(device.clone(), tokio::runtime::Handle::current()).await?;
    let caster =
        DlnaCaster::new(controller, device, ip, port).with_sessions(shared.media_sessions.clone());
    set_dlna_quality(Quality::P1080)?;
    caster.play_song(&SongRef(song.into())).await?;
    let session = shared
        .media_sessions
        .current()
        .ok_or("no active media session")?;
    let media = session.prepare(&reqwest::Client::new()).await?;
    let ktv_casting_lib::media_session::SessionMedia::Seekable { cache, mp4 } = media.as_ref()
    else {
        caster.stop().await?;
        return Err("1080P seek benchmark rejected: playback fell back to a direct stream".into());
    };
    if args.get(4).is_some_and(|arg| arg == "--warm-all") {
        // Diagnostic control only. Bound it below the session capacity to leave
        // room for source block alignment; never make playback depend on this.
        if mp4.len > 200 * 1024 * 1024 {
            return Err("fixture exceeds warm-all test limit".into());
        }
        let started = Instant::now();
        let mut response = reqwest::Client::new()
            .get(format!("http://{ip}:{port}/{}", session.path()))
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = 0u64;
        while let Some(chunk) = response.chunk().await? {
            bytes += chunk.len() as u64;
        }
        if bytes != mp4.len {
            return Err("warm-all response length mismatch".into());
        }
        println!(
            "{}",
            serde_json::json!({"event":"warm_all_control", "bytes":bytes,
            "elapsed_ms":started.elapsed().as_millis(), "not_production_requirement":true})
        );
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    for position in positions {
        let started = Instant::now();
        let cache_before = cache.stats();
        let result = caster.seek(position).await;
        println!(
            "{}",
            serde_json::json!({"event":"seek_command", "wall_time":chrono::Utc::now().to_rfc3339(), "target":position, "elapsed_ms":started.elapsed().as_millis(), "success":result.is_ok(), "error":result.err().map(|e| e.to_string())})
        );
        // Do not enqueue the next test seek while the previous one is still
        // recovering. Progress telemetry is not proof of decoded audio/video.
        let mut previous = None;
        let mut advancing = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(p) = caster.get_progress().await {
                if previous != Some(p.current_secs) {
                    println!(
                        "{}",
                        serde_json::json!({"event":"device_progress", "target":position, "elapsed_ms":started.elapsed().as_millis(), "position":p.current_secs, "total":p.total_secs})
                    );
                    previous = Some(p.current_secs);
                }
                if p.current_secs > position && p.current_secs <= position + 3 {
                    advancing = true;
                    break;
                }
            }
        }
        let cache_after = cache.stats();
        println!(
            "{}",
            serde_json::json!({"event":"seek_observation", "target":position,
            "elapsed_ms":started.elapsed().as_millis(), "progress_advancing":advancing,
                "av_recovery_verified":false, "source_requests":cache_after.0-cache_before.0,
                "cache_hits":cache_after.1-cache_before.1})
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    caster.stop().await?;
    Ok(())
}
