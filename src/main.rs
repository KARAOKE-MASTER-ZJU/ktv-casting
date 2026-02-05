use crate::dlna_controller::{DlnaController, DlnaDevice};
use actix_web::{App, HttpServer, web};
use anyhow::{Context, Result, bail};
use crossterm::event::{self};
use crossterm::terminal;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use local_ip_address::local_ip;
use log::{Log, Metadata, Record, info, warn};
use playlist_manager::PlaylistManager;
use reqwest::Client;
use std::fmt::Write;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;
use url::{Position, Url};

mod bilibili_parser;
mod dlna_controller;
mod media_server;
mod mp4_util;
mod playlist_manager;

// --- 结构定义 ---

pub struct SharedState {
    pub duration_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
}

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
            self.pb.suspend(|| {
                self.inner.log(record);
            });
        }
    }
    fn flush(&self) {
        self.inner.flush();
    }
}

// --- 主程序入口 ---

#[tokio::main]
async fn main() -> Result<()> {
    let pb = setup_env();

    let (base_url, room_id) = get_room_config_interactively()?;
    let controller = DlnaController::new();
    let device = select_dlna_device_interactively(&controller).await?;
    pb.set_draw_target(ProgressDrawTarget::stdout());
    let set_len = {
        let pb = pb.clone();
        Arc::new(move |len: u64| pb.set_length(len))
    };

    let set_pos = {
        let pb = pb.clone();
        Arc::new(move |pos: u64| pb.set_position(pos))
    };
    spawn_keyboard_handler(controller.clone(), device.clone());
    run_app(base_url, room_id, controller, device, set_len, set_pos).await
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

    let _ = terminal::enable_raw_mode();

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

    let _ = terminal::disable_raw_mode();
    Ok(())
}

// --- 辅助逻辑函数 ---

fn setup_env() -> ProgressBar {
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "INFO");
        }
    }
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let pb = ProgressBar::new(0);
    setup_pb_style(&pb);

    let env_log = env_logger::Builder::from_default_env().build();
    let _ = log::set_boxed_logger(Box::new(ProgressLogger {
        inner: Box::new(env_log),
        pb: pb.clone(),
    }))
    .map(|()| log::set_max_level(log::LevelFilter::Debug));
    pb
}

fn get_room_config_interactively() -> Result<(String, u64)> {
    println!("=== KTV投屏DLNA应用启动 ===");
    println!("输入房间链接:");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let url_str = input.trim();

    let mut normalized = url_str.to_string();
    if !normalized.contains("://") && !normalized.is_empty() {
        normalized = format!("http://{}", normalized);
    }

    let parsed = Url::parse(&normalized).with_context(|| "无法解析 URL")?;
    let base_url = parsed[..Position::AfterPort].to_string();

    let room_str = parsed
        .query_pairs()
        .find(|(key, _)| key == "roomId")
        .map(|(_, v)| v.into_owned())
        .or_else(|| {
            parsed
                .path_segments()?
                .filter(|s| !s.is_empty())
                .last()
                .map(|s| s.to_string())
        })
        .with_context(|| "URL 中未找到房间号")?;

    let room_id = room_str.parse::<u64>()?;
    Ok((base_url, room_id))
}

async fn select_dlna_device_interactively(controller: &DlnaController) -> Result<DlnaDevice> {
    let devices = controller.discover_devices().await?;
    if devices.is_empty() {
        bail!("未发现任何 DLNA 设备");
    }

    println!("发现以下设备：");
    for (i, d) in devices.iter().enumerate() {
        println!("{}: {} at {}", i, d.friendly_name, d.location);
    }
    println!("输入设备编号：");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let idx: usize = input.trim().parse()?;
    devices.get(idx).cloned().context("编号无效")
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

fn spawn_keyboard_handler(controller: DlnaController, device: DlnaDevice) {
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        let mut last_act = std::time::Instant::now();
        let mut paused = false;
        loop {
            if let Ok(event::Event::Key(key)) = event::read() {
                if key.code == event::KeyCode::Char('c')
                    && key.modifiers.contains(event::KeyModifiers::CONTROL)
                {
                    let _ = terminal::disable_raw_mode();
                    std::process::exit(0);
                }
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                if key.code == event::KeyCode::Char('p')
                    && key.modifiers.contains(event::KeyModifiers::CONTROL)
                {
                    if last_act.elapsed() < Duration::from_millis(500) {
                        continue;
                    }
                    rt.block_on(async {
                        let res = if paused {
                            controller.play(&device).await
                        } else {
                            controller.pause(&device).await
                        };
                        if res.is_ok() {
                            paused = !paused;
                            last_act = std::time::Instant::now();
                            info!(
                                "{}",
                                if paused {
                                    "⏸ 已暂停"
                                } else {
                                    "▶ 已恢复播放"
                                }
                            );
                        }
                    });
                }
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

fn setup_pb_style(pb: &ProgressBar) {
    pb.set_style(
        ProgressStyle::with_template("{bar:40.green/blue} {my_pos} / {my_len}")
            .unwrap()
            .with_key("my_pos", |s: &ProgressState, w: &mut dyn Write| {
                write!(w, "{:02}:{:02}", s.pos() / 60, s.pos() % 60).unwrap()
            })
            .with_key("my_len", |s: &ProgressState, w: &mut dyn Write| {
                write!(
                    w,
                    "{:02}:{:02}",
                    s.len().unwrap_or(0) / 60,
                    s.len().unwrap_or(0) % 60
                )
                .unwrap()
            })
            .progress_chars("━━━"),
    );
    pb.set_draw_target(ProgressDrawTarget::hidden());
}
