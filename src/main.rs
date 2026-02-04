use crate::dlna_controller::DlnaController;
use actix_web::{App, HttpServer, web};
use anyhow::{Context, Result, bail};
use local_ip_address::local_ip;
use log::{error, info, warn, debug, Log, Metadata, Record};
use indicatif::{ProgressBar, ProgressStyle, ProgressDrawTarget,ProgressState};
use crossterm::event::{self};
use crossterm::terminal;
use std::fmt::Write;
use playlist_manager::PlaylistManager;
use reqwest::Client;
use std::io;
use std::sync::Arc;
use std::time::{Duration};
use tokio::sync::Mutex;
use tokio::time::sleep;
use url::{Position, Url};

mod bilibili_parser;
mod dlna_controller;
mod media_server;
mod mp4_util;
mod playlist_manager;

/// 包装日志器：在输出其他日志前暂停/恢复进度条，确保进度条始终置底且不被日志打断
struct ProgressLogger {
    inner: Box<dyn Log>,
    pb: ProgressBar,
}

impl Log for ProgressLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            // 在输出日志前暂停进度条，避免日志内容插入进度条下方
            self.pb.suspend(|| {
                self.inner.log(record);
            });
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

pub struct SharedState {
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "INFO");
        }
    }
    let pb = ProgressBar::new(0);
    pb.set_draw_target(ProgressDrawTarget::hidden());
    let env_log = env_logger::Builder::from_default_env().build();
     // 注册logger box接管日志
    let custom_logger = ProgressLogger {
        inner: Box::new(env_log),
        pb: pb.clone(),
    };
    let max_level = log::LevelFilter::Debug;
    log::set_boxed_logger(Box::new(custom_logger))
        .map(|()| log::set_max_level(max_level))
        .expect("Logger 注册失败");

    println!("=== KTV投屏DLNA应用启动 ===");
    println!("输入房间链接，如 http://127.0.0.1:1145/102 或 https://ktv.example.com/102");
    let mut input = String::new();
    io::stdin().read_line(&mut input).expect("无法读取输入");
    let url_str = input.trim();
    let mut normalized_url = url_str.to_string();
    if !normalized_url.contains("://") && !normalized_url.is_empty() {
        normalized_url = format!("http://{}", normalized_url);
    }
    // ② 使用 url crate 解析并提取 base URL 与 room_id
    let parsed_url = Url::parse(&normalized_url).with_context(|| "无法解析 URL")?;

    let base_url = parsed_url[..Position::AfterPort].to_string();
    info!("Base URL: {}", base_url);


    // 尝试从不同的地方获取 room_id
    let room_id_result: Option<String> = {
        // 优先尝试从查询参数 ?roomId=123 中获取
        let query_room_id = parsed_url.query_pairs()
            .find(|(key, _)| key == "roomId")
            .map(|(_, value)| value.into_owned());

        if query_room_id.is_some() {
            query_room_id
        } else {
            // 如果 Query 里没有，回退到原来的路径末尾逻辑
            parsed_url.path_segments()
                .and_then(|s| s.filter(|seg| !seg.is_empty()).last())
                .map(|s| s.to_string())
        }
    };
    // 检查结果
    let room_str = room_id_result.with_context(|| "URL 中未找到房间号 (roomId 参数或路径末尾)")?;

    // 解析为 u64
    let room_id: u64 = room_str
        .parse::<u64>()
        .with_context(|| format!("房间号解析失败: '{}'", room_str))?;

    info!("Parsed room_id: {}", room_id);

    let server_port = 8080;
    let playlist: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let mut playlist_manager = PlaylistManager::new(&base_url, room_id, playlist.clone());

    let duration_cache = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let shared_state = web::Data::new(SharedState {
        duration_cache: duration_cache.clone(),
    });

    // 1. 创建 Reqwest Client
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .expect("Failed to create client");

    let client_data = web::Data::new(client);

    // 2. 配置 HttpServer，运行
    let server = HttpServer::new(move || {
        App::new()
            .app_data(client_data.clone())
            .app_data(shared_state.clone())
            .service(media_server::proxy_handler)
    })
    .bind(("0.0.0.0", server_port))?
    .run();

    let local_ip = local_ip()?;
    let controller = DlnaController::new();
    let devices = controller.discover_devices().await?;
    if devices.is_empty() {
        bail!("No DLNA Devices");
    }
    println!("发现以下DLNA设备：");
    println!("编号: 设备名称 at 设备地址");
    for (i, device) in devices.iter().enumerate() {
        println!("{}: {} at {}", i, device.friendly_name, device.location);
    }
    println!("输入设备编号：");
    input.clear();
    io::stdin().read_line(&mut input).expect("读取编号失败");
    let device_num: usize = input.trim().parse()?;
    if device_num > devices.len() {
        bail!("编号有误");
    }
    let device = devices[device_num].clone(); // clone owned copy
    let device_cloned = device.clone();

    // 现在显示并启用进度条
    
    // 初始化 env_logger 并创建 ProgressBar，播放进度由轮询处直接更新
    
    pb.set_style(
        ProgressStyle::with_template("{bar:40.green/blue} {my_pos} / {my_len}")
            .expect("invalid style")
            .with_key("my_pos", |state: &ProgressState, w: &mut dyn Write| {
                write!(w, "{:02}:{:02}", state.pos() / 60, state.pos() % 60).unwrap()
            })
            .with_key("my_len", |state: &ProgressState, w: &mut dyn Write| {
                write!(w, "{:02}:{:02}", state.len().unwrap_or(0) / 60, state.len().unwrap_or(0) % 60).unwrap()
            })
            .progress_chars("━━━")
    );

   

    pb.set_draw_target(ProgressDrawTarget::stdout());

    let _ = terminal::enable_raw_mode();
    let controller_for_update = controller.clone();

    //同步播放列表更新到DLNA设备
    playlist_manager.start_sync(move |url| {
        let controller = controller_for_update.clone();
        let device = device.clone();
        Box::pin(async move {

            // 重试直到stop成功
            loop {
                match controller.stop(&device).await {
                    Ok(_) => {
                        info!("成功停止播放");
                        break;
                    }
                    Err(e) => {
                        let error_msg = format!("{}", e);
                        let error_code: Option<u32> = error_msg
                            .split(|c: char| !c.is_numeric())
                            .find(|s| s.len() == 3)
                            .and_then(|s| s.parse().ok());
                        if let Some(code) = error_code
                            && code / 100 == 2
                        {
                            // 2xx错误码视为成功
                            info!("停止播放返回错误码{}，视为成功", code);
                            break;
                        }

                        warn!("停止播放失败: {}，500ms后重试", error_msg);
                        sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            
            // 重试设置AVTransport URI
            loop {
                match controller
                    .set_avtransport_uri(&device, &url, "", local_ip, server_port)
                    .await
                {
                    Ok(_) => {
                        info!("成功设置AVTransport URI为 {}", url);
                        break;
                    }
                    Err(e) => {
                        let error_msg = format!("{}", e);
                        let error_code: Option<u32> = error_msg
                            .split(|c: char| !c.is_numeric())
                            .find(|s| s.len() == 3)
                            .and_then(|s| s.parse().ok());
                        if let Some(code) = error_code
                            && code / 100 == 2
                        {
                            // 2xx错误码视为成功
                            info!("设置AVTransport URI返回错误码{}，视为成功", code);
                            break;
                        }

                        warn!("设置AVTransport URI失败: {}，500ms后重试", error_msg);
                        sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            // // 重试设置AVTransport URI
            // loop {
            //     match controller
            //         .set_next_avtransport_uri(&device, &url, "", local_ip, server_port)
            //         .await
            //     {
            //         Ok(_) => break,
            //         Err(e) => {
            //             let error_msg = format!("{}", e);
            //             let error_code: Option<u32> = error_msg
            //                 .split(|c: char| !c.is_numeric())
            //                 .find(|s| s.len() == 3)
            //                 .and_then(|s| s.parse().ok());
            //             if let Some(code) = error_code {
            //                 if code / 100 == 2 {
            //                     // 2xx错误码视为成功
            //                     info!("设置AVTransport URI返回错误码{}，视为成功", code);
            //                     break
            //                 }
            //             }
            //             warn!("设置AVTransport URI失败: {}，500ms后重试", error_msg);
            //             sleep(Duration::from_millis(500)).await;
            //         }
            //     }
            // }

            // // 重试next
            // loop {
            //     match controller.next(&device).await {
            //         Ok(_) => break,
            //         Err(e) => {
            //             let error_msg = format!("{}", e);
            //             let error_code: Option<u32> = error_msg
            //                 .split(|c: char| !c.is_numeric())
            //                 .find(|s| s.len() == 3)
            //                 .and_then(|s| s.parse().ok());
            //             if let Some(code) = error_code {
            //                 if code / 100 == 2 {
            //                     // 2xx错误码视为成功
            //                     info!("设置AVTransport URI返回错误码{}，视为成功", code);
            //                     break;
            //                 }
            //             }
            //             warn!("next失败: {}，500ms后重试", error_msg);
            //             sleep(Duration::from_millis(500)).await;
            //         }
            //     }
            // }

            // 重试play
            loop {
                match controller.play(&device).await {
                    Ok(_) => {
                        info!("成功开始播放");
                        break;
                    }
                    Err(e) => {
                        let error_msg = format!("{}", e);
                        let error_code: Option<u32> = error_msg
                            .split(|c: char| !c.is_numeric())
                            .find(|s| s.len() == 3)
                            .and_then(|s| s.parse().ok());
                        if let Some(code) = error_code
                            && code / 100 == 2
                        {
                            // 2xx错误码视为成功
                            info!("播放返回错误码{}，视为成功", code);
                            break;
                        }

                        warn!("play失败: {}，500ms后重试", error_msg);
                        sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        })
    });

    let device_for_poll = device_cloned.clone();

    // 轮询播放进度，更新进度条，自动切歌
    tokio::spawn(async move {
        let controller = DlnaController::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut current_secs: u32 = 0;
        let mut total_secs: u32 = 0;
        loop {
            interval.tick().await;

            // 首先尝试从缓存中获取总长度
            let mut cached_total = 0;
            if let Some(playing) = playlist_manager.get_song_playing().await {
                let cache = duration_cache.lock().await;
                if let Some(&d) = cache.get(&playing) {
                    cached_total = d;
                }
            }

            // 重试get_secs
            loop {
                match controller.get_secs(&device_for_poll).await {
                    Ok(result) => {
                        (current_secs, _) = result;

                        // 如果从缓存拿到了长度，
                        if cached_total > 0 {
                            total_secs = cached_total;
                            debug!("使用缓存的视频时长: {}s", total_secs);
                        }

                        let remaining_secs = if total_secs > current_secs {
                            total_secs - current_secs
                        } else {
                            0
                        };

                        // 使用 ProgressBar 显示播放进度（代替日志）
                        if total_secs > 0 {
                            pb.set_length(total_secs as u64);
                        }
                        pb.set_position(current_secs as u64);

                        if remaining_secs <= 2 && total_secs > 0 {
                            info!(
                                "剩余时间{}秒，总时间{}秒，准备切歌",
                                remaining_secs, total_secs
                            );
                            // 重试next_song
                            loop {
                                match playlist_manager.next_song().await {
                                    Ok(_) => break,
                                    Err(e) => {
                                        let error_msg = e.to_string();
                                        error!("next_song失败: {}，500ms后重试", error_msg);
                                        sleep(Duration::from_millis(500)).await;
                                    }
                                }
                            }
                            sleep(Duration::from_secs(5)).await;
                        }
                        break;
                    }
                    Err(e) => {
                        let error_msg = format!("{}", e);
                        let error_code: Option<u32> = error_msg
                            .split(|c: char| !c.is_numeric())
                            .find(|s| s.len() == 3)
                            .and_then(|s| s.parse().ok());
                        if let Some(code) = error_code
                            && code / 100 == 2
                        {
                            // 2xx错误码视为成功
                            info!("获取进度返回错误码{}，视为成功", code);
                            break;
                        }

                        warn!("get_secs失败: {}，500ms后重试", error_msg);
                        sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    });
    
    let controller_kb = controller.clone(); 
    let device_kb = device_cloned.clone();
    /*
        监听键盘输入，处理 Ctrl + P 播放/暂停 和 Ctrl + C 退出
        由于 crossterm 的事件读取是阻塞的，因此使用 spawn_blocking
    */
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        let mut last_action_time = std::time::Instant::now();
        let debounce_duration = std::time::Duration::from_millis(500);
        let mut paused_lock = false;

        loop {
            if let Ok(event::Event::Key(key)) = event::read() {
                
                // 处理 Ctrl + C 退出
                if key.code == event::KeyCode::Char('c') && key.modifiers.contains(event::KeyModifiers::CONTROL) {
                    let _ = terminal::disable_raw_mode();
                    log::info!("检测到 Ctrl+C，正在退出...");
                    std::process::exit(0);
                }

                // 过滤掉按键释放事件
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }

                // 处理 Ctrl + P 播放/暂停
                if key.code == event::KeyCode::Char('p') && key.modifiers.contains(event::KeyModifiers::CONTROL) {
                    
                    if last_action_time.elapsed() < debounce_duration {
                        continue; 
                    }

                    // 使用 block_on 回到异步环境执行具体的网络请求
                    rt.block_on(async {
                        if paused_lock {
                            log::info!("检测到 Ctrl+P: 尝试恢复播放...");
                            if let Ok(_) = controller_kb.play(&device_kb).await {
                                paused_lock = false;
                                last_action_time = std::time::Instant::now();
                                log::info!("▶ 已恢复播放");
                            }
                        } else {
                            log::info!("检测到 Ctrl+P: 尝试暂停播放...");
                            if let Ok(_) = controller_kb.pause(&device_kb).await {
                                paused_lock = true;
                                last_action_time = std::time::Instant::now();
                                log::info!("⏸ 已暂停");
                            }
                        }
                    });
                }
            }
        }
    });

    server.await?;

    println!("应用已退出");
    Ok(())
}
