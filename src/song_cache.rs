//! Bounded, per-song source-block cache. A new song gets a new instance and
//! cancels the old one. The caller chooses an app-private writable directory.
//! Files and their directory are removed only after active readers release them.
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use std::{
    collections::{HashMap, VecDeque},
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, OnceCell, Semaphore, watch};

pub const BLOCK_BYTES: u64 = 1024 * 1024;
const HOT_BLOCKS: usize = 4;
type Key = (usize, u64);
static DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

pub struct RemoteSource {
    pub url: String,
    pub len: u64,
}
struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
struct Block {
    path: PathBuf,
    ready: OnceCell<()>,
    accessed: AtomicU64,
    _directory: Arc<Directory>,
}
impl Drop for Block {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct SongCache {
    client: reqwest::Client,
    sources: Vec<RemoteSource>,
    directory: Arc<Directory>,
    blocks: Mutex<HashMap<Key, Arc<Block>>>,
    hot: Mutex<VecDeque<(Key, Bytes)>>,
    block_limit: usize,
    parallel: Arc<Semaphore>,
    cancelled: watch::Sender<bool>,
    tick: AtomicU64,
    requests: AtomicU64,
    hits: AtomicU64,
}

fn error(kind: io::ErrorKind, message: &str) -> io::Error {
    io::Error::new(kind, message)
}
fn cancelled() -> io::Error {
    error(io::ErrorKind::Interrupted, "song cache cancelled")
}

impl SongCache {
    pub fn new(
        parent: &Path,
        client: reqwest::Client,
        sources: Vec<RemoteSource>,
        capacity: u64,
    ) -> io::Result<Self> {
        let block_limit = usize::try_from(capacity / BLOCK_BYTES)
            .map_err(|_| error(io::ErrorKind::InvalidInput, "cache capacity too large"))?;
        if block_limit == 0 || sources.is_empty() || sources.iter().any(|s| s.len == 0) {
            return Err(error(
                io::ErrorKind::InvalidInput,
                "empty cache capacity or source",
            ));
        }
        // create_dir (not create_dir_all) refuses to adopt another session's directory.
        let directory = loop {
            let n = DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("ktv-song-{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => break Arc::new(Directory(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let (cancelled, _) = watch::channel(false);
        Ok(Self {
            client,
            sources,
            directory,
            blocks: Mutex::new(HashMap::new()),
            hot: Mutex::new(VecDeque::new()),
            block_limit,
            parallel: Arc::new(Semaphore::new(4)),
            cancelled,
            tick: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        })
    }

    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
    }
    pub fn stats(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.hits.load(Ordering::Relaxed),
        )
    }

    /// Read at most one block of data. Cross-boundary requests are split internally.
    /// Larger virtual responses should stream these pieces instead of accumulating.
    pub async fn read(&self, source: usize, range: Range<u64>) -> io::Result<Bytes> {
        let remote = self
            .sources
            .get(source)
            .ok_or_else(|| error(io::ErrorKind::InvalidInput, "unknown source"))?;
        if range.start > range.end
            || range.end > remote.len
            || range.end - range.start > BLOCK_BYTES
        {
            return Err(error(
                io::ErrorKind::InvalidInput,
                "invalid or oversized cache read",
            ));
        }
        let mut stop = self.cancelled.subscribe();
        if *stop.borrow() {
            return Err(cancelled());
        }
        tokio::select! {
            biased;
            _ = stop.changed() => Err(cancelled()),
            result = async {
                let mut output = BytesMut::with_capacity((range.end - range.start) as usize);
                let mut offset = range.start;
                while offset < range.end {
                    let block_number = offset / BLOCK_BYTES;
                    let bytes = self.block((source, block_number)).await?;
                    let start = (offset % BLOCK_BYTES) as usize;
                    let count = ((range.end - offset) as usize).min(bytes.len() - start);
                    output.extend_from_slice(&bytes[start..start+count]);
                    offset += count as u64;
                }
                Ok(output.freeze())
            } => result,
        }
    }

    async fn block(&self, key: Key) -> io::Result<Bytes> {
        {
            let mut hot = self.hot.lock().await;
            if let Some(index) = hot.iter().position(|(k, _)| *k == key) {
                let entry = hot.remove(index).unwrap();
                let bytes = entry.1.clone();
                hot.push_back(entry);
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(bytes);
            }
        }
        let block = {
            let mut blocks = self.blocks.lock().await;
            if let Some(block) = blocks.get(&key) {
                Arc::clone(block)
            } else {
                if blocks.len() >= self.block_limit {
                    let oldest = blocks
                        .iter()
                        .filter(|(_, b)| Arc::strong_count(b) == 1)
                        .min_by_key(|(_, b)| b.accessed.load(Ordering::Relaxed))
                        .map(|(k, _)| *k);
                    let Some(oldest) = oldest else {
                        return Err(error(
                            io::ErrorKind::WouldBlock,
                            "cache capacity pinned by active readers",
                        ));
                    };
                    blocks.remove(&oldest);
                    self.hot.lock().await.retain(|(k, _)| *k != oldest);
                }
                let block = Arc::new(Block {
                    path: self.directory.0.join(format!("{}-{}", key.0, key.1)),
                    ready: OnceCell::new(),
                    accessed: AtomicU64::new(0),
                    _directory: Arc::clone(&self.directory),
                });
                blocks.insert(key, Arc::clone(&block));
                block
            }
        };
        block
            .accessed
            .store(self.tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        let fetched = std::sync::atomic::AtomicBool::new(false);
        block
            .ready
            .get_or_try_init(|| async {
                let permit = self
                    .parallel
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| cancelled())?;
                let remote = &self.sources[key.0];
                let start = key.1 * BLOCK_BYTES;
                let end = start.saturating_add(BLOCK_BYTES).min(remote.len);
                self.requests.fetch_add(1, Ordering::Relaxed);
                let bytes = fetch_range(&self.client, &remote.url, start..end, remote.len).await?;
                // A dropped async filesystem write can continue on Tokio's worker.
                // Publish atomically from a unique staging file; keep the block pinned
                // until that worker finishes, even if the initiating read is cancelled.
                let writer = Arc::clone(&block);
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let n = DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
                    let staging = Staging(writer.path.with_extension(format!("pending-{n}")));
                    std::fs::write(&staging.0, &bytes)?;
                    std::fs::rename(&staging.0, &writer.path)
                })
                .await
                .map_err(io::Error::other)??;
                fetched.store(true, Ordering::Relaxed);
                Ok::<(), io::Error>(())
            })
            .await?;
        if !fetched.load(Ordering::Relaxed) {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
        let bytes = Bytes::from(tokio::fs::read(&block.path).await?);
        let expected = (self.sources[key.0].len - key.1 * BLOCK_BYTES).min(BLOCK_BYTES);
        if bytes.len() as u64 != expected {
            return Err(error(
                io::ErrorKind::InvalidData,
                "cached block length mismatch",
            ));
        }
        let mut hot = self.hot.lock().await;
        hot.retain(|(k, _)| *k != key);
        hot.push_back((key, bytes.clone()));
        while hot.len() > HOT_BLOCKS.min(self.block_limit) {
            hot.pop_front();
        }
        Ok(bytes)
    }
}

/// Strict byte-range fetch, shared by index loading and media caching. Rejects
/// ignored ranges, mismatched file sizes and truncated/oversized responses.
pub async fn fetch_range(
    client: &reqwest::Client,
    url: &str,
    range: Range<u64>,
    total: u64,
) -> io::Result<Bytes> {
    if range.start >= range.end || range.end > total || range.end - range.start > 16 * BLOCK_BYTES {
        return Err(error(io::ErrorKind::InvalidInput, "invalid upstream range"));
    }
    let response = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0")
        .header("Referer", "https://www.bilibili.com/")
        .header("Accept-Encoding", "identity")
        .header("Range", format!("bytes={}-{}", range.start, range.end - 1))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| io::Error::other(e.without_url()))?;
    let expected = format!("bytes {}-{}/{}", range.start, range.end - 1, total);
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || response
            .headers()
            .get("Content-Range")
            .and_then(|v| v.to_str().ok())
            != Some(expected.as_str())
    {
        return Err(error(
            io::ErrorKind::InvalidData,
            "upstream did not return the exact requested range",
        ));
    }
    let mut result = BytesMut::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| io::Error::other(e.without_url()))?;
        if result.len() as u64 + chunk.len() as u64 > range.end - range.start {
            return Err(error(
                io::ErrorKind::InvalidData,
                "oversized upstream range",
            ));
        }
        result.extend_from_slice(&chunk);
    }
    if result.len() as u64 != range.end - range.start {
        return Err(error(
            io::ErrorKind::UnexpectedEof,
            "truncated upstream range",
        ));
    }
    Ok(result.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    struct Origin {
        url: String,
        requests: Arc<AtomicU64>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Origin {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn origin(data: Bytes, status: u16, delay: bool) -> Origin {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/media", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicU64::new(0));
        let count = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let data = data.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..n]);
                    }
                    count.fetch_add(1, Ordering::Relaxed);
                    if delay {
                        std::future::pending::<()>().await;
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    let range = request
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end: usize = end.parse().unwrap();
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        data.len(),
                        end - start + 1
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&data[start..=end]).await;
                });
            }
        });
        Origin {
            url,
            requests,
            task,
        }
    }

    fn cache(server: &Origin, length: u64, blocks: u64) -> SongCache {
        SongCache::new(
            &std::env::temp_dir(),
            reqwest::Client::new(),
            vec![RemoteSource {
                url: server.url.clone(),
                len: length,
            }],
            blocks * BLOCK_BYTES,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn concurrent_reads_share_download_and_cleanup_directory() {
        let server = origin(Bytes::from_static(b"0123456789"), 206, false).await;
        let cache = Arc::new(cache(&server, 10, 2));
        let path = cache.directory.0.clone();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let c = cache.clone();
            tasks.push(tokio::spawn(async move { c.read(0, 2..7).await.unwrap() }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().as_ref(), b"23456");
        }
        assert_eq!(server.requests.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats().0, 1);
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
        drop(cache);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cross_block_reads_eviction_and_capacity_are_exact() {
        let data: Vec<u8> = (0..BLOCK_BYTES as usize * 3 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        let server = origin(Bytes::from(data.clone()), 206, false).await;
        let cache = cache(&server, data.len() as u64, 1);
        let r = BLOCK_BYTES - 3..BLOCK_BYTES + 5;
        assert_eq!(
            cache.read(0, r.clone()).await.unwrap().as_ref(),
            &data[r.start as usize..r.end as usize]
        );
        cache
            .read(0, 2 * BLOCK_BYTES..2 * BLOCK_BYTES + 10)
            .await
            .unwrap();
        assert_eq!(cache.blocks.lock().await.len(), 1);
        assert_eq!(cache.hot.lock().await.len(), 1);
        let disk: u64 = std::fs::read_dir(&cache.directory.0)
            .unwrap()
            .map(|f| f.unwrap().metadata().unwrap().len())
            .sum();
        assert!(disk <= BLOCK_BYTES);
        assert_eq!(cache.read(0, 0..10).await.unwrap().as_ref(), &data[..10]);
        assert_eq!(server.requests.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn cancellation_interrupts_inflight_network_and_rejects_later_reads() {
        let server = origin(Bytes::from_static(b"0123456789"), 206, true).await;
        let cache = Arc::new(cache(&server, 10, 1));
        let path = cache.directory.0.clone();
        let reader = cache.clone();
        let read = tokio::spawn(async move { reader.read(0, 0..10).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.requests.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cache.cancel();
        let result = tokio::time::timeout(Duration::from_millis(200), read)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            cache.read(0, 0..1).await.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        drop(cache);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn ignored_range_is_never_cached_as_valid_data() {
        let server = origin(Bytes::from_static(b"0123456789"), 200, false).await;
        let cache = cache(&server, 10, 1);
        assert!(cache.read(0, 0..5).await.is_err());
        assert!(cache.read(0, 0..5).await.is_err());
        assert_eq!(server.requests.load(Ordering::Relaxed), 2);
        assert_eq!(std::fs::read_dir(&cache.directory.0).unwrap().count(), 0);
    }
}
