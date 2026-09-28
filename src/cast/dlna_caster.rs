use async_trait::async_trait;
use std::net::IpAddr;

use super::{Capabilities, CastError, Caster, Progress, Quality, SongRef};
use crate::dlna_controller::{DlnaController, DlnaDevice};

pub struct DlnaCaster {
    controller: DlnaController,
    device: DlnaDevice,
    server_ip: IpAddr,
    server_port: u16,
    current_song: std::sync::Mutex<Option<String>>,
    sessions: Option<std::sync::Arc<crate::media_session::MediaSessions>>,
    media_client: reqwest::Client,
    transport: tokio::sync::Mutex<()>,
    seek_sequence: std::sync::atomic::AtomicU64,
    loaded_generation: std::sync::atomic::AtomicU64,
}

impl DlnaCaster {
    pub fn new(
        controller: DlnaController,
        device: DlnaDevice,
        server_ip: IpAddr,
        server_port: u16,
    ) -> Self {
        Self {
            controller,
            device,
            server_ip,
            server_port,
            current_song: std::sync::Mutex::new(None),
            sessions: None,
            media_client: reqwest::Client::builder()
                .connect_timeout(crate::PROXY_CONNECT_TIMEOUT)
                .build()
                .expect("valid media client"),
            transport: tokio::sync::Mutex::new(()),
            seek_sequence: std::sync::atomic::AtomicU64::new(0),
            loaded_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn with_sessions(
        mut self,
        sessions: std::sync::Arc<crate::media_session::MediaSessions>,
    ) -> Self {
        self.sessions = Some(sessions);
        self
    }

    async fn reload_song_at(
        &self,
        song: &str,
        position: u32,
        reason: &str,
    ) -> Result<(), CastError> {
        let quality = crate::get_dlna_quality();
        let session = self
            .sessions
            .as_ref()
            .map(|sessions| sessions.activate(song, quality));
        let media_url = if let Some(session) = &session {
            session
                .prepare(&self.media_client)
                .await
                .map_err(|error| CastError::Device(error.to_string()))?;
            session.path()
        } else if quality == Quality::P1080 {
            with_start_offset(song, position)
        } else {
            song.to_owned()
        };
        let _transport = self.transport.lock().await;
        if session.as_ref().is_some_and(|s| s.is_stopped()) {
            return Err(CastError::Device("曲目准备已被新请求替代".into()));
        }
        let _ = self.controller.stop(&self.device).await;
        if session.as_ref().is_some_and(|s| s.is_stopped()) {
            return Err(CastError::Device("播放请求已过期".into()));
        }
        self.controller
            .set_avtransport_uri(
                &self.device,
                &media_url,
                "",
                self.server_ip,
                self.server_port,
            )
            .await
            .map_err(e)?;
        if session.as_ref().is_some_and(|s| s.is_stopped()) {
            return Err(CastError::Device("播放请求已过期".into()));
        }
        self.controller.play(&self.device).await.map_err(e)?;
        if let Some(session) = &session {
            self.loaded_generation
                .store(session.generation, std::sync::atomic::Ordering::Relaxed);
        }

        if (session.is_some() || quality == Quality::P720) && position > 0 {
            if let Err(error) = self.controller.seek(&self.device, position).await {
                log::warn!(target: "DLNA1080", "切换媒体后恢复进度失败: quality={}, position={}s, error={}", quality.label(), position, error);
                return Err(e(error));
            }
        }
        log::info!(
            target: "DLNA1080",
            "DLNA 已重载媒体: reason={}, quality={}, start={}s",
            reason,
            quality.label(),
            position
        );
        Ok(())
    }
}

fn with_start_offset(song: &str, position: u32) -> String {
    let (path, query) = song.split_once('?').unwrap_or((song, ""));
    let parameters: Vec<&str> = query
        .split('&')
        .filter(|parameter| !parameter.is_empty() && !parameter.starts_with("start="))
        .collect();
    let prefix = parameters.join("&");
    if prefix.is_empty() {
        format!("{path}?start={position}")
    } else {
        format!("{path}?{prefix}&start={position}")
    }
}

fn e(err: rupnp::Error) -> CastError {
    CastError::Device(err.to_string())
}

impl Drop for DlnaCaster {
    fn drop(&mut self) {
        if let Some(sessions) = &self.sessions {
            sessions.clear();
        }
    }
}

#[async_trait]
impl Caster for DlnaCaster {
    async fn play_song(&self, song: &SongRef) -> Result<(), CastError> {
        if let Ok(mut current) = self.current_song.lock() {
            *current = Some(song.0.clone());
        }
        self.seek_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.reload_song_at(&song.0, 0, "播放歌曲").await
    }

    async fn resume(&self) -> Result<(), CastError> {
        let _transport = self.transport.lock().await;
        self.controller.play(&self.device).await.map_err(e)
    }

    async fn pause(&self) -> Result<(), CastError> {
        let _transport = self.transport.lock().await;
        self.controller.pause(&self.device).await.map_err(e)
    }

    async fn stop(&self) -> Result<(), CastError> {
        self.seek_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _transport = self.transport.lock().await;
        self.controller.stop(&self.device).await.map_err(e)
    }

    async fn seek(&self, secs: u32) -> Result<(), CastError> {
        if self.sessions.is_none() && crate::get_dlna_quality() == Quality::P1080 {
            let song = self
                .current_song
                .lock()
                .ok()
                .and_then(|current| current.clone())
                .ok_or_else(|| CastError::Device("DLNA 尚未加载歌曲，无法定位".to_string()))?;
            log::info!(target: "DLNA1080", "1080P 定位请求: {}s；通过 start 参数重开混流", secs);
            return self.reload_song_at(&song, secs, "1080P 定位").await;
        }
        let sequence = self
            .seek_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let _transport = self.transport.lock().await;
        if sequence
            != self
                .seek_sequence
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(());
        }
        let mut old_streams = None;
        if let Some(sessions) = &self.sessions {
            if let Some(session) = sessions
                .get(
                    self.loaded_generation
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
            {
                old_streams = session.seek_stream_cutoff();
                session.warm_seek(secs);
            } else {
                return Err(CastError::Device("新曲目正在准备，尚不能定位".into()));
            }
        }
        let started = std::time::Instant::now();
        let result = self.controller.seek(&self.device, secs).await.map_err(e);
        if result.is_ok() {
            if let Some((cache, cutoff)) = old_streams {
                // Cancel only streams present before the accepted command.
                // A fast renderer may already have opened its new Range GET.
                cache.retire_streams(cutoff);
            }
        }
        log::info!(target: "DLNA1080", "原生 DLNA 定位指令返回: target={}s, command_ms={}, success={}（不代表画面已恢复）", secs, started.elapsed().as_millis(), result.is_ok());
        result
    }

    async fn get_progress(&self) -> Result<Progress, CastError> {
        self.controller
            .get_secs(&self.device)
            .await
            .map(|(curr, total)| Progress {
                current_secs: curr,
                total_secs: total,
            })
            .map_err(e)
    }

    async fn set_volume(&self, volume: u32) -> Result<(), CastError> {
        self.controller
            .set_volume(&self.device, volume.clamp(0, 100))
            .await
            .map_err(e)
    }

    async fn get_volume(&self) -> Result<Option<u32>, CastError> {
        self.controller
            .get_volume(&self.device)
            .await
            .map(Some)
            .map_err(e)
    }

    async fn set_quality(&self, quality: Quality) -> Result<(), CastError> {
        let previous = crate::get_dlna_quality();
        if previous == quality {
            return Ok(());
        }
        log::info!(target: "DLNA1080", "切换 DLNA 清晰度: {} -> {}", previous.label(), quality.label());
        let position = self
            .controller
            .get_secs(&self.device)
            .await
            .ok()
            .map(|progress| progress.0)
            .unwrap_or(0);
        crate::set_dlna_quality(quality).map_err(|message| {
            log::error!(target: "DLNA1080", "拒绝 DLNA 清晰度切换: {}", message);
            CastError::Device(message.to_string())
        })?;

        // Re-open the same proxy URL so the renderer immediately requests the
        // newly selected representation. Keep the old position when possible.
        let song = self
            .current_song
            .lock()
            .ok()
            .and_then(|current| current.clone());
        if let Some(song) = song {
            self.reload_song_at(&song, position, "清晰度切换").await?;
        } else {
            log::info!(target: "DLNA1080", "DLNA 清晰度已保存，将在下一首生效: quality={}", quality.label());
        }
        Ok(())
    }

    fn get_quality(&self) -> Result<Quality, CastError> {
        Ok(crate::get_dlna_quality())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            absolute_volume: true,
            seek: true,
            hardware_progress: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::with_start_offset;

    #[test]
    fn replaces_an_existing_start_offset() {
        assert_eq!(
            with_start_offset("BV1xx-page0?qn=80&start=12", 42),
            "BV1xx-page0?qn=80&start=42"
        );
    }
}
