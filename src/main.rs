#![cfg(feature = "cli")]
use anyhow::{Context, Result, bail};
use crossterm::event::{self};
use crossterm::terminal;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use ktv_casting_lib::dlna_controller::{DlnaController, DlnaDevice};
use ktv_casting_lib::{ENGINE_STATE, start_engine_core, toggle_pause_core, trigger_next_song};
use log::{Log, Metadata, Record, info};
use std::fmt::Write;
use std::io;
use std::time::Duration;
use url::{Position, Url};

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

#[tokio::main]
async fn main() -> Result<()> {
    let pb = setup_env();

    // 1. 交互式获取配置
    let (base_url, room_id) = get_room_config_interactively()?;
    let controller = DlnaController::new();
    let device = select_dlna_device_interactively(&controller).await?;

    // 2. 准备 Runtime 传给引擎
    let engine_rt = tokio::runtime::Runtime::new().context("Failed to create engine runtime")?;

    // 3. 启动引擎逻辑 (调用 lib.rs 中的异步函数)
    start_engine_core(base_url, room_id, device.location.clone(), engine_rt).await;

    pb.set_draw_target(ProgressDrawTarget::stdout());

    // 4. 键盘监听处理
    spawn_keyboard_handler();

    // 5. 调用封装好的监控函数，传入回调更新进度条
    let pb_for_len = pb.clone();
    let pb_for_pos = pb.clone();

    // 这是一个异步死循环，会在这里持续运行直到引擎关闭或报错
    run_cli_monitor(
        move |total| pb_for_len.set_length(total),
        move |curr| pb_for_pos.set_position(curr),
    )
    .await?;

    Ok(())
}

// --- 辅助逻辑函数 ---
async fn select_dlna_device_interactively(controller: &DlnaController) -> Result<DlnaDevice> {
    let devices = controller.discover_devices().await.unwrap_or_default();
    if devices.is_empty() {
        bail!("未发现任何 DLNA 设备");
    }

    println!("发现以下设备：");
    for (i, d) in devices.iter().enumerate() {
        println!("{}: {} at {}", i, d.friendly_name, d.location);
    }
    print!("输入设备编号：");
    io::Write::flush(&mut io::stdout())?; // 确保提示文字先打印
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let idx: usize = input.trim().parse()?;
    devices.get(idx).cloned().context("编号无效")
}

fn spawn_keyboard_handler() {
    let _ = terminal::enable_raw_mode();
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
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

                rt.block_on(async {
                    match key.code {
                        event::KeyCode::Char('p') => {
                            if let Ok(state) = toggle_pause_core().await {
                                info!(
                                    "{}",
                                    if state {
                                        "▶ 已恢复播放"
                                    } else {
                                        "⏸ 已暂停"
                                    }
                                );
                            }
                        }
                        event::KeyCode::Char('n') => {
                            trigger_next_song();
                            info!("⏭ 切歌");
                        }
                        _ => {}
                    }
                });
            }
        }
    });
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
fn setup_env() -> ProgressBar {
    if std::env::var("RUST_LOG").is_err() {
        unsafe {
            std::env::set_var("RUST_LOG", "INFO");
        }
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
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

/// 负责进度查询、自动切歌，并通过回调更新 UI
async fn run_cli_monitor<FL, FP>(mut set_len: FL, mut set_pos: FP) -> anyhow::Result<()>
where
    FL: FnMut(u64),
    FP: FnMut(u64),
{
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let guard = ENGINE_STATE.read().unwrap();
        if let Some(ctx) = guard.as_ref() {
            // 1. 查询 DLNA 进度
            if let Ok((curr, _)) = ctx.controller.get_secs(&ctx.device).await {
                let curr_u64 = curr as u64;

                // 2. 获取总时长
                if let Some(playing) = ctx.playlist_manager.get_song_playing().await {
                    let cache = ctx.duration_cache.lock().await;
                    if let Some(&total) = cache.get(&playing) {
                        let total_u64 = total as u64;

                        set_len(total_u64);
                        set_pos(curr_u64);

                        // 3. 自动切歌逻辑
                        if total_u64 > 0
                            && curr_u64 > 5
                            && total_u64 > curr_u64
                            && (total_u64 - curr_u64) <= 2
                        {
                            log::info!(">> 歌曲即将结束，自动切换下一首...");
                            let mut pm = ctx.playlist_manager.clone();
                            let _ = pm.next_song().await;

                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        } else {
            break;
        }
    }
    Ok(())
}
