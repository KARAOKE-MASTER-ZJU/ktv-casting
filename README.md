# ktv-casting

将 [ktv-song-web](https://github.com/KARAOKE-MASTER-ZJU/ktv-song-web) 点歌台的歌曲投屏到 **DLNA 设备** 或 **哔哩哔哩小电视** 的命令行工具与核心引擎。

> **配套 Android App**：[ktv-casting-android-app](https://github.com/KARAOKE-MASTER-ZJU/ktv-casting-android-app)

> **技术文档**：[Bilibili DLNA 1080P 与纯 Rust 在线混流](docs/bilibili-dlna-1080.md)

---

## 功能概览

- **两种投屏模式**
  - **DLNA 模式**：通过 UPnP/SSDP 发现局域网内的 DLNA 渲染器，使用 SOAP (AVTransport) 控制播放
  - **Bilibili 模式**：通过 B 站扫码登录，投屏到 Bilibili TV 端设备（小电视/盒子等）
- **本地媒体代理**：自动将 B 站视频链接转为 DLNA 设备可拉取的 HTTP 流（默认 `0.0.0.0:8080`），支持 Range 请求与进度拖拽
- **实时同步**：支持 WebSocket（默认，低延迟）和 HTTP 轮询两种模式，与点歌台保持歌曲列表同步
- **自动切歌**：检测歌曲播放结束后自动切换到下一首
- **音量控制**：通过 DLNA RenderingControl 服务调节音量
- **CLI 交互式控制**：暂停/继续、切歌、音量调节、进度条
- **Android JNI 支持**：提供完整的 JNI 接口，可作为 Android 应用的 Native 引擎
- **跨平台编译**：支持 Windows、macOS、Linux、Android (4 种 ABI)

---

## 快速开始

### 命令行模式

```bash
cargo run --release
```

程序会交互式引导：

1. 输入房间链接（例如 `https://ktv.example.com/102`）
2. 选择投屏模式（1: DLNA / 2: Bilibili）
3. 选择设备

#### 房间链接格式

- `https://ktv.example.com/102` — 路径最后一段
- `https://ktv.example.com/?roomId=102` — URL 查询参数

#### 快捷键

| 按键 | 功能 |
|------|------|
| `p` | 暂停 / 继续 |
| `n` | 切歌 |
| `+` / `=` | 音量 +5 |
| `-` | 音量 -5 |
| `Ctrl-C` | 退出 |

### Android App

直接安装 [ktv-casting-android-app](https://github.com/KARAOKE-MASTER-ZJU/ktv-casting-android-app) 即可获得完整 UI 体验。

---

## 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `KTV_SYNC_MODE` | 同步模式：`WS`（WebSocket）或 `POLLING`（轮询） | `WS` |
| `RUST_LOG` | 日志等级：`error`, `warn`, `info`, `debug` 等 | `INFO` |
| `KTV_NICKNAME` | 投屏设备名称（通过 WebSocket 传给点歌台） | 空 |
| `KEEP_ALIVE_INTERVAL` | WebSocket 心跳间隔（秒） | `30` |

---

## 编译

### 桌面端

```bash
cargo build --release
```

### Android ABI

```bash
rustup target add aarch64-linux-android
cargo ndk -t arm64-v8a build --lib --release
```


---

## 开发者文档

详细的架构说明、模块介绍、协议详解、抓包调试指南等请参阅：

👉 **[docs/DEVELOPER.md](docs/DEVELOPER.md)**
