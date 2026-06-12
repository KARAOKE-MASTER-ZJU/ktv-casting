pub mod dlna_caster;
pub mod progress;

use async_trait::async_trait;
use std::fmt;

#[derive(Debug, Clone)]
pub struct SongRef(pub String);

#[derive(Debug, Clone, Copy, Default)]
pub struct Progress {
    pub current_secs: u32,
    pub total_secs: u32,
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub absolute_volume: bool,
    pub seek: bool,
    pub hardware_progress: bool,
}

#[derive(Debug)]
pub enum CastError {
    Unsupported,
    Device(String),
}

impl fmt::Display for CastError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CastError::Unsupported => write!(f, "not supported"),
            CastError::Device(s) => write!(f, "device error: {}", s),
        }
    }
}

impl std::error::Error for CastError {}

#[async_trait]
pub trait Caster: Send + Sync {
    async fn play_song(&self, song: &SongRef) -> Result<(), CastError>;
    async fn resume(&self) -> Result<(), CastError>;
    async fn pause(&self) -> Result<(), CastError>;
    async fn stop(&self) -> Result<(), CastError>;
    async fn seek(&self, secs: u32) -> Result<(), CastError>;
    async fn get_progress(&self) -> Result<Progress, CastError>;
    async fn set_volume(&self, volume: u32) -> Result<(), CastError>;
    async fn get_volume(&self) -> Result<Option<u32>, CastError>;
    fn capabilities(&self) -> Capabilities;
}
