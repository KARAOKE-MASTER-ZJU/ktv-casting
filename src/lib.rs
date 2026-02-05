use crate::dlna_controller::{DlnaController, DlnaDevice};
use actix_web::{App, HttpServer, web};
use anyhow::Result;
use local_ip_address::local_ip;
use log::{info, warn};
use playlist_manager::PlaylistManager;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

mod bilibili_parser;
pub mod dlna_controller;
mod media_server;
mod mp4_util;
mod playlist_manager;

pub struct SharedState {
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
}

// --- 核心执行引擎 ---
pub async fn run_app(
    base_url: String,
    room_id: u64,
    controller: DlnaController,
    device: DlnaDevice,
    set_length: Arc<dyn Fn(u64) + Send + Sync + 'static>,
    set_position: Arc<dyn Fn(u64) + Send + Sync + 'static>,
) -> Result<()> {
    let server_port = 8080;
    let local_ip = local_ip()?;

    let playlist: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let pm = PlaylistManager::new(&base_url, room_id, playlist.clone());
    let duration_cache = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let shared_state = web::Data::new(SharedState {
        duration_cache: duration_cache.clone(),
    });
    let client = web::Data::new(Client::builder().use_rustls_tls().build()?);

    let server = HttpServer::new(move || {
        App::new()
            .app_data(client.clone())
            .app_data(shared_state.clone())
            .service(media_server::proxy_handler)
    })
    .bind(("0.0.0.0", server_port))?
    .run();

    // 后台任务 1: 同步播放列表
    let c1 = controller.clone();
    let d1 = device.clone();
    let pm_sync = pm.clone();
    pm_sync.start_sync(move |url| {
        let c = c1.clone();
        let d = d1.clone();
        Box::pin(async move {
            let _ =
                retry_dlna_op(|| async { c.stop(&d).await.map_err(|e| anyhow::anyhow!(e)) }).await;
            let _ = retry_dlna_op(|| async {
                c.set_avtransport_uri(&d, &url, "", local_ip, server_port)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            })
            .await;
            let _ =
                retry_dlna_op(|| async { c.play(&d).await.map_err(|e| anyhow::anyhow!(e)) }).await;
        })
    });

    // 后台任务 2: 状态轮询
    spawn_status_poller(
        controller.clone(),
        device.clone(),
        pm.clone(),
        duration_cache.clone(),
        set_length,
        set_position,
    );

    info!("投屏应用已启动");
    server.await?;
    Ok(())
}

fn spawn_status_poller(
    controller: DlnaController,
    device: DlnaDevice,
    pm: PlaylistManager,
    cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    set_length: Arc<dyn Fn(u64) + Send + Sync + 'static>,
    set_position: Arc<dyn Fn(u64) + Send + Sync + 'static>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;
            let mut total_secs = 0;
            if let Some(playing) = pm.get_song_playing().await {
                if let Some(&d) = cache.lock().await.get(&playing) {
                    total_secs = d;
                }
            }

            let op_res = retry_dlna_op(|| async {
                controller
                    .get_secs(&device)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            })
            .await;

            if let Ok((curr, _)) = op_res {
                if total_secs > 0 {
                    set_length(total_secs as u64);
                    if total_secs > curr && (total_secs - curr) <= 2 {
                        info!("准备切歌...");
                        let pm_next = pm.clone();
                        let _ = retry_dlna_op(move || {
                            let mut p = pm_next.clone();
                            async move { p.next_song().await.map_err(|e| anyhow::anyhow!(e)) }
                        })
                        .await;
                        sleep(Duration::from_secs(5)).await;
                    }
                }
                set_position(curr as u64);
            }
        }
    });
}

async fn retry_dlna_op<F, Fut, T>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("200") || msg.contains("202") {
                    return Ok(unsafe { std::mem::zeroed() });
                }
                warn!("操作失败: {}，重试中...", msg);
                sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
