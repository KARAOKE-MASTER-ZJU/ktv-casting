use crate::dlna_controller::{DlnaController, DlnaDevice};
use actix_web::{App, HttpServer, web};
use anyhow::Result;
use local_ip_address::list_afinet_netifas;
use log::{error, info, warn};
use playlist_manager::PlaylistManager;
use reqwest::Client;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

pub mod bilibili_parser;
pub mod dlna_controller;
pub mod media_server;
pub mod mp4_util;
pub mod playlist_manager;

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
    let target_ip = device
        .location
        .split('/')
        .nth(2)
        .and_then(|host_port| host_port.split(':').next())
        .unwrap_or("127.0.0.1");
    let local_ip_str = get_best_local_ip(target_ip);
    info!("确定投屏回调 IP: {}", local_ip_str);

    // 解析为 IpAddr 类型供后续使用
    let local_ip: std::net::IpAddr = local_ip_str.parse()?;

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
    .workers(1)
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

pub fn get_best_local_ip(target_device_ip: &str) -> String {
    let interfaces = list_afinet_netifas().unwrap_or_default();

    // 将目标 IP 转为 u32 二进制，如果解析失败则回退
    let target_u32 = target_device_ip
        .parse::<Ipv4Addr>()
        .map(|ip| u32::from(ip))
        .ok();

    if let Some(target) = target_u32 {
        let best_match = interfaces
            .iter()
            .filter_map(|(name, ip)| {
                if let IpAddr::V4(v4) = ip {
                    let candidate = u32::from(*v4);
                    // 异或运算：相同位为0，不同位为1
                    // 领先的 0 越多，说明从左往右匹配的位数越多（子网前缀越长）
                    let match_bits = (target ^ candidate).leading_zeros();
                    Some((match_bits, ip.to_string(), name))
                } else {
                    None
                }
            })
            // 找出匹配位数最多的那个
            .max_by_key(|(bits, _, _)| *bits);

        if let Some((bits, ip_str, name)) = best_match {
            info!(
                "二进制匹配成功: 网卡 {} (匹配位: {}), IP: {}",
                name, bits, ip_str
            );
            return ip_str;
        }
    }

    // 彻底失败后的兜底
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
    use std::sync::Arc; // 用于解析 Uri

    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_initLogging(
        _env: JNIEnv,
        _class: JClass,
        level: jint, // 0: Error, 1: Warn, 2: Info, 3: Debug
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
        info!("Logger initialized at level: {:?}", log_level);
    }
    // 搜索接口
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_searchDevices(
        mut env: JNIEnv,
        _class: JClass,
    ) -> jobjectArray {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dlna_devices = rt.block_on(async {
            let controller = DlnaController::new();
            controller.discover_devices().await.unwrap_or_default()
        });

        let cls = env
            .find_class("zju/bangdream/ktv/casting/DlnaDeviceItem")
            .expect("找不到类定义");

        // 3. 修复：new_object_array 必须传入 JObject::null() 而非 std::ptr::null_mut()
        let array = env
            .new_object_array(dlna_devices.len() as jsize, &cls, JObject::null())
            .unwrap();

        for (i, d) in dlna_devices.iter().enumerate() {
            let name = env.new_string(&d.friendly_name).unwrap();
            let loc = env.new_string(&d.location).unwrap();

            let item_obj = env
                .new_object(
                    &cls,
                    "(Ljava/lang/String;Ljava/lang/String;)V",
                    &[(&name).into(), (&loc).into()],
                )
                .unwrap();

            env.set_object_array_element(&array, i as jsize, item_obj)
                .unwrap();
        }
        array.into_raw()
    }

    // 启动接口
    #[unsafe(no_mangle)]
    pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_startApp(
        mut env: JNIEnv,
        _class: JClass,
        base_url: JString,
        room_id: jlong,
        target_location: JString,
    ) {
        info!("KTV-DEBUG: startApp called from JNI!");
        let base_url: String = env.get_string(&base_url).unwrap().into();
        let room_id = room_id as u64;
        let target_loc: String = env.get_string(&target_location).unwrap().into();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let controller = DlnaController::new();
                info!("正在连接用户指定的设备: {}", target_loc);

                if let Ok(uri) = target_loc.parse() {
                    if let Ok(device_obj) = rupnp::Device::from_url(uri).await {
                        let dlna_device = DlnaDevice {
                            friendly_name: device_obj.friendly_name().to_string(),
                            location: target_loc.clone(),
                            device: device_obj,
                            services: vec![],
                        };

                        let _ = run_app(
                            base_url,
                            room_id,
                            controller,
                            dlna_device,
                            Arc::new(|_| {}),
                            Arc::new(|_| {}),
                        )
                        .await;
                    } else {
                        error!("无法连接到 DLNA 设备 URL: {}", target_loc);
                    }
                } else {
                    error!("解析设备 URL 失败: {}", target_loc);
                }
            });
        });
    }
}
