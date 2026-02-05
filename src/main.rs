use anyhow::{Context, Result, bail};
use crossterm::event::{self};
use crossterm::terminal;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use ktv_casting::dlna_controller::{DlnaController, DlnaDevice};
use ktv_casting::run_app;
use log::{Log, Metadata, Record, info};
use std::fmt::Write;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use url::{Position, Url};

// --- 结构定义 ---

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
    let result = run_app(base_url, room_id, controller, device, set_len, set_pos).await;

    // 程序结束前强制关闭 Raw Mode
    let _ = terminal::disable_raw_mode();
    result
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

fn spawn_keyboard_handler(controller: DlnaController, device: DlnaDevice) {
    let _ = terminal::enable_raw_mode();
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
