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
        }
    }

    async fn reload_song_at(
        &self,
        song: &str,
        position: u32,
        reason: &str,
    ) -> Result<(), CastError> {
        let quality = crate::get_dlna_quality();
        let media_url = if quality == Quality::P1080 {
            with_start_offset(song, position)
        } else {
            song.to_owned()
        };
        let _ = self.controller.stop(&self.device).await;
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
        self.controller.play(&self.device).await.map_err(e)?;

        if quality == Quality::P720 && position > 0 {
            if let Err(error) = self.controller.seek(&self.device, position).await {
                log::warn!(target: "DLNA1080", "720P 重载后恢复进度失败: position={}s, error={}", position, error);
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

#[async_trait]
impl Caster for DlnaCaster {
    async fn play_song(&self, song: &SongRef) -> Result<(), CastError> {
        if let Ok(mut current) = self.current_song.lock() {
            *current = Some(song.0.clone());
        }
        let _ = self.controller.stop(&self.device).await;
        self.controller
            .set_avtransport_uri(&self.device, &song.0, "", self.server_ip, self.server_port)
            .await
            .map_err(e)?;
        self.controller.play(&self.device).await.map_err(e)
    }

    async fn resume(&self) -> Result<(), CastError> {
        self.controller.play(&self.device).await.map_err(e)
    }

    async fn pause(&self) -> Result<(), CastError> {
        self.controller.pause(&self.device).await.map_err(e)
    }

    async fn stop(&self) -> Result<(), CastError> {
        self.controller.stop(&self.device).await.map_err(e)
    }

    async fn seek(&self, secs: u32) -> Result<(), CastError> {
        if crate::get_dlna_quality() == Quality::P1080 {
            let song = self
                .current_song
                .lock()
                .ok()
                .and_then(|current| current.clone())
                .ok_or_else(|| CastError::Device("DLNA 尚未加载歌曲，无法定位".to_string()))?;
            log::info!(target: "DLNA1080", "1080P 定位请求: {}s；通过 start 参数重开混流", secs);
            return self.reload_song_at(&song, secs, "1080P 定位").await;
        }
        self.controller.seek(&self.device, secs).await.map_err(e)
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
