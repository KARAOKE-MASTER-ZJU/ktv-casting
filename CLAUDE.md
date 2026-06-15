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

## Rust 库与 Android App 的版本联动

**重要：** 每次更新 Rust 库（ktv-casting）并打新 tag 后，必须同步更新 Android App 的 `gradle.properties`：

```
# ktv-casting-android-app/gradle.properties
rust_libs_version=v1.x.x   ← 必须与 Rust 库的 release tag 一致
```

**流程：**
1. Rust 库改动 → commit → 打 tag（如 `v1.5.0`）→ push → CI 构建 `.so`
2. 修改 Android App 的 `gradle.properties`，将 `rust_libs_version` 改为新 tag
3. commit → 打 App tag → push

**不更新此值的后果：** App CI 会下载旧版 `.so`，导致新 JNI 函数缺失，运行时 `UnsatisfiedLinkError` 崩溃。

## GitHub Actions CI 检查流程

推送新版本和 tag 后，使用 gh cli 检查 Android App 构建状态：

```bash
# 进入 Android App 目录
cd /Users/ypy/projects/ktv-casting-android-app

# 查看最近的构建状态（显示最近 5 次）
gh run list --repo birchtree2/ktv-casting-android-app --limit 5 --json status,conclusion,name,createdAt,headBranch

# 获取最新运行的 ID
LATEST_RUN=$(gh run list --repo birchtree2/ktv-casting-android-app --limit 1 --json databaseId | jq -r '.[0].databaseId')

# 查看特定运行的详细状态
gh run view $LATEST_RUN --repo birchtree2/ktv-casting-android-app --json status,conclusion,headBranch

# 在浏览器中打开运行详情页面
gh run view $LATEST_RUN --repo birchtree2/ktv-casting-android-app --web
```

**关键字段说明：**
- `status`: `queued` (排队中), `in_progress` (运行中), `completed` (已完成)
- `conclusion`: `success` (成功), `failure` (失败), `neutral` (中立), 空值表示正在运行
