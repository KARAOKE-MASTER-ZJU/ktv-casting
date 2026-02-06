use crate::dlna_controller::{DlnaController, DlnaDevice};
use crate::playlist_manager::PlaylistManager;
use actix_web::{App, HttpServer, web};
use log::info;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock}; // 改用 RwLock 以支持重置
use tokio::sync::Mutex;

#[cfg(target_os = "android")]
pub mod android;

pub mod bilibili_parser;
pub mod dlna_controller;
pub mod media_server;
pub mod mp4_util;
pub mod playlist_manager;

// --- 全局静态容器：改为 RwLock<Option<...>> 以支持重新初始化 ---
pub static ENGINE_STATE: RwLock<Option<Arc<EngineContext>>> = RwLock::new(None);

pub struct EngineContext {
    pub controller: DlnaController,
    pub device: DlnaDevice,
    pub playlist_manager: PlaylistManager,
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    pub local_ip: std::net::IpAddr,
    pub server_port: u16,
    pub is_playing: AtomicBool,
    pub rt: tokio::runtime::Runtime,
}

pub struct SharedState {
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
}

// --- 辅助工具函数 ---
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

/// 重置引擎，释放资源
pub fn reset_engine() {
    if let Ok(mut guard) = ENGINE_STATE.write() {
        info!("释放引擎资源...");
        *guard = None;
    }
}

/// 获取当前播放进度（秒）
pub async fn get_current_progress() -> (i32, i32) {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            return match ctx.controller.get_secs(&ctx.device).await {
                Ok((curr, total)) => (curr as i32, total as i32),
                Err(_) => (-1 , -1),
            };
        }
    }
    (-1, -1)
}

/// 切换下一首歌曲
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

/// 跳转到指定秒数
pub async fn jump_to_secs(target_secs: u32) -> Result<(), Box<dyn std::error::Error>> {

    let ctx = {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        guard.as_ref().cloned().ok_or("Engine not initialized")?
    };

    ctx.controller.seek(&ctx.device, target_secs)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;

    Ok(())
}

/// 启动引擎核心逻辑
pub async fn start_engine_core(
    base_url_str: String,
    room_id: String,
    loc_str: String,
    rt: tokio::runtime::Runtime,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    info!("开始初始化核心引擎: {}, Room: {}", loc_str, room_id);

    // A. 如果已有旧引擎，先清理掉
    if let Ok(mut guard) = ENGINE_STATE.write() {
        if guard.is_some() {
            info!("检测到旧引擎正在运行，正在重置以连接新设备...");
            *guard = None;
            // 给系统时间释放端口
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }

    // B. 初始化新运行时
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

    info!("目标设备 IP 地址: {}", target_ip);

    let pm = PlaylistManager::new(&base_url_str, room_id);
    let cache = Arc::new(Mutex::new(std::collections::HashMap::new()));

    let shared_state = web::Data::new(SharedState {
        duration_cache: cache.clone(),
    });
    let port = 8080u16;

    // 启动 HttpServer (使用当前 Runtime 的 spawn)
    tokio::spawn(async move {
        info!("正在启动媒体服务器...");
        let _ = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(reqwest::Client::new()))
                .app_data(shared_state.clone())
                .service(media_server::proxy_handler)
        })
        .workers(1)
        .bind(("0.0.0.0", port))
        .unwrap()
        .run()
        .await;
    });

    let local_ip_addr: std::net::IpAddr = get_best_local_ip(target_ip).parse().unwrap();

    // 配置同步回调
    let ctrl_sync = controller.clone();
    let dev_sync = device.clone();
    pm.start_sync(move |video_url| {
        let c = ctrl_sync.clone();
        let d = dev_sync.clone();
        let ip_obj = local_ip_addr;
        let uri_path = video_url.clone();
        Box::pin(async move {
            info!("通知设备准备拉取路径: {}", uri_path);
            let _ = c.stop(&d).await;
            if let Ok(_) = c.set_avtransport_uri(&d, &uri_path, "", ip_obj, port).await {
                let _ = c.play(&d).await;
            }
        })
    });

    // C. 打包存入全局状态
    let ctx = Arc::new(EngineContext {
        controller,
        device,
        playlist_manager: pm,
        duration_cache: cache,
        local_ip: local_ip_addr,
        server_port: port,
        is_playing: std::sync::atomic::AtomicBool::new(true),
        rt,
    });

    if let Ok(mut guard) = ENGINE_STATE.write() {
        *guard = Some(ctx);
        info!("Rust Engine 已重新初始化，设备连接成功");
    }
}

// 获取当前歌曲总时长
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

// 切换播放/暂停状态
pub async fn toggle_pause_core() -> Result<bool, Box<dyn std::error::Error>> {
    let (target_state, _) = {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        let ctx = guard.as_ref().ok_or("Engine not initialized")?;
        let curr = ctx.is_playing.load(Ordering::SeqCst);
        (!curr, curr)
    };

    // 执行 DLNA 操作
    {
        let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
        let ctx = guard.as_ref().unwrap();
        if target_state {
            ctx.controller.play(&ctx.device).await?;
        } else {
            ctx.controller.pause(&ctx.device).await?;
        }
        ctx.is_playing.store(target_state, Ordering::SeqCst);
    }

    Ok(target_state)
}

// 设置音量
pub async fn set_volume_core(volume: u32) -> Result<u32, Box<dyn std::error::Error>> {
    let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
    let ctx = guard.as_ref().ok_or("Engine not initialized")?;
    let target = volume.clamp(0, 100);
    ctx.controller.set_volume(&ctx.device, target).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(target)
}

// 获取音量
pub async fn get_volume_core() -> Result<u32, Box<dyn std::error::Error>> {
    let guard = ENGINE_STATE.read().map_err(|_| "Lock error")?;
    let ctx = guard.as_ref().ok_or("Engine not initialized")?;
    let v = ctx.controller.get_volume(&ctx.device).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(v)
}

// 搜索设备
pub async fn discover_devices_core() -> Vec<DlnaDevice> {
    DlnaController::new()
        .discover_devices()
        .await
        .unwrap_or_default()
}

/// 获取当前正在播放的歌曲标题
pub async fn get_current_song_title_core() -> String {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            // 调用 PlaylistManager 中我们之前添加的 get_song_title
            return ctx.playlist_manager.get_song_title().await
                .unwrap_or_else(|| "暂无歌曲".to_string());
        }
    }
    "未连接".to_string()
}