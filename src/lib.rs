use crate::cast::dlna_caster::DlnaCaster;
use crate::dlna_controller::{DlnaController, DlnaDevice};
use crate::playlist_manager::PlaylistManager;
use actix_web::{App, HttpServer, web};
use log::{info, debug};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

#[cfg(target_os = "android")]
pub mod android;

pub mod bilibili_parser;
pub mod cast;
pub mod dlna_controller;
pub mod media_server;
pub mod mp4_util;
pub mod playlist_manager;

pub static ENGINE_STATE: RwLock<Option<Arc<EngineContext>>> = RwLock::new(None);

/// Shared state for the Bilibili QR login flow (polled by Android UI / CLI).
pub static BILI_AUTH_STATE: RwLock<BiliAuthState> = RwLock::new(BiliAuthState::Idle);

pub enum BiliAuthState {
    Idle,
    AwaitingScan { qr_url: String },
    Success { access_token: String, mid: u64 },
    Error(String),
}

pub struct EngineContext {
    pub caster: Arc<dyn cast::Caster>,
    pub playlist_manager: PlaylistManager,
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    pub local_ip: std::net::IpAddr,
    pub server_port: u16,
    pub is_playing: AtomicBool,
    pub rt: tokio::runtime::Runtime,
}

pub struct SharedState {
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    pub eplus_auth: Arc<tokio::sync::Mutex<Option<String>>>,
}

pub(crate) fn get_best_local_ip(target_device_ip: &str) -> String {
    let interfaces = local_ip_address::list_afinet_netifas().unwrap_or_default();
    let target_u32 = target_device_ip.parse::<Ipv4Addr>().map(u32::from).ok();
    if let Some(target) = target_u32 {
        let best = interfaces
            .iter()
            .filter_map(|(name, ip)| {
                if let std::net::IpAddr::V4(v4) = ip {
                    let m_bits = (target ^ u32::from(*v4)).leading_zeros();
                    Some((m_bits, ip.to_string(), name))
                } else {
                    None
                }
            })
            .max_by_key(|(bits, _, _)| *bits);
        if let Some((_, ip_str, _)) = best {
            return ip_str;
        }
    }
    local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

pub fn reset_engine() {
    if let Ok(mut guard) = ENGINE_STATE.write() {
        info!("释放引擎资源...");
        *guard = None;
    }
}

pub async fn get_current_progress() -> (i32, i32) {
    let ctx = {
        let guard = ENGINE_STATE.read().ok();
        guard.and_then(|g| g.as_ref().cloned())
    };
    let Some(ctx) = ctx else { return (-1, -1) };

    match ctx.caster.get_progress().await {
        Ok(p) => {
            let cached_total = get_total_duration().await;
            let playing = ctx.playlist_manager.get_song_playing().await;
            debug!(
                "progress: curr={} device_total={} cached_total={} playing={:?}",
                p.current_secs, p.total_secs, cached_total, playing
            );
            let total = if cached_total > 0 && cached_total != p.total_secs {
                cached_total as i32
            } else {
                p.total_secs as i32
            };
            (p.current_secs as i32, total)
        }
        Err(_) => (-1, -1),
    }
}

pub fn trigger_next_song() {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            let ctx_task = Arc::clone(ctx);
            ctx.rt.spawn(async move {
                let mut pm = ctx_task.playlist_manager.clone();
                let _ = pm.next_song().await;
            });
        }
    }
}

pub async fn jump_to_secs(target_secs: u32) -> Result<(), Box<dyn std::error::Error>> {
    let ctx = {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        guard.as_ref().cloned().ok_or("Engine not initialized")?
    };
    ctx.caster.seek(target_secs).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

pub async fn start_engine_core(
    base_url_str: String,
    room_id: String,
    loc_str: String,
    rt: tokio::runtime::Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    info!("开始初始化核心引擎: {}, Room: {}", loc_str, room_id);

    if let Ok(mut guard) = ENGINE_STATE.write() {
        if guard.is_some() {
            info!("检测到旧引擎正在运行，正在重置...");
            *guard = None;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    let handle = rt.handle().clone();
    let (controller, device, local_ip_addr, port, cache, _) =
        connect_dlna_device(loc_str, handle).await?;

    let caster: Arc<dyn cast::Caster> =
        Arc::new(DlnaCaster::new(controller, device, local_ip_addr, port));

    connect_room(base_url_str, room_id, caster, local_ip_addr, port, cache, rt).await?;

    info!("Rust Engine 已初始化，设备连接成功");
    Ok(())
}

pub async fn connect_dlna_device(
    loc_str: String,
    handle: tokio::runtime::Handle,
) -> Result<
    (
        DlnaController,
        DlnaDevice,
        std::net::IpAddr,
        u16,
        Arc<Mutex<std::collections::HashMap<String, u32>>>,
        web::Data<SharedState>,
    ),
    Box<dyn std::error::Error>,
> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    info!("开始连接DLNA设备: {}", loc_str);

    let controller = DlnaController::new();
    let uri = loc_str.parse().expect("解析 URL 失败");
    let device_obj = rupnp::Device::from_url(uri).await.expect("连接设备失败");

    let device = DlnaDevice {
        friendly_name: device_obj.friendly_name().to_string(),
        location: loc_str.clone(),
        device: device_obj,
        services: vec![],
    };

    let target_ip = loc_str
        .split('/')
        .nth(2)
        .and_then(|hp| hp.split(':').next())
        .unwrap_or("127.0.0.1");

    let cache = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let shared_state = web::Data::new(SharedState {
        duration_cache: cache.clone(),
        eplus_auth: Arc::new(tokio::sync::Mutex::new(None)),
    });
    let port = 8080u16;

    let shared_state_clone = shared_state.clone();
    handle.spawn(async move {
        info!("正在启动媒体服务器...");
        let app_factory = move || {
            App::new()
                .app_data(web::Data::new(reqwest::Client::new()))
                .app_data(shared_state_clone.clone())
                .service(media_server::proxy_handler)
        };
        let _ = HttpServer::new(app_factory)
            .workers(1)
            .bind(("0.0.0.0", port))
            .unwrap()
            .run()
            .await;
    });

    let local_ip_addr: std::net::IpAddr = get_best_local_ip(target_ip).parse().unwrap();
    Ok((controller, device, local_ip_addr, port, cache, shared_state))
}

pub async fn connect_room(
    base_url_str: String,
    room_id: String,
    caster: Arc<dyn cast::Caster>,
    local_ip_addr: std::net::IpAddr,
    port: u16,
    cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    rt: tokio::runtime::Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("开始连接房间: {}", room_id);

    let pm = PlaylistManager::new(&base_url_str, room_id);

    let caster_cb = Arc::clone(&caster);
    let cache_cb = Arc::clone(&cache);
    pm.start_sync(move |video_url| {
        let c = Arc::clone(&caster_cb);
        let cache = Arc::clone(&cache_cb);
        Box::pin(async move {
            info!("通知设备准备拉取路径: {}", video_url);
            let _ = c.play_song(&cast::SongRef(video_url.clone())).await;

            // 如果是 BV 视频，立刻从 bilibili API 获取时长并写入 cache
            if let Some(bvid) = extract_bvid(&video_url) {
                if let Ok((_, duration)) = crate::bilibili_parser::get_page_info(&bvid, 0).await {
                    if duration > 0 {
                        cache.lock().await.insert(video_url, duration);
                        debug!("[DLNA] 预填充 BV 视频时长到 cache: {} -> {}s", bvid, duration);
                    }
                }
            }
        })
    });

    let ctx = Arc::new(EngineContext {
        caster,
        playlist_manager: pm,
        duration_cache: cache,
        local_ip: local_ip_addr,
        server_port: port,
        is_playing: AtomicBool::new(true),
        rt,
    });

    if let Ok(mut guard) = ENGINE_STATE.write() {
        *guard = Some(ctx);
        info!("房间连接成功");
    }

    Ok(())
}

fn extract_bvid(video_url: &str) -> Option<String> {
    if let Some((bv, _)) = video_url.split_once("-page") {
        if bv.starts_with("BV") {
            return Some(bv.to_string());
        }
    }
    None
}

pub async fn get_total_duration() -> u32 {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            if let Some(playing) = ctx.playlist_manager.get_song_playing().await {
                if let Some(&d) = ctx.duration_cache.lock().await.get(&playing) {
                    return d;
                }
            }
        }
    }
    0
}

pub async fn toggle_pause_core() -> Result<bool, Box<dyn std::error::Error>> {
    let target_state = {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        let ctx = guard.as_ref().ok_or("Engine not initialized")?;
        !ctx.is_playing.load(Ordering::SeqCst)
    };

    let ctx = {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        guard.as_ref().cloned().ok_or("Engine not initialized")?
    };

    if target_state {
        ctx.caster.resume().await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    } else {
        ctx.caster.pause().await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    }
    ctx.is_playing.store(target_state, Ordering::SeqCst);

    Ok(target_state)
}

pub async fn set_volume_core(volume: u32) -> Result<u32, Box<dyn std::error::Error>> {
    let ctx = {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        guard.as_ref().cloned().ok_or("Engine not initialized")?
    };
    let target = volume.clamp(0, 100);
    ctx.caster.set_volume(target).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(target)
}

pub async fn get_volume_core() -> Result<u32, Box<dyn std::error::Error>> {
    let ctx = {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        guard.as_ref().cloned().ok_or("Engine not initialized")?
    };
    let v = ctx.caster.get_volume().await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(v.unwrap_or(0))
}

pub async fn start_bilibili_engine_core(
    base_url_str: String,
    room_id: String,
    session: cast::bilibili_caster::BilibiliSession,
    device_buvid: String,
    rt: tokio::runtime::Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    info!("开始初始化Bilibili引擎, Room: {}", room_id);

    if let Ok(mut guard) = ENGINE_STATE.write() {
        if guard.is_some() {
            *guard = None;
        }
    }

    let caster: Arc<dyn cast::Caster> = Arc::new(
        cast::bilibili_caster::BilibiliCaster::new(session, device_buvid)
    );
    let cache = Arc::new(Mutex::new(std::collections::HashMap::new()));
    connect_room(base_url_str, room_id, caster, "127.0.0.1".parse().unwrap(), 0, cache, rt).await
}

pub async fn discover_devices_core() -> Vec<DlnaDevice> {
    DlnaController::new()
        .discover_devices()
        .await
        .unwrap_or_default()
}

pub async fn discover_device_from_url_core(url: String) -> Result<DlnaDevice, String> {
    DlnaController::new()
        .get_device_from_url(&url)
        .await
        .map_err(|e| e.to_string())
}

pub async fn get_current_song_title_core() -> String {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            return ctx.playlist_manager.get_song_title().await
                .unwrap_or_else(|| "暂无歌曲".to_string());
        }
    }
    "未连接".to_string()
}
