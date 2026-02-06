use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use reqwest::Client;
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::{env, future::Future};
use tokio::sync::Mutex;
#[cfg(test)]
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

#[derive(Clone)]
pub struct PlaylistManager {
    url: String,
    room_id: String,
    client: Client,
    hash: Arc<Mutex<Option<String>>>,
    playlist: Arc<Mutex<Vec<String>>>,
    song_playing: Arc<Mutex<Option<String>>>,
}

impl PlaylistManager {
    pub fn new(url: &str, room_id: String, playlist: Arc<Mutex<Vec<String>>>) -> Self {
        // 在初始化时构建一次 Client
        let client = Client::builder()
            .use_rustls_tls()
            .build()
            .expect("构建 HTTP 客户端失败");
        Self {
            url: url.to_string(),
            room_id,
            client,
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
    async fn fetch_playlist(&self) -> Result<Option<String>, String> {
        let last_hash = self
            .hash
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "EMPTY_LIST_HASH".into());

        let url = format!(
            "{}/api/songListInfo?roomId={}&lastHash={}",
            self.url, self.room_id, last_hash
        );

        debug!("正在获取播放列表: {}", url);

        let resp = self
            .client
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

        // 当前正在演唱的歌曲：list.singing.url
        let singing_url: Option<String> = resp_json["list"]["singing"]["url"]
            .as_str()
            .map(extract_bv_function);

        info!("获取到 {} 个URL，新的hash: {}", urls.len(), new_hash);

        // 打印每个URL用于调试
        for (i, url) in urls.iter().enumerate() {
            debug!("  {}. {}", i + 1, url);
        }

        // 更新状态
        *self.playlist.lock().await = urls;
        *self.song_playing.lock().await = singing_url.clone();
        *self.hash.lock().await = Some(new_hash);

        Ok(singing_url)
    }

    // 根据环境变量切换同步驱动（WS / POLLING）
    // 环境变量：KTV_SYNC_MODE = "WS" 或 "POLLING"（不区分大小写），默认为 WS
    pub fn start_sync<F>(&self, f_on_update: F)
    where
        F: Fn(String) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + 'static,
    {
        let mode = env::var("KTV_SYNC_MODE").unwrap_or_else(|_| "WS".to_string());
        info!("播放列表同步模式: {}", mode);
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
        let self_clone = self.clone();
        tokio::spawn(async move {
            /*
               这是维护WebSocket连接的循环
               负责连接、重连
            */
            let interval_secs = env::var("KEEP_ALIVE_INTERVAL")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(30);
            info!("心跳间隔: {} 秒", interval_secs);
            loop {
                // 构造 WS URL （将 http(s) -> ws(s)）
                let nickname = env::var("KTV_NICKNAME").unwrap_or_default();
                let mut ws_url = format!(
                    "{}/api/ws?roomId={}&nickname={}",
                    self_clone.url.trim_end_matches('/'),
                    self_clone.room_id,
                    urlencoding::encode(&nickname)
                );
                if let Ok(mut parsed) = Url::parse(&ws_url) {
                    let _ = match parsed.scheme() {
                        "https" => parsed.set_scheme("wss"),
                        "http" => parsed.set_scheme("ws"),
                        _ => Ok(()),
                    };
                    ws_url = parsed.to_string();
                }

                info!("WebSocket Connecting: {}", ws_url);

                // 调试用代码
                /* let url = match url::Url::parse(&ws_url) {
                    Ok(u) => u,
                    Err(e) => {
                        error!("URL 解析失败: {}", e);
                        break;
                    }
                };
                let host = url.host_str().unwrap_or_default().to_string();
                let port = url.port_or_known_default().unwrap_or(443);

                // 异步 DNS 解析
                let addrs = match tokio::task::spawn_blocking(move || {
                    use std::net::ToSocketAddrs;
                    (host.as_str(), port).to_socket_addrs()
                })
                .await
                {
                    Ok(Ok(addr_iter)) => addr_iter,
                    _ => {
                        error!("DNS 解析失败");
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let addr = match addrs.into_iter().next() {
                    Some(a) => a,
                    None => {
                        error!("DNS 未找到记录");
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                info!("DNS 解析成功: {}", addr);

                // 建立 TCP 连接
                let stream = match tokio::time::timeout(
                    tokio::time::Duration::from_secs(10),
                    tokio::net::TcpStream::connect(addr),
                )
                .await
                {
                    Ok(Ok(s)) => s,
                    _ => {
                        error!("TCP 连接超时或拒绝");
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                info!("TCP 已连接，准备 WS 握手...");

                // WebSocket 握手

                // 加载 webpki 根证书
                let mut root_store = rustls::RootCertStore::empty();
                root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

                // 2. 创建配置 (支持 TLS 1.2 和 1.3)
                let config = rustls::ClientConfig::builder()
                    .with_root_certificates(root_store)
                    .with_no_client_auth();

                let connector = tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(config));

                let (mut ws_stream, _) = match tokio::time::timeout(
                    tokio::time::Duration::from_secs(10),
                    tokio_tungstenite::client_async_tls_with_config(
                        ws_url.clone(),
                        stream,
                        None,
                        Some(connector),
                    ),
                )
                .await
                {
                    Ok(Ok(val)) => val,
                    Ok(Err(e)) => {
                        error!("WS 握手报错: {:?}", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        continue;
                    }
                    Err(_) => {
                        error!("WS 握手超时");
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        continue;
                    }
                }; */

                let (ws_stream, _) = match connect_async(ws_url).await {
                    Ok(val) => val,
                    Err(e) => {
                        error!("连接 WebSocket 失败: {}", e);
                        continue;
                    }
                };

                info!("WebSocket connected for room {}", self_clone.room_id);

                // 本地缓存当前正在播放的歌曲，用于判断是否需要触发投屏切换
                let mut song_playing_cached: Option<String> =
                    self_clone.song_playing.lock().await.clone();
                match self_clone.fetch_playlist().await {
                    Ok(Some(url)) => {
                        if Some(url.clone()) != song_playing_cached {
                            info!("检测到新歌曲，初始化投屏: {}", url);
                            f_on_update(url.clone()).await;
                            song_playing_cached = Some(url);
                        } else {
                            info!("重连成功，歌曲未变，跳过重复投屏");
                        }
                    }
                    Ok(None) => debug!("歌单目前为空，等待点歌..."),
                    Err(e) => error!("初始化拉取失败: {}", e),
                }
                let (mut write, mut read) = ws_stream.split();

                // 引入心跳计时器
                let mut heartbeat = tokio::time::interval(Duration::from_secs(interval_secs));
                // 设置为延迟触发模式，避免不必要的积压和短间隔
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // 立即触发第一次心跳
                heartbeat.tick().await;

                loop {
                    tokio::select! {
                        // 分支 A：定时发送心跳
                        _ = heartbeat.tick() => {
                            // 发送 WebSocket 协议层 Ping (维持 WS 长连接)
                            if let Err(e) = write.send(Message::Ping(vec![].into())).await {
                                warn!("发送 WS 心跳失败: {}, 准备重连", e);
                                break;
                            }

                            // Keep-Alive HTTP Connection Pool
                            let pm_warm = self_clone.clone();
                            tokio::spawn(async move {
                                // 调用 fetch_playlist 会执行一次完整的 HTTP GET 请求
                                // 从而让 reqwest 保持与后端的 TCP 连接处于活跃状态
                                match pm_warm.fetch_playlist().await {
                                    Ok(_) => debug!("HTTP Keep-Alive"),
                                    Err(e) => debug!("HTTP Keep-Alive Failed: {}", e),
                                }
                            });
                        }

                        // 分支 B：接收 WS 消息
                        msg = read.next() => {
                            let msg = match msg {
                                Some(Ok(m)) => m,
                                Some(Err(e)) => { error!("WS 读取错误: {}", e); break; },
                                None => { info!("WS 连接关闭"); break; }
                            };

                            match msg {
                                Message::Text(text) => {
                                    let v = match serde_json::from_str::<serde_json::Value>(&text) {
                                        Ok(v) => v,
                                        Err(e) => { error!("解析 JSON 失败: {}", e); continue; },
                                    };
                                    if v["type"].as_str().unwrap_or("") != "UPDATE" { continue; }
                                    info!("收到 WS UPDATE 消息: {:?}", v);
                                    let incoming_hash = v["hash"].as_str().unwrap_or("");
                                    let current_hash = self_clone.hash.lock().await.clone().unwrap_or_default();
                                    if incoming_hash == current_hash { continue; }

                                    debug!("[WS UPDATE]: {} -> {}", current_hash, incoming_hash);
                                    if let Ok(song_playing_new) = self_clone.fetch_playlist().await {
                                        if song_playing_new != song_playing_cached {
                                            if let Some(url) = song_playing_new.clone() {
                                                f_on_update(url).await;
                                            }
                                            song_playing_cached = song_playing_new;
                                        }
                                    }
                                }
                                Message::Ping(p) => {
                                    let _ = write.send(Message::Pong(p)).await;
                                }
                                Message::Close(_) => { info!("服务器关闭连接"); break; }
                                _ => {}
                            }
                        }
                    }
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
        let self_clone = self.clone();
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
        let resp = self
            .client
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

    let mut manager = PlaylistManager::new("https://ktv.starfreedomx.top", "1111".to_string(), playlist.clone());

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
