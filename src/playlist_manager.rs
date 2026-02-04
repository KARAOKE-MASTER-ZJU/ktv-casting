use log::{debug, error, info};
use reqwest::Client;
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::{env, future::Future};
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::time::sleep;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

#[derive(Clone)]
pub struct PlaylistManager {
    url: String,
    room_id: u64,
    hash: Arc<Mutex<Option<String>>>,
    playlist: Arc<Mutex<Vec<String>>>,
    song_playing: Arc<Mutex<Option<String>>>,
}

impl PlaylistManager {
    pub fn new(url: &str, room_id: u64, playlist: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            url: url.to_string(),
            room_id,
            hash: Arc::new(Mutex::new(None)),
            playlist,
            song_playing: Arc::new(Mutex::new(None)),
        }
    }

    // 适配新的返回结构：
    // {
    //   changed: boolean,
    //   list: { queued: Song[]; singing: Song; sung: Song[] },
    //   hash: string
    // }
    // Song { id, title, url, addedBy? }
    async fn fetch_playlist(&mut self) -> Result<Option<String>, String> {
        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .map_err(|e| format!("创建HTTP客户端失败: {}", e))?;

        let hash_guard = self.hash.lock().await;
        let last_hash = hash_guard.clone().unwrap_or("EMPTY_LIST_HASH".to_string());
        drop(hash_guard); // 释放锁，避免长时间持有

        let url = format!(
            "{}/api/songListInfo?roomId={}&lastHash={}",
            self.url, self.room_id, last_hash
        );

        debug!("正在获取播放列表: {}", url);

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("发送请求失败: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("请求失败，状态码: {}", resp.status()));
        }

        let resp_json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析JSON失败: {}", e))?;
        let changed: bool = resp_json["changed"].as_bool().unwrap_or(false);

        if !changed {
            debug!("播放列表未改变，跳过更新");
            return Ok(self.song_playing.lock().await.clone());
        }

        // 获取新的 hash 值
        let new_hash = resp_json["hash"]
            .as_str()
            .unwrap_or("EMPTY_LIST_HASH")
            .to_string();

        let extract_bv_function = |url: &str| {
            // 提取 bilibili://video/ 后面的部分
            if let Some(start) = url.find("bilibili://video/") {
                let after_prefix = &url[start + "bilibili://video/".len()..];
                after_prefix.to_string().replace("?", "-").replace("=", "") // 替换问号和等号，避免DLNA设备不支持
            } else {
                url.to_string().replace("?", "-").replace("=", "")
            }
        };

        // 新结构：从 list.queued 中提取待播歌单 URL
        let urls: Vec<String> = if let Some(queued) = resp_json["list"]["queued"].as_array() {
            queued
                .iter()
                .filter_map(|song| song["url"].as_str())
                .map(extract_bv_function)
                .collect()
        } else {
            Vec::new()
        };

        // 当前正在演唱的歌曲：list.singing.url（回退到 list.sung 的最后一项如果 singing 缺失）
        let singing_url: Option<String> = resp_json["list"]["singing"]
            .as_object()
            .and_then(|_| resp_json["list"]["singing"]["url"].as_str().map(extract_bv_function))
            .or_else(|| {
                resp_json["list"]["sung"]
                    .as_array()
                    .and_then(|arr| arr.last())
                    .and_then(|last| last["url"].as_str())
                    .map(extract_bv_function)
            });

        info!("获取到 {} 个URL，新的hash: {}", urls.len(), new_hash);

        // 打印每个URL用于调试
        for (i, url) in urls.iter().enumerate() {
            debug!("  {}. {}", i + 1, url);
        }

        // 更新播放列表
        let mut playlist = self.playlist.lock().await;
        playlist.clear();
        playlist.extend(urls);
        drop(playlist); // 释放锁，避免长时间持有

        // 更新当前歌曲
        let mut song_playing = self.song_playing.lock().await;
        *song_playing = singing_url.clone();
        drop(song_playing);

        // 更新 hash 值
        let mut hash = self.hash.lock().await;
        *hash = Some(new_hash);
        drop(hash); // 释放锁

        Ok(singing_url)
    }

    // 根据环境变量切换同步驱动（WS / POLLING）
    // 环境变量：KTV_SYNC_MODE = "WS" 或 "POLLING"（不区分大小写），默认为 WS
    pub fn start_sync<F>(&self, f_on_update: F)
    where
        F: Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
    {
        let mode = env::var("KTV_SYNC_MODE").unwrap_or_else(|_| "WS".to_string());
        if mode.to_uppercase() != "POLLING" {
            self.start_ws_update(f_on_update);
        } else {
            self.start_periodic_update(f_on_update);
        }
    }

    fn start_ws_update<F>(&self, f_on_update: F)
    where
        F: Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
    {
        let mut self_clone = self.clone();
        tokio::spawn(async move {
            /*
                这是维护WebSocket连接的循环
                负责连接、重连
             */
            loop {
                // 构造 WS URL （将 http(s) -> ws(s)）
                let nickname = env::var("KTV_NICKNAME").unwrap_or_default();
                let mut ws_url = format!("{}/api/ws?roomId={}&nickname={}", self_clone.url.trim_end_matches('/'), self_clone.room_id, urlencoding::encode(&nickname));

                if let Ok(mut parsed) = Url::parse(&ws_url) {
                    let _ = match parsed.scheme() {
                        "https" => parsed.set_scheme("wss"),
                        "http" => parsed.set_scheme("ws"),
                        _ => Ok(()),
                    };
                    ws_url = parsed.to_string();
                }

                debug!("尝试连接 WS: {}", ws_url);

                let (ws_stream, _) = match connect_async(ws_url).await {
                    Ok(val) => val,
                    Err(e) => {
                        error!("连接 WebSocket 失败: {}", e);
                        continue;
                    }
                };
                
                info!("WebSocket connected for room {}", self_clone.room_id);
                match self_clone.fetch_playlist().await {
                    Ok(Some(url)) => {
                        info!("WS 连接成功，初始化播放: {}", url);
                        f_on_update(url).await;
                    }
                    Ok(None) => debug!("歌单目前为空，等待点歌..."),
                    Err(e) => error!("初始化拉取失败: {}", e),
                }
                let (mut write, mut read) = ws_stream.split();

                // 本地缓存当前正在播放的歌曲，用于判断是否需要触发投屏切换
                let mut song_playing_cached: Option<String> = self_clone.song_playing.lock().await.clone();

                /*
                    监听ws消息的循环
                 */
                while let Some(msg) = read.next().await {
                    let text = match msg {
                        Ok(Message::Text(txt)) => txt,
                        Ok(Message::Ping(p)) => {let _ = write.send(Message::Pong(p)).await; continue;},
                        Ok(Message::Close(_)) => {info!("WebSocket closed by server, reconnecting in 3s..."); break;},
                        Err(e) => {error!("WebSocket read 错误: {}", e); break;},
                        _ => continue,
                    };
                    let v = match serde_json::from_str::<serde_json::Value>(&text) {
                            Ok(v) => {v}
                            Err(e) => {error!("解析 WS 消息失败: {}", e); continue;},
                    };
                    if v["type"].as_str().unwrap_or("") != "UPDATE" {
                        continue;
                    }
                    
                    let incoming_hash = v["hash"].as_str().unwrap_or("");
                    let current_hash = self_clone.hash.lock().await.clone().unwrap_or_default();
                    if incoming_hash == current_hash {
                        continue;
                    }
                    debug!("[WS UPDATE]: {} -> {}", current_hash, incoming_hash);
                    let song_playing_new = match self_clone.fetch_playlist().await {
                        Ok(song_playing_new) => song_playing_new,
                        Err(e) => {error!("WS 触发拉取失败: {}", e); continue;},
                    };
                    // 仅当“正在播放”的歌曲实际变化时才触发投屏切换
                    if song_playing_new == song_playing_cached { debug!("[WS UPDATE] 播放歌曲未变，跳过投屏切换"); continue; }
                    if let Some(url) = song_playing_new.clone() {
                        f_on_update(url).await;
                    }
                    // 更新本地缓存
                    song_playing_cached = song_playing_new;
                }

                // 等待并重连
                tokio::time::sleep(Duration::from_secs(3)).await;
                debug!("重连 WS...");
            }
        });
    }

    pub fn start_periodic_update<F>(&self, f_on_update: F)
    where
        F: Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
    {
        let mut self_clone = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(300));
            let mut song_playing: Option<String> = None;
            loop {
                interval.tick().await;
                match self_clone.fetch_playlist().await {
                    Err(e) => error!("定时更新播放列表失败: {}", e),
                    Ok(song_playing_new) => {
                        if song_playing_new != song_playing {
                            if let Some(url) = song_playing_new.clone() {
                                f_on_update(url).await; // await the future
                            }
                            song_playing = song_playing_new;
                        }
                    }
                }
            }
        });
    }

    pub async fn next_song(&mut self) -> Result<(), String> {
        let url = format!("{}/api/nextSong?roomId={}", self.url, self.room_id);
        let temp_hash = self
            .hash
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "EMPTY_LIST_HASH".to_string());
        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .map_err(|e| format!("创建HTTP客户端失败: {}", e))?;
        let resp = client
            .post(&url)
            .json(&json!({"idArrayHash": temp_hash}))
            .send()
            .await
            .map_err(|e| format!("发送请求失败: {}", e))?;
        let resp_json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析JSON失败: {}", e))?;

        if !resp_json["success"].as_bool().unwrap_or(false) {
            return Err(format!("请求失败: {}", resp_json));
        }
        self.fetch_playlist().await?;

        Ok(())
    }

    pub async fn get_song_playing(&self) -> Option<String> {
        self.song_playing.lock().await.clone()
    }
}

#[tokio::test]
async fn test_playlist_manager() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== PlaylistManager 使用示例 ===");

    let playlist = Arc::new(Mutex::new(Vec::<String>::new()));

    let mut manager = PlaylistManager::new("https://ktv.starfreedomx.top", 1111, playlist.clone());

    println!("开始获取播放列表...");

    // --- 第一次操作 ---
    match manager.fetch_playlist().await {
        Ok(_) => {
            println!("✓ 成功获取播放列表");
            // 【关键点 1】：用大括号包裹锁的使用
            {
                let playlist_lock = playlist.lock().await;
                println!("播放列表内容 ({} 个项目):", playlist_lock.len());
                for (i, url) in playlist_lock.iter().enumerate() {
                    println!("  {}. {}", i + 1, url);
                }
            } // <--- 锁在这里被强制释放 (DROP)
        }
        Err(e) => error!("✗ 获取播放列表失败: {}", e),
    }

    // --- 第二次操作 ---
    manager.next_song().await?;
    println!("请求下一首歌曲后播放列表状态:");

    // 【关键点 2】：再次用大括号包裹锁
    {
        let playlist_lock = playlist.lock().await;
        for (i, url) in playlist_lock.iter().enumerate() {
            println!("  {}. {}", i + 1, url);
        }
    } // <--- 锁在这里被强制释放 (DROP)

    // --- 后台任务开始 ---
    manager.start_periodic_update(|url: String| {
        println!("Song singing changed to {}!", url);
        Box::pin(async {})
    });

    // 【关键点 3】：sleep 必须在“裸奔”状态下运行（不持有任何锁）
    // 此时 playlist 锁是空闲的，后台线程的 fetch_playlist 才能拿到锁并更新数据
    sleep(Duration::from_secs(5)).await;

    println!("5秒后播放列表状态:");

    // 【关键点 4】：休眠结束后，再次获取锁查看结果
    {
        let playlist_lock = playlist.lock().await;
        for (i, url) in playlist_lock.iter().enumerate() {
            println!("  {}. {}", i + 1, url);
        }
    }

    println!("=== 示例结束 ===");
    Ok(())
}
