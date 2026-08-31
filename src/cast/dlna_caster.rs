use async_trait::async_trait;
use std::net::IpAddr;

use crate::dlna_controller::{DlnaController, DlnaDevice};
use super::{Capabilities, CastError, Caster, Progress, Quality, SongRef};

pub struct DlnaCaster {
    controller: DlnaController,
    device: DlnaDevice,
    server_ip: IpAddr,
    server_port: u16,
}

impl DlnaCaster {
    pub fn new(
        controller: DlnaController,
        device: DlnaDevice,
        server_ip: IpAddr,
        server_port: u16,
    ) -> Self {
        Self { controller, device, server_ip, server_port }
    }
}

fn e(err: rupnp::Error) -> CastError {
    CastError::Device(err.to_string())
}

#[async_trait]
impl Caster for DlnaCaster {
    async fn play_song(&self, song: &SongRef) -> Result<(), CastError> {
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
            .map(|(curr, total)| Progress { current_secs: curr, total_secs: total })
            .map_err(e)
    }

    async fn set_volume(&self, volume: u32) -> Result<(), CastError> {
        self.controller
            .set_volume(&self.device, volume.clamp(0, 100))
            .await
            .map_err(e)
    }

    async fn get_volume(&self) -> Result<Option<u32>, CastError> {
        self.controller.get_volume(&self.device).await.map(Some).map_err(e)
    }

    async fn set_quality(&self, quality: Quality) -> Result<(), CastError> {
        if quality == Quality::P1080 && !crate::bilibili_session::has_valid_session() {
            return Err(CastError::Device("1080P 需要先扫码登录 B站".into()));
        }
        crate::set_dlna_quality(quality.as_qn()).ok_or(CastError::Unsupported)?;
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
