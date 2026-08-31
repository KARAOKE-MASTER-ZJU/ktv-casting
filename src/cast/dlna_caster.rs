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
            let position = self
                .controller
                .get_secs(&self.device)
                .await
                .ok()
                .map(|progress| progress.0)
                .unwrap_or(0);
            let _ = self.controller.stop(&self.device).await;
            self.controller
                .set_avtransport_uri(&self.device, &song, "", self.server_ip, self.server_port)
                .await
                .map_err(e)?;
            self.controller.play(&self.device).await.map_err(e)?;
            if position > 0 {
                if let Err(error) = self.controller.seek(&self.device, position).await {
                    log::warn!(target: "DLNA1080", "清晰度切换后恢复进度失败: position={}s, error={}", position, error);
                }
            }
            log::info!(target: "DLNA1080", "DLNA 清晰度切换已重载媒体: quality={}, resume={}s", quality.label(), position);
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
