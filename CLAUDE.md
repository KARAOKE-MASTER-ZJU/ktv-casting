# KTV Casting Development Guide

## 分P (Multi-page Videos) 处理规则

**核心规则：**
- **代码层 (page parameter)**: 从 0 开始索引 (P1→page=0, P2→page=1, P3→page=2, ...)
- **UI层/用户输入**: 从 1 开始计数 (P1, P2, P3, ...)

**现状：**
- `get_page_info(bv_id, page)` 中，page 直接用作数组索引，所以 page=0 表示第一页
- `parse_song_ref("BV号-page0")` 解析格式正确，返回 (bvid, 0)

**注意：** 当从 UI 或外部数据源获取分P信息时，如果输入是 1-indexed (P1, P2)，需要先 -1 再传给代码层。

## 其他关键实现细节

- Bilibili token 存储在应用专属目录 (Android filesDir)，使用 EncryptedSharedPreferences
- Rust 版本锁定到 1.81 (rust-toolchain.toml)
- 库版本在 Cargo.toml 中维护，应与 GitHub release tag 保持一致
