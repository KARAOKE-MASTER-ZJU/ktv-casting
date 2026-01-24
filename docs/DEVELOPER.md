# 开发者文档（ktv-casting）

本文面向要在本仓库上二次开发/调试的开发者。

## 项目概览

`ktv-casting` 是一个命令行 DLNA/UPnP 投屏工具，用来配合 [ktv-song-web](https://github.com/StarfreedomX/ktv-song-web)

- 你输入房间链接（例如 `https://ktv.example.com/102`）
- 程序解析出 `base_url` 与 `room_id`
- 启动一个本地 HTTP 服务（默认 `0.0.0.0:8080`）提供媒体代理/转发（见 `media_server::proxy_handler`）
- 通过 UPnP/DLNA 发现局域网内的 MediaRenderer
- 调用 AVTransport（SOAP over HTTP）设置播放 URL 并触发播放
- 后台轮询播放进度、自动切歌（逻辑主要在 `PlaylistManager` + `DlnaController`）

### 关键模块

- `src/main.rs`：CLI 入口；读取房间 URL；启动本地 HTTP 服务；发现设备并开始投屏。
- `src/dlna_controller.rs`：UPnP/DLNA 控制逻辑；SSDP 发现；构造并发送 AVTransport SOAP；兼容某些设备的 `controlURL` 异常。
- `src/media_server.rs`：本地媒体代理（把远端视频/音频转成渲染器可拉取的 URL）。
- `src/playlist_manager.rs`：从 `ktv-song-web` 拉取播放列表/当前曲目并触发投屏动作。

## 编译与运行

## 环境准备

- 建议：Rust stable（能支持 `edition = "2024"` 的版本）
- 安装方式：使用 rustup

> 注意：如果你的 Rust 版本过旧，会在解析 `Cargo.toml` 的 edition 时失败。

依赖说明：

- Web 服务：`actix-web`, `actix-files`
- 异步运行时：`tokio`
- UPnP/DLNA：`rupnp`
- HTTP 客户端：`reqwest`（使用 rustls）
- 日志：`log`, `env_logger`
- 其他：`anyhow`, `serde`, `url`, `local-ip-address` 等


### 编译

在仓库根目录执行：

```bash
cargo build
```

发布编译：

```bash
cargo build --release
```

### 运行

电脑端可直接运行：

```bash
cargo run
```

手机端可用termux运行编译好的可执行文件,见[README.md](../README.md#手机上怎么用)

程序会提示：

1. 输入房间链接（例如 `https://ktv.example.com/102`）
2. 自动搜索 DLNA 设备并列出
3. 输入设备编号

### 运行时网络要求

- 运行机器与 DLNA 设备必须在同一局域网
- 需要允许 UDP 多播/广播（SSDP 发现依赖 239.255.255.250:1900）
- 渲染器会反向访问你本机的媒体代理服务：默认 `http://<你的局域网IP>:8080/...`
  - 所以：防火墙要放行入站 TCP 8080
  - 如果你在 macOS 上开启了严格防火墙/第三方安全软件，常见现象是“能发现设备但无法播放”

### 日志与调试输出

项目默认设置 `RUST_LOG=INFO`（见 `src/main.rs`）。你也可以手动覆盖：

```bash
RUST_LOG=debug cargo run
```

如果你要只看 UPnP/DLNA 相关日志，建议：

```bash
RUST_LOG=info,ktv_casting=debug cargo run
```

> 如果 crate 名称不是 `ktv_casting`（取决于代码里 `log::` 的 target），以实际为准；最稳妥还是用 `RUST_LOG=debug`。

## (重要!)连接DLNA设备

以ch**k为例子，需要先扫码后，包房的机器才能被DLNA协议发现
1. 在包房的平板上选择 发现-手机投屏，显示出投屏二维码(**注意不是点歌二维码**)
2. 连接设备(取决于你在哪里运行ktv-casting，是手机还是电脑)
- 手机：用手机微信扫码连接投屏，登录-同意用户协议-连接设备，看到成功的提示即可。此时打开b站或 BubbleUPNP等客户端测试，设备列表中会出现包房的机器名称。
- 电脑: 用手机浏览器扫码得到二维码对应的链接，在**电脑版微信**中打开链接(注意选择用微信内部浏览器，**不能使用系统浏览器**)，之后操作同手机端。MacOS的微信成功，但是Windows版有时会有bug。如果弹出允许访问本地网络的设备，选择允许即可。

## (重要!)辅助工具安装与抓包调试


### 手机端抓包

手机抓包的核心目标：

用一个“已知可用”的投屏 App（例如某些播放器）投同一个 DLNA 设备时，抓到它发的 SOAP 请求将其与本项目日志/抓包对比，找出差异（controlURL、SOAPAction、MetaData、协议字段等）

基本步骤：
1. 安装 [PCAPdroid](https://play.google.com/store/apps/details?id=com.emanuelef.remote_capture)
2. 启动捕获（会创建本地 VPN），选择导出到文件
3. 在bilibili等客户端连接，执行投屏操作
4. 结束捕获，导出 .pcap文件，在电脑端 Wireshark 打开

### 电脑端：Wireshark

安装[Wireshark](https://www.wireshark.org/download.html)

> MacOS安装时如果提示安装抓包权限组件（ChmodBPF），建议按提示完成，否则可能无法捕获某些接口流量。

### (重要!)常用 Display Filter和操作技巧

首先在ktv-casting的日志中获取设备ip, 记为`192.168.x.x`
常用抓包点：

1. SSDP 发现（UDP 1900，多播 239.255.255.250）
2. 设备描述与控制（HTTP：通常是设备的 80/1400/49152+ 等端口）
3. 你的本地媒体代理服务（HTTP：默认 8080）


常用过滤器：

- 获取发现设备的过程(UDP)：`ip.addr == 192.168.x.x && udp.port == 1900`
- 获取投屏控制过程(SOAPAction) (HTTP)：`ip.addr == 192.168.x.x && http`
    - SetAVTransportURI 请求：由于b站自定义链接和header一般很长，可以`ip.addr == 192.168.x.x && http && frame.len>=1000`来过滤
    - Play 请求：`ip.addr == 192.168.x.x && http && http.request.method == "POST" && http contains "Play"` 或者直接用frame.len较短(300-400)来过滤
- 只看 AVTransport：`http contains "AVTransport"`
- 只看你的本地代理服务（默认 8080）：`tcp.port == 8080`

找到对应的包后，可以右键“Follow”→“HTTP Stream”查看完整请求/响应内容。点击“Back”返回抓包列表。

### 建议的抓包流程（定位“发现了但播不起来”）

1. 先确认 SSDP：能看到本机发出 `M-SEARCH`，也能看到设备返回 `HTTP/1.1 200 OK`
2. 点击设备返回里的 `LOCATION:`，确认能在浏览器访问到 `description.xml`
3. 观察程序调用 `SetAVTransportURI`/`Play` 时的 HTTP 请求：
   - URL 的 host/port/path 是否与 `description.xml` 的 `controlURL`/base URL 匹配
   - `SOAPAction` 是否正确
   - HTTP status 是否为 200
4. 观察 DLNA 设备是否来拉取媒体：是否访问了 `http://<你的IP>:8080/...`


### 其他辅助工具(可选)

[BubbleUPnP](https://bubblesoftapps.com/bubbleupnp/)（Android）：可以测试投屏到不同 DLNA 设备，确认设备是否支持 AVTransport 播放视频/音频。

- 投视频需要下载[VLC](https://www.videolan.org/vlc/index.zh.html)等播放器配合使用
- 在KTV使用DLNA工具可以参考[这篇帖子](https://www.xiaohongshu.com/discovery/item/68a01b0e000000001d01c489?source=webshare&xhsshare=pc_web&xsec_token=ABRib4kFPexc3iGS37nK9H-MDIYe91LBEGmU1hKU-oShk=&xsec_source=pc_share)

[扫码投屏的参考资料](https://dolphinstar.cn/)

## DLNA / UPnP 协议速览（结合本项目）

这里以“控制端（本程序）→ 渲染器（电视/盒子）”为主线。

### 1) SSDP 发现（UDP 1900）

- 控制端发送 `M-SEARCH` 到多播地址 `239.255.255.250:1900`
- 设备响应 `HTTP/1.1 200 OK`，包含 `LOCATION:`（设备描述 XML 的 URL）

你在 Wireshark 里应看到：

- 请求：`M-SEARCH * HTTP/1.1`
- 关键头：
  - `ST:`（Search Target）
  - `MAN: "ssdp:discover"`
  - `MX:`

本项目使用 `rupnp::discover`，并以 `AVTransport` service URN 作为 SearchTarget（见 `src/dlna_controller.rs` 中 `AV_TRANSPORT`）。

### 2) 设备描述（Device Description XML）

设备返回的 `LOCATION` 指向 `description.xml`，里面会列出：

- 设备类型：MediaRenderer/MediaServer
- serviceList：每个 service 的：
  - `serviceType`（例如 `urn:schemas-upnp-org:service:AVTransport:1`）
  - `controlURL`（SOAP 控制地址）
  - `eventSubURL`（事件订阅地址）
  - `SCPDURL`（服务描述）

常见坑：

- 有些设备的 `controlURL` 不以 `/` 开头（例如 `_urn:...`）。
  - 本项目里有一个兼容逻辑：把控制 URL 强行拼成 `/_urn:schemas-upnp-org:service:AVTransport_control`（见 `avtransport_action_compat`）。

### 3) SOAP 控制（AVTransport）

投屏最核心的两个动作：

1. `SetAVTransportURI`
2. `Play`

请求形态：

- HTTP POST 到 `controlURL`
- Header：
  - `Content-Type: text/xml; charset="utf-8"`
  - `SOAPAction: "urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI"`
- Body：SOAP Envelope + Action 参数

本项目额外做了 DIDL-Lite 元数据（`CurrentURIMetaData`）的构造与 XML escaping（见 `build_didl_lite_metadata`）。

### 4) 进度查询（GetPositionInfo）

渲染器不一定会主动上报进度，因此应用侧通常轮询：

- `GetPositionInfo` 返回 `RelTime`/`TrackDuration` 等

本项目的兼容实现会从 SOAP 返回 XML 中“尽力解析”常见 tag（`extract_xml_tag_value`），用来计算剩余/总时长。

### 5) RenderingControl（音量等，可选）

不少 DLNA 设备把音量、静音等放在 `RenderingControl` 服务。

本项目当前侧重点是 AVTransport；如果后续要加音量控制，建议：

- 先在抓包里确认设备暴露的 `RenderingControl:1` 端点
- 对照设备的 SCPD（服务描述）确认 action 名称与参数

## 常见问题（FAQ）

### 能发现设备，但 SetAVTransportURI/Play 没效果

优先按顺序排查：

1. 设备是否真的支持 `AVTransport:1`（某些只支持投图片/镜像）
2. `controlURL` 拼接是否正确（特别是缺 `/` 的设备）
3. 渲染器是否能访问你的 `http://<IP>:8080/...`（防火墙/跨网段/NAT/手机热点都常见）
4. URL/MetaData 是否被设备拒绝：
   - MIME 类型不匹配
   - `protocolInfo` 太严格
   - `CurrentURIMetaData` 缺字段或转义有误

### 设备拉取 8080 失败

- 看 Wireshark：是否有来自设备的 TCP SYN 到 8080
- 若无：设备根本访问不到你机器（网络隔离/访客 Wi‑Fi/跨 VLAN）
- 若有但被 reset：本机防火墙/安全软件

### 抓包里看到 HTTP 204/401/500

- 204：少数设备会返回非 200 但实际上成功（项目里对部分 2xx error code 有“视为成功”的处理）
- 401：需要认证（少见）或是走错了 controlURL
- 500：多数是 SOAP 参数不匹配/MetaData 格式不被接受

## 贡献与开发建议

- 优先把“可复现问题”的抓包（pcap）和日志（`RUST_LOG=debug`）一起提交
- 不同电视/盒子兼容性差异很大：新增兼容逻辑时建议
  - 以抓包为基准
  - 对控制 URL、SOAPAction、MetaData 做可配置化（后续可做）
