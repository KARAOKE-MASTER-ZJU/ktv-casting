use reqwest::Client;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

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
        let idx = alpha.iter().position(|&a| a == chars[BV_DECODE_MAP[i]]).unwrap_or(0) as u64;
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
    let duration = data[idx]["duration"].as_u64().map(|d| d as u32).unwrap_or(0);
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
    get_bilibili_direct_link_quality(bv_id, page, 64).await
}

/// Resolve a legacy single-file Bilibili URL.  720p is anonymous; 1080p
/// requires the persisted TV login access_key and deliberately fails with a
/// user-actionable message when no valid session exists.
pub async fn get_bilibili_direct_link_quality(
    bv_id: &str,
    page: Option<u32>,
    qn: u32,
) -> Result<String, String> {
    let page = page.unwrap_or(0);

    //如果bv_id本来就是一个URL，直接返回
    if bv_id.starts_with("http") {
        return Ok(bv_id.to_string());
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
async fn get_video_url(bv_id: &str, cid: &str, qn: u32) -> Result<String, String> {
    let qn = if qn == 80 { 80 } else { 64 };
    let session = if qn >= 80 {
        Some(crate::bilibili_session::load_session()
            .filter(|s| !crate::bilibili_session::is_session_expired(s))
            .ok_or_else(|| "1080P 需要先扫码登录 B站".to_string())?)
    } else { None };

    let (url, cookie_header) = if let Some(session) = session.as_ref() {
        let cookie = crate::bilibili_session::cookie_header(session);
        if cookie.is_empty() { return Err("登录 session 中没有 SESSDATA cookie，请重新扫码登录".into()); }
        let nav: Value = bili_api_client().get("https://api.bilibili.com/x/web-interface/nav")
            .header("Cookie", &cookie).send().await
            .map_err(|e| format!("请求 WBI key 失败: {e}"))?.json().await
            .map_err(|e| format!("解析 WBI key 失败: {e}"))?;
        let img = nav["data"]["wbi_img"]["img_url"].as_str().unwrap_or("");
        let sub = nav["data"]["wbi_img"]["sub_url"].as_str().unwrap_or("");
        let raw = format!("{}{}", img.rsplit('/').next().unwrap_or("").split('.').next().unwrap_or(""), sub.rsplit('/').next().unwrap_or("").split('.').next().unwrap_or(""));
        const MIX: [usize; 64] = [46,47,18,2,53,8,23,32,15,50,10,31,58,3,45,35,27,43,5,49,33,9,42,19,29,28,14,39,12,38,41,13,37,48,7,16,24,55,40,61,26,17,0,1,60,51,30,4,22,25,54,21,56,59,6,63,57,62,11,36,20,34,44,52];
        let key: String = MIX.iter().filter_map(|&i| raw.chars().nth(i)).take(32).collect();
        let wts = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs();
        let mut params = vec![
            ("bvid", bv_id.to_string()), ("cid", cid.to_string()), ("fnval", "1".into()),
            ("fnver", "0".into()), ("fourk", "0".into()), ("platform", "html5".into()),
            ("qn", qn.to_string()), ("wts", wts.to_string()),
        ];
        params.sort_by(|a, b| a.0.cmp(b.0));
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter().map(|(k, v)| (*k, v))).finish();
        let wrid = format!("{:x}", md5::compute(format!("{query}{key}")));
        (format!("https://api.bilibili.com/x/player/wbi/playurl?{query}&w_rid={wrid}"), Some(cookie))
    } else {
        (format!("https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn=64&type=&otype=json&platform=html5&high_quality=1", bv_id, cid), None)
    };

    let mut request = bili_api_client().get(&url);
    if let Some(cookie) = cookie_header.as_ref() { request = request.header("Cookie", cookie); }
    let response = request
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

    // 提取直链
    let actual_quality = json["data"]["quality"].as_u64().unwrap_or(0) as u32;
    if qn == 80 && actual_quality < 80 {
        return Err(format!(
            "B站未返回 1080P（实际最高 qn={}, 可用={:?}），请确认视频源和账号权限",
            actual_quality, json["data"]["accept_quality"]
        ));
    }
    let video_url = json
        .get("data")
        .and_then(|d| d.get("durl"))
        .and_then(|d| d.get(0))
        .and_then(|d| d.get("url"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| "无法获取视频链接".to_string())?;

    Ok(video_url.to_string())
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
        crate::bilibili_session::init_session_dir("b-api/biliTVCast");
        match get_bilibili_direct_link_quality("BV1p8VQ6DE7Y", Some(0), 80).await {
            Ok(url) => println!("1080P with persisted session: {}", url),
            Err(e) => println!("1080P with persisted session error: {}", e),
        }
    }
}
