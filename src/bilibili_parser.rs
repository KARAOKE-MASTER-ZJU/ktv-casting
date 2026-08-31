use reqwest::Client;
use serde_json::Value;
use std::sync::OnceLock;

/// 全局共享 B站 API 客户端：连接池复用 + 超时。
/// 超时值来自 `crate::API_TIMEOUT`，编译期保证 < ANR 阈值（5s）。
/// 修复前每次调用 `Client::new()` 都新建 TCP 连接，B站 API 卡住时无超时
/// 永久挂起 → block_on 死锁 → ANR / 闪退。
fn bili_api_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(crate::API_TIMEOUT)
            .user_agent("Mozilla/5.0")
            .build()
            .expect("创建 B站 API 客户端失败")
    })
}

// 新算法 (2020-03+): https://socialsisteryi.github.io/bilibili-API-collect/docs/misc/bvid_desc.html
const BV_XOR_CODE: u64 = 23442827791579;
const BV_MASK_CODE: u64 = 2251799813685247;
const BV_BASE: u64 = 58;
const BV_ALPHABET: &str = "FcwAPNKTMug3GV5Lj7EJnHpWsx4tb8haYeviqBz6rkCy12mUSDQX9RdoZf";
// ENCODE_MAP = [8,7,0,5,1,3,2,4,6]; DECODE_MAP = reversed(ENCODE_MAP)
const BV_DECODE_MAP: [usize; 9] = [6, 4, 2, 3, 1, 5, 0, 7, 8];

pub fn bv_to_aid(bvid: &str) -> u64 {
    let s = bvid.strip_prefix("BV1").unwrap_or(bvid);
    let alpha: Vec<char> = BV_ALPHABET.chars().collect();
    let chars: Vec<char> = s.chars().collect();
    let mut tmp: u64 = 0;
    for i in 0..9 {
        let idx = alpha
            .iter()
            .position(|&a| a == chars[BV_DECODE_MAP[i]])
            .unwrap_or(0) as u64;
        tmp = tmp * BV_BASE + idx;
    }
    (tmp & BV_MASK_CODE) ^ BV_XOR_CODE
}

/// Returns `(cid, duration_secs)` for the given page of a BV video.
pub async fn get_page_info(bv_id: &str, page: u32) -> Result<(u64, u32), String> {
    let url = format!("https://api.bilibili.com/x/player/pagelist?bvid={}", bv_id);
    let json: Value = bili_api_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let data = json["data"].as_array().ok_or("no data")?;
    let idx = page as usize;
    if idx >= data.len() {
        return Err(format!("page {} out of range (len={})", page, data.len()));
    }
    let cid = data[idx]["cid"].as_u64().ok_or("no cid")?;
    let duration = data[idx]["duration"]
        .as_u64()
        .map(|d| d as u32)
        .unwrap_or(0);
    Ok((cid, duration))
}

pub async fn get_page_duration(bv_id: &str, page: u32) -> Result<u32, String> {
    get_page_info(bv_id, page).await.map(|(_, d)| d)
}

/// 获取BiliBili视频直链
///
/// # Arguments
/// * `bv_id` - 视频BV号（例如："BV1AP411x7YW"）
/// * `page` - 分P页码，默认为0
///
/// # Returns
/// * `Result<String, String>` - 返回直链URL或错误信息
pub async fn get_bilibili_direct_link(bv_id: &str, page: Option<u32>) -> Result<String, String> {
    match get_bilibili_media(bv_id, page, 64).await? {
        BilibiliMedia::Direct { url } => Ok(url),
        BilibiliMedia::Dash { .. } => Err("720P 接口意外返回 DASH".to_string()),
    }
}

/// B站媒体地址。720P 保持单文件 MP4；1080P 使用独立的 DASH 视频、音频轨。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BilibiliMedia {
    Direct {
        url: String,
    },
    Dash {
        video_url: String,
        audio_url: String,
        width: u32,
        height: u32,
    },
}

/// 获取指定清晰度的 B站媒体。
///
/// `qn=80` 使用实测可用的匿名 DASH 接口。`try_look=1` 会在没有 Cookie 时
/// 返回可试看高画质轨道；DASH 模式下必须检查 `dash.video[].id`，不能依赖
/// 顶层 `quality` 字段。
pub async fn get_bilibili_media(
    bv_id: &str,
    page: Option<u32>,
    qn: u32,
) -> Result<BilibiliMedia, String> {
    let page = page.unwrap_or(0);

    //如果bv_id本来就是一个URL，直接返回
    if bv_id.starts_with("http") {
        return Ok(BilibiliMedia::Direct {
            url: bv_id.to_string(),
        });
    }

    // 第一步：获取CID
    let cid = get_video_cid(bv_id, page).await?;

    // 第二步：获取视频直链
    get_video_url(bv_id, &cid, qn).await
}

/// 获取视频的CID（分集ID）
async fn get_video_cid(bv_id: &str, page: u32) -> Result<String, String> {
    let url = format!("https://api.bilibili.com/x/player/pagelist?bvid={}", bv_id);

    let response = bili_api_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("请求CID失败: {}", e))?;

    let json: Value = response
        .json()
        .await
        .map_err(|e| format!("解析JSON失败: {}", e))?;

    // 检查API返回状态
    if json["code"].as_i64() != Some(0) {
        return Err(format!(
            "API错误:  {}",
            json.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误")
        ));
    }

    // 检查分P是否存在
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "无效的数据格式".to_string())?;

    if data.is_empty() {
        return Err("该视频没有可用的分P数据".to_string());
    }

    let idx = page as usize;
    if idx >= data.len() {
        return Err(format!(
            "无效的分P: page={}, 有效范围: 0..{}, 总分P数: {}",
            page,
            data.len(),
            data.len()
        ));
    }

    // 获取指定分P的CID
    let cid = data[idx]
        .get("cid")
        .and_then(|c| c.as_u64())
        .ok_or_else(|| "无法获取CID".to_string())?;

    Ok(cid.to_string())
}

/// 获取视频播放链接
async fn get_video_url(bv_id: &str, cid: &str, qn: u32) -> Result<BilibiliMedia, String> {
    let wants_1080 = qn >= 80;
    let url = if wants_1080 {
        format!(
            "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn=80&fnval=4048&fnver=0&fourk=0&try_look=1",
            bv_id, cid
        )
    } else {
        format!(
            "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn=64&type=&otype=json&platform=html5&high_quality=1",
            bv_id, cid
        )
    };

    let response = bili_api_client()
        .get(&url)
        .header("Referer", format!("https://www.bilibili.com/video/{bv_id}"))
        .send()
        .await
        .map_err(|e| format!("请求视频链接失败: {}", e))?;

    let json: Value = response
        .json()
        .await
        .map_err(|e| format!("解析JSON失败:  {}", e))?;

    // 检查API返回状态
    if json["code"].as_i64() != Some(0) {
        return Err(format!(
            "API错误: {}",
            json.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误")
        ));
    }

    let data = &json["data"];
    if !wants_1080 {
        let video_url = data["durl"][0]["url"]
            .as_str()
            .ok_or_else(|| "无法获取 720P 视频链接".to_string())?;
        return Ok(BilibiliMedia::Direct {
            url: video_url.to_string(),
        });
    }

    let videos = data["dash"]["video"]
        .as_array()
        .ok_or_else(|| "1080P 接口没有返回 DASH 视频轨".to_string())?;
    let audios = data["dash"]["audio"]
        .as_array()
        .ok_or_else(|| "1080P 接口没有返回 DASH 音频轨".to_string())?;

    // DLNA 设备对 AVC/H.264 的支持远好于 HEVC/AV1，优先选择 AVC 的 id=80。
    let video = videos
        .iter()
        .filter(|v| v["id"].as_u64() == Some(80))
        .max_by_key(|v| {
            let avc_bonus = v["codecs"].as_str().is_some_and(|c| c.starts_with("avc1")) as u64;
            avc_bonus * u64::MAX / 2 + v["bandwidth"].as_u64().unwrap_or(0)
        })
        .ok_or_else(|| {
            let ids: Vec<u64> = videos.iter().filter_map(|v| v["id"].as_u64()).collect();
            format!("B站未返回 1080P DASH 轨道，可用 id={ids:?}")
        })?;
    let audio = audios
        .iter()
        .max_by_key(|a| a["bandwidth"].as_u64().unwrap_or(0))
        .ok_or_else(|| "1080P DASH 音频轨为空".to_string())?;

    let media_url = |value: &Value| {
        value["base_url"]
            .as_str()
            .or_else(|| value["baseUrl"].as_str())
            .map(str::to_string)
    };
    Ok(BilibiliMedia::Dash {
        video_url: media_url(video).ok_or_else(|| "DASH 视频轨没有 URL".to_string())?,
        audio_url: media_url(audio).ok_or_else(|| "DASH 音频轨没有 URL".to_string())?,
        width: video["width"].as_u64().unwrap_or(0) as u32,
        height: video["height"].as_u64().unwrap_or(0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_bilibili_direct_link() {
        // 示例：测试获取视频直链
        match get_bilibili_direct_link("BV1LS4MzKE8y", Some(2)).await {
            Ok(url) => println!("视频直链: {}", url),
            Err(e) => println!("错误: {}", e),
        }
    }

    #[tokio::test]
    #[ignore = "public Bilibili network fixture; run manually before release"]
    async fn test_anonymous_1080_dash() {
        let media = get_bilibili_media("BV1dqj16aECG", Some(0), 80)
            .await
            .expect("目标视频应提供匿名 1080P DASH");
        match media {
            BilibiliMedia::Dash {
                width,
                height,
                video_url,
                audio_url,
            } => {
                assert_eq!((width, height), (1920, 1080));
                assert!(video_url.starts_with("https://"));
                assert!(audio_url.starts_with("https://"));
            }
            BilibiliMedia::Direct { .. } => panic!("1080P 不应退回单文件 720P"),
        }
    }
}
