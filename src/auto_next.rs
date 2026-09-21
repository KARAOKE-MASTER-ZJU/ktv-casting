use crate::playlist_manager::{PlaylistManager, SwitchSongError};
use std::time::Duration;
use tokio::task::JoinHandle;

const END_MARGIN_SECS: i32 = 2;
const REWIND_SECS: i32 = 3;

/// An end-of-song trigger stays consumed until valid progress rewinds.
/// Playlist/hash updates and request completion never rearm it.
#[derive(Default)]
pub(crate) struct AutoNextSong {
    fired: bool,
    last_current: Option<i32>,
    retry_task: Option<JoinHandle<()>>,
}

impl AutoNextSong {
    pub(crate) fn observe(&mut self, current: i32, total: i32) -> bool {
        // Zero can be a transient loading response, even with a known duration.
        // Wait for positive progress before treating a rewind as new playback.
        if current <= 0 || total <= 0 {
            return false;
        }
        let near_end = total.saturating_sub(current) <= END_MARGIN_SECS;
        if !near_end
            && self
                .last_current
                .is_some_and(|last| last - current >= REWIND_SECS)
        {
            self.fired = false;
            // Retrying an old end event is no longer useful after playback rewinds.
            if let Some(task) = self.retry_task.take() {
                task.abort();
            }
        }
        self.last_current = Some(current);
        if current > 5 && near_end && !self.fired {
            self.fired = true;
            return true;
        }
        false
    }

    pub(crate) fn start_request(
        &mut self,
        manager: PlaylistManager,
        hash: String,
        runtime: &tokio::runtime::Handle,
    ) {
        self.retry_task = Some(runtime.spawn(retry_next_song(manager, hash)));
    }
}

impl Drop for AutoNextSong {
    fn drop(&mut self) {
        if let Some(task) = self.retry_task.take() {
            task.abort();
        }
    }
}

async fn retry_next_song(manager: PlaylistManager, hash: String) {
    loop {
        match manager.switch_song_with_hash(true, &hash).await {
            Ok(()) => return,
            Err(SwitchSongError::Rejected(error)) => {
                // A lost success response may be followed by REJECT on retry.
                // Leave playlist synchronization to the existing WS/polling loop.
                log::info!("自动切歌请求已结束，等待歌单同步: {}", error);
                return;
            }
            Err(SwitchSongError::Retryable(error)) => {
                log::warn!("自动切歌暂时失败，1 秒后使用原 hash 重试: {}", error);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_end_progress_only_fires_once_even_with_a_new_duration() {
        let mut trigger = AutoNextSong::default();
        assert!(!trigger.observe(290, 300));
        assert!(trigger.observe(298, 300));
        for (current, total) in [(299, 300), (300, 300), (301, 180), (301, 600), (302, 180)] {
            assert!(!trigger.observe(current, total));
        }
    }

    #[test]
    fn valid_rewind_rearms_the_next_end_event() {
        let mut trigger = AutoNextSong::default();
        assert!(trigger.observe(298, 300));
        assert!(!trigger.observe(1, 180));
        assert!(!trigger.observe(2, 180));
        assert!(trigger.observe(178, 180));
        assert!(!trigger.observe(180, 180));
    }

    #[test]
    fn errors_loading_and_small_jitter_do_not_rearm() {
        let mut trigger = AutoNextSong::default();
        assert!(trigger.observe(298, 300));
        for (current, total) in [(-1, -1), (0, 0), (0, 300), (1, 0), (297, 300), (298, 300)] {
            assert!(!trigger.observe(current, total));
        }
    }

    #[test]
    fn rewind_still_inside_end_zone_does_not_rearm() {
        let mut trigger = AutoNextSong::default();
        assert!(trigger.observe(305, 300));
        assert!(!trigger.observe(299, 300));
        assert!(!trigger.observe(300, 300));
    }

    #[test]
    fn polling_past_exact_end_still_triggers_once() {
        let mut trigger = AutoNextSong::default();
        assert!(!trigger.observe(175, 180));
        assert!(trigger.observe(181, 180));
        assert!(!trigger.observe(182, 180));
    }
}
