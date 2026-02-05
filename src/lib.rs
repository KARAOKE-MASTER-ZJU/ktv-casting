use crate::dlna_controller::{DlnaController, DlnaDevice};
use crate::playlist_manager::PlaylistManager;
use actix_web::{App, HttpServer, web};
use log::info;
use std::net::Ipv4Addr;
use std::sync::{Arc, RwLock}; // 改用 RwLock 以支持重置
use tokio::sync::Mutex;

pub mod bilibili_parser;
pub mod dlna_controller;
pub mod media_server;
pub mod mp4_util;
pub mod playlist_manager;

// --- 全局静态容器：改为 RwLock<Option<...>> 以支持重新初始化 ---
static ENGINE_STATE: RwLock<Option<Arc<EngineContext>>> = RwLock::new(None);

pub struct EngineContext {
    pub controller: DlnaController,
    pub device: DlnaDevice,
    pub playlist_manager: PlaylistManager,
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    pub local_ip: std::net::IpAddr,
    pub server_port: u16,
    pub rt: tokio::runtime::Runtime,
}

pub struct SharedState {
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
}

// --- 辅助工具函数 ---
pub fn get_best_local_ip(target_device_ip: &str) -> String {
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

#[allow(non_snake_case)]
pub mod android {
    use super::*;
    use jni::JNIEnv;
    use jni::objects::{JClass, JObject, JString};
    use jni::sys::{jint, jlong, jobjectArray, jsize};

    // 1. 日志初始化
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_initLogging(
        _env: JNIEnv,
        _class: JClass,
        level: jint,
    ) {
        let log_level = match level {
            0 => log::LevelFilter::Error,
            1 => log::LevelFilter::Warn,
            2 => log::LevelFilter::Info,
            _ => log::LevelFilter::Debug,
        };
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log_level)
                .with_tag("RUST_KTV"),
        );
    }

    // 2. 搜索接口
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_searchDevices(
        mut env: JNIEnv,
        _class: JClass,
    ) -> jobjectArray {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dlna_devices = rt.block_on(async {
            DlnaController::new()
                .discover_devices()
                .await
                .unwrap_or_default()
        });
        let cls = env
            .find_class("zju/bangdream/ktv/casting/DlnaDeviceItem")
            .unwrap();
        let array = env
            .new_object_array(dlna_devices.len() as jsize, &cls, JObject::null())
            .unwrap();
        for (i, d) in dlna_devices.iter().enumerate() {
            let name = env.new_string(&d.friendly_name).unwrap();
            let loc = env.new_string(&d.location).unwrap();
            let item = env
                .new_object(
                    &cls,
                    "(Ljava/lang/String;Ljava/lang/String;)V",
                    &[(&name).into(), (&loc).into()],
                )
                .unwrap();
            env.set_object_array_element(&array, i as jsize, item)
                .unwrap();
        }
        array.into_raw()
    }

    // 3. 核心初始化接口 (支持重复调用以更换设备)
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_startEngine(
        mut env: JNIEnv,
        _class: JClass,
        base_url: JString,
        room_id: jlong,
        target_location: JString,
    ) {
        let base_url_str: String = env.get_string(&base_url).unwrap().into();
        let loc_str: String = env.get_string(&target_location).unwrap().into();
        let _ = rustls::crypto::ring::default_provider().install_default();
        std::thread::spawn(move || {
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
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (controller, device, pm, cache, local_ip_addr, port) = rt.block_on(async {
                let controller = DlnaController::new();
                let uri = loc_str.parse().unwrap();
                let device_obj = rupnp::Device::from_url(uri).await.unwrap();
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

                let playlist = Arc::new(Mutex::new(vec![]));
                let pm = PlaylistManager::new(&base_url_str, room_id as u64, playlist);
                let cache = Arc::new(Mutex::new(std::collections::HashMap::new()));

                let shared_state = web::Data::new(SharedState {
                    duration_cache: cache.clone(),
                });
                let port = 8080u16;
                // 启动 HttpServer
                tokio::spawn(async move {
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
                let ctrl_sync = controller.clone();
                let dev_sync = device.clone();
                let local_ip_for_fn = local_ip_addr;
                pm.start_sync(move |video_url| {
                    let c = ctrl_sync.clone();
                    let d = dev_sync.clone();
                    let ip_obj = local_ip_for_fn;
                    let uri_path = video_url.clone();
                    Box::pin(async move {
                        info!("通知设备准备拉取路径: {}", uri_path);
                        let _ = c.stop(&d).await;
                        if let Ok(_) = c.set_avtransport_uri(&d, &uri_path, "", ip_obj, port).await
                        {
                            let _ = c.play(&d).await;
                        }
                    })
                });

                (controller, device, pm, cache, local_ip_addr, port)
            });

            // C. 打包并存入全局状态
            let ctx = Arc::new(EngineContext {
                controller,
                device,
                playlist_manager: pm,
                duration_cache: cache,
                local_ip: local_ip_addr,
                server_port: port,
                rt,
            });

            if let Ok(mut guard) = ENGINE_STATE.write() {
                *guard = Some(ctx);
                info!("Rust Engine 已重新初始化，设备连接成功");
            }
        });
    }

    // 4. 新增接口：重置引擎 (UI 点击重新选择设备时调用)
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_resetEngine(
        _env: JNIEnv,
        _class: JClass,
    ) {
        if let Ok(mut guard) = ENGINE_STATE.write() {
            info!("用户触发重置：释放引擎资源...");
            *guard = None; // 这会销毁 Arc 并停止内部 Runtime 及其任务
        }
    }

    // 5. 数据接口：获取当前播放进度
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_queryProgress(
        _env: JNIEnv,
        _class: JClass,
    ) -> jlong {
        if let Ok(guard) = ENGINE_STATE.read() {
            if let Some(ctx) = guard.as_ref() {
                return ctx.rt.block_on(async {
                    match ctx.controller.get_secs(&ctx.device).await {
                        Ok((curr, _)) => curr as jlong,
                        Err(_) => -1,
                    }
                });
            }
        }
        -1
    }

    // 6. 核心接口：获取当前歌曲总时长
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_queryTotalDuration(
        _env: JNIEnv,
        _class: JClass,
    ) -> jlong {
        if let Ok(guard) = ENGINE_STATE.read() {
            if let Some(ctx) = guard.as_ref() {
                return ctx.rt.block_on(async {
                    if let Some(playing) = ctx.playlist_manager.get_song_playing().await {
                        if let Some(&d) = ctx.duration_cache.lock().await.get(&playing) {
                            return d as jlong;
                        }
                    }
                    0
                });
            }
        }
        0
    }

    // 7. 控制接口：切歌
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_nextSong(
        _env: JNIEnv,
        _class: JClass,
    ) {
        if let Ok(guard) = ENGINE_STATE.read() {
            if let Some(ctx) = guard.as_ref() {
                // 克隆两份引用
                let ctx_for_rt = Arc::clone(ctx);
                let ctx_for_task = Arc::clone(ctx);

                // 1. 使用 ctx_for_rt 来调用 spawn
                // 2. 将 ctx_for_task 移动 (move) 进异步块
                ctx_for_rt.rt.spawn(async move {
                    let mut pm = ctx_for_task.playlist_manager.clone();
                    match pm.next_song().await {
                        Ok(_) => info!("切歌指令发送成功"),
                        Err(e) => log::error!("切歌指令发送失败: {}", e),
                    }
                });
            }
        }
    }
}
