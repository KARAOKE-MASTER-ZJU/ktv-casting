use reqwest::Client;
use serde_json::Value;

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
    let client = Client::new();
    let url = format!("https://api.bilibili.com/x/player/pagelist?bvid={}", bv_id);
    let json: Value = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
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
/// * `page` - 分P页码，默认为1
///
/// # Returns
/// * `Result<String, String>` - 返回直链URL或错误信息
pub async fn get_bilibili_direct_link(bv_id: &str, page: Option<u32>) -> Result<String, String> {
    let client = Client::new();
    let page = page.unwrap_or(0);

    //如果bv_id本来就是一个URL，直接返回
    if bv_id.starts_with("http") {
        return Ok(bv_id.to_string());
    }

    // 第一步：获取CID
    let cid = get_video_cid(&client, bv_id, page).await?;

    // 第二步：获取视频直链
    get_video_url(&client, bv_id, &cid).await
}

/// 获取视频的CID（分集ID）
async fn get_video_cid(client: &Client, bv_id: &str, page: u32) -> Result<String, String> {
    let url = format!("https://api.bilibili.com/x/player/pagelist?bvid={}", bv_id);

    let response = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
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
async fn get_video_url(client: &Client, bv_id: &str, cid: &str) -> Result<String, String> {
    let url = format!(
        "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn=116&type=&otype=json&platform=html5&high_quality=1",
        bv_id, cid
    );

    let response = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0")
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
    }
}
