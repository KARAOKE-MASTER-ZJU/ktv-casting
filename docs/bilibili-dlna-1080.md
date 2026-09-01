# Bilibili DLNA 1080P 与纯 Rust 在线混流

本文记录 ktv-casting 在不依赖 Cookie、FFmpeg 和 Android 媒体处理的前提下，将 Bilibili 1080P DASH 视频投送到 DLNA 设备的实现。

## 最终数据流

```mermaid
flowchart LR
    App[Android 清晰度选择] -->|setQuality 64/80| JNI[现有 JNI 接口]
    JNI --> Rust[Rust DLNA Caster]
    TV[DLNA 电视] -->|HTTP GET| Proxy[Rust 媒体服务器]
    Proxy --> API[Bilibili playurl API]
    API -->|720P durl| P720[完整 MP4]
    API -->|1080P DASH| Video[H.264 视频 fMP4]
    API -->|1080P DASH| Audio[AAC 音频 fMP4]
    P720 --> Proxy
    Video --> Mux[纯 Rust fMP4 在线混流]
    Audio --> Mux
    Mux -->|双轨 fragmented MP4| Proxy
    Proxy --> TV
```

- 720P：保留原来的单 URL 代理路径，不增加额外处理。
- 1080P：Rust 同时读取 DASH 视频与音频，在线生成一个双轨 fragmented MP4。
- Android：只选择清晰度。获取媒体、同步和混流全部在 Rust 中完成。

## 匿名 1080P API

测试视频：`BV1dqj16aECG`，CID：`39334447539`。

```http
GET https://api.bilibili.com/x/player/playurl
    ?bvid=BV1dqj16aECG
    &cid=39334447539
    &qn=80
    &fnval=4048
    &fnver=0
    &fourk=0
    &try_look=1
Referer: https://www.bilibili.com/video/BV1dqj16aECG
User-Agent: Mozilla/5.0
```

关键参数：

| 参数 | 作用 |
| --- | --- |
| `fnval=4048` | 返回全部可用 DASH 表示，视频和音频分离 |
| `try_look=1` | 匿名请求仍可获得该视频的 1080P 试看轨 |
| `qn=80` | 表达 1080P 目标；DASH 模式最终仍应检查轨道 ID |
| `fourk=0` | 不请求 4K，减少无关表示 |

### 不要用顶层 quality 判断 DASH 清晰度

匿名返回的顶层 `data.quality` 可能仍然是 `64`，但 `data.dash.video` 已经包含 `id=80`。正确判断方式是：

```text
data.dash.video[].id == 80
```

然后优先选择 `codecs` 以 `avc1` 开头的轨道。H.264/AVC 比 HEVC、AV1 更适合常见 DLNA 电视。

实测选中的媒体：

| 轨道 | 编码 | 参数 | 时长 |
| --- | --- | --- | --- |
| 视频 | H.264 `avc1.640033` | 1920×1080，24000/1001 fps | 285.201563 秒 |
| 音频 | AAC-LC `mp4a.40.2` | 48 kHz，双声道 | 285.205333 秒 |

## 为什么完整 MP4 只能到 720P

```mermaid
flowchart TD
    Q{请求格式}
    Q -->|fnval=1 / html5| MP4[完整 MP4 durl]
    Q -->|fnval=4048| DASH[DASH representations]
    MP4 --> CAP[该新视频最高 720P]
    DASH --> V80[1080P H.264 视频轨 id=80]
    DASH --> A30280[AAC 音频轨 id=30280]
```

Bilibili 的新视频通常只为较低清晰度保留带声音的完整 MP4。高分辨率以 DASH 形式保存，因此“获得一个 1080P 单 URL”并不是请求参数问题，而是源站没有提供这种文件。

## Bilibili DASH 文件结构

视频和音频 URL 都是单轨 fragmented MP4：

```text
视频: [ftyp][moov(video)][sidx][moof][mdat][moof][mdat] ...
音频: [ftyp][moov(audio)][sidx][moof][mdat][moof][mdat] ...
```

- `ftyp`（File Type Box，文件类型框）：声明这是哪种 MP4 文件，以及播放器应按哪些兼容规则解析。
- `moov`（Movie Box，媒体总目录框）：保存轨道初始化信息、编码参数和时间基，不包含实际音视频数据。
- `sidx`（Segment Index Box，片段索引框）：记录后续媒体片段的位置、大小和时长，可用于计算输出总长度。
- `moof`（Movie Fragment Box，媒体分片框）：记录一个媒体片段中的样本索引、所属轨道和解码时间。
- `mdat`（Media Data Box，媒体数据框）：保存已经编码好的 H.264 视频样本或 AAC 音频样本。

这里的 `box`（框）是 MP4 的基本数据单元：每个框都有长度、四字符类型和内容。`ftyp`、`moov`、`trak` 等四字符名称是 ISO BMFF 标准定义的框类型代码，并不是本项目自行发明的缩写。ISO BMFF 全称为 ISO Base Media File Format，即 ISO 基础媒体文件格式；MP4 和 fragmented MP4 都建立在这种结构之上。fragmented MP4 常缩写为 fMP4，中文可称为“分片式 MP4”。

## 手写混流原理

混流不等于转码。本实现不会解码 H.264/AAC，只重组 ISO BMFF 容器。

### 1. 合并初始化段

初始化段是播放器开始解码前必须先读到的文件头，由 `ftyp` 和 `moov` 组成。它只描述“有哪些轨道、各自使用什么编码和时间单位”，不保存真正的画面或声音。Bilibili 的 DASH 视频 URL 和音频 URL 是两个相互独立的单轨 MP4，因此它们原本都可以把自己的唯一轨道编号设为 `1`；合并成一个双轨 MP4 后，两个轨道编号必须唯一。

```mermaid
flowchart TB
    VM[视频 moov：视频文件总目录] --> MVHD[mvhd：媒体公共头]
    VM --> VTRAK[视频 trak：视频轨道说明，编号 1]
    VM --> VTREX[视频 trex：视频分片默认规则，编号 1]
    AM[音频 moov：音频文件总目录] --> ATRAK[音频 trak：音频轨道说明，编号 1]
    AM --> ATREX[音频 trex：音频分片默认规则，编号 1]
    ATRAK -->|track_ID 改为 2| ATRAK2[音频 trak：音频轨道说明，编号 2]
    ATREX -->|track_ID 改为 2| ATREX2[音频 trex：音频分片默认规则，编号 2]
    MVHD --> OUT[合并后的 moov：双轨总目录]
    VTRAK --> OUT
    ATRAK2 --> OUT
    VTREX --> MVEX[mvex：分片播放规则容器]
    ATREX2 --> MVEX
    MVEX --> OUT
```

图中概念说明：

- `mvhd`（Movie Header Box，媒体公共头框）：位于 `moov` 内，保存整个 MP4 共用的时间信息和 `next_track_ID` 等字段。
- `trak`（Track Box，轨道框）：描述一条完整轨道。视频 `trak` 保存视频编码、分辨率和时间信息；音频 `trak` 保存音频编码、声道和采样相关信息。`trak` 是标准规定的四字符框类型，不是普通英文单词的随意缩写。
- `tkhd`（Track Header Box，轨道头框）：位于 `trak` 内，保存这条轨道的 `track_ID`、时长，以及视频尺寸或音频音量等基础属性。
- `mdia`（Media Box，轨道媒体信息框）：位于 `trak` 内，容纳该轨道更具体的媒体类型、时间基和样本描述。
- `mdhd`（Media Header Box，轨道媒体头框）：位于 `mdia` 内，保存该轨道自己的 `timescale` 和时长。
- `mvex`（Movie Extends Box，媒体分片扩展框）：位于 `moov` 内，表示后续数据会以分片形式出现，并集中保存各轨道的分片默认规则。
- `trex`（Track Extends Box，轨道分片扩展框）：位于 `mvex` 内，为某一条轨道声明后续分片的默认样本时长、大小和标志等规则。播放器依靠其中的 `track_ID` 判断这套规则属于视频还是音频。
- `track_ID`（轨道编号）：MP4 内部用于关联“轨道说明”和“媒体分片”的正整数。它不是清晰度编号，也不是 Bilibili 的视频质量编号；同一个 MP4 中不能有两个轨道使用相同编号。
- `next_track_ID`（下一个可用轨道编号）：提示后续若再增加轨道，应从哪个编号开始。合并后已使用 `1` 和 `2`，所以设为 `3`。
- `timescale`（时间刻度）：表示“一秒包含多少个时间单位”。例如 `timescale=16000` 时，时间值 `32000` 代表 2 秒。时长必须除以对应轨道的 `timescale` 才是秒数。

合并时按以下规则处理轨道编号：

1. 视频轨道继续使用 `track_ID=1`。
2. 音频轨道从 `track_ID=1` 改为 `track_ID=2`。
3. 音频 `trak` 中 `tkhd.track_ID`、音频 `trex.track_ID`，以及后续每个音频分片中的轨道编号都必须一起改为 `2`。如果只修改其中一处，播放器就无法把音频分片归到音频轨道。
4. `mvhd.next_track_ID` 改为 `3`，表示编号 `1` 和 `2` 已被占用。

输出初始化段：

```text
[video ftyp]                         # 沿用视频文件的 MP4 类型声明
[moov                                # 新建的双轨媒体总目录
  [mvhd next_track_ID=3]             # 整个 MP4 的公共头，下一个可用轨道编号为 3
  [video trak track_ID=1]            # 视频轨道说明，编号保持为 1
  [audio trak track_ID=2]            # 音频轨道说明，编号从 1 改为 2
  [mvex                               # 后续分片的默认规则容器
    [video trex track_ID=1]           # 视频分片默认规则属于轨道 1
    [audio trex track_ID=2]           # 音频分片默认规则改为属于轨道 2
  ]
]
```

这里的 patch 指“在原有二进制数据中原位修改字段”。`track_ID`、`next_track_ID` 和时长等字段的字节宽度固定，因此修改数值不会改变任何子 `box` 的大小，也不需要解码或重新编码 H.264/AAC 数据。

### 2. 按时间戳交错片段

每个 `moof` 中的 `tfdt.baseMediaDecodeTime` 表示片段解码起点。视频和音频有不同 timescale，因此比较时使用交叉乘法，避免浮点误差：

```text
video_decode_time * audio_timescale
    <= audio_decode_time * video_timescale
```

```mermaid
sequenceDiagram
    participant V as 视频 HTTP 流
    participant M as Rust muxer
    participant A as 音频 HTTP 流
    participant D as DLNA 设备
    V->>M: video moof(tfdt=0) + mdat
    A->>M: audio moof(tfdt=0) + mdat
    M->>M: 比较换算后的 decode time
    M->>D: video moof(track=1, seq=1) + mdat
    M->>D: audio moof(track=2, seq=2) + mdat
    V->>M: 下一视频片段
    A->>M: 下一音频片段
    M->>D: 按时间继续交错输出
```

输出过程中还会修改：

- 音频 `tfhd.track_ID`：`1 → 2`。
- 所有 `mfhd.sequence_number`：改成一个全局递增序列。
- `mdat`：原样输出，媒体字节不做任何修改。

本节新增术语说明：

- `fragment`（媒体片段）：一小段可独立定位解码时间的音频或视频数据，通常由一个 `moof` 和紧随其后的一个 `mdat` 组成。
- `mfhd`（Movie Fragment Header Box，媒体分片头框）：位于 `moof` 内；其中的 `sequence_number` 是分片序号。合并两条流后统一重新编号，避免视频流和音频流出现重复序号。
- `traf`（Track Fragment Box，轨道分片框）：位于 `moof` 内，保存某一条轨道在当前片段中的信息。
- `tfhd`（Track Fragment Header Box，轨道分片头框）：位于 `traf` 内；其中的 `track_ID` 指明当前片段属于哪条轨道，因此音频分片必须从轨道 `1` 改为轨道 `2`。
- `tfdt`（Track Fragment Decode Time Box，轨道分片解码时间框）：位于 `traf` 内；其中的 `baseMediaDecodeTime` 是当前片段第一个样本的解码起始时间，单位是该轨道自己的 `timescale`，不是秒。
- `decode time`（解码时间）：解码器应开始处理样本的时间。它可能与最终画面显示时间不同，但足以用于本实现按时间顺序交错视频和音频片段。

### 3. 流式与内存边界

解析器按 box 读取，内存中最多保留视频、音频各一个 fragment。不会先下载完整文件，也不会把 100 MB 视频整体放入内存。HTTP 响应在初始化段完成后立即开始输出。

`sidx` 中所有 `referenced_size` 的总和用于计算精确的 `Content-Length`：

```text
combined_init_size + video_fragment_bytes + audio_fragment_bytes
```

这比纯 chunked 响应更兼容 DLNA 渲染器。

## 720P/1080P 切换

Android 继续调用已有接口：

```kotlin
RustEngine.setQuality(64) // 720P
RustEngine.setQuality(80) // 1080P (Beta)
```

Rust 的 `DlnaCaster` 只接受这两个值。切换后重新向设备设置相同媒体 URI，并尽可能恢复原播放位置。

1080P 标记为 Beta，原因是在线生成的 fragmented MP4 不提供任意字节 Range 映射；不同品牌 DLNA 设备对拖动进度的行为可能不同。

## 验证方法

### App 调试日志

1080P 链路的关键日志统一使用 `DLNA1080` tag，并通过 JNI
`RustEngine.onRustLog()` 写入 App 的 `LogRepository`，可在 App 日志页面查看。覆盖：

- 清晰度切换和媒体重载；
- 播放地址请求、API 错误及最终 DASH 编码轨选择；
- 视频/音频上游连接及 HTTP 错误；
- 初始化段大小、timescale 和预计 `Content-Length`；
- 每 100 个 fragment 的混流进度、正常完成或下游中断；
- HTTP HEAD 探测和开始向 DLNA 设备输出的时点。

如果某视频匿名 API 不提供 `id=80` 的 1080P DASH 轨道，DLNA 代理会记录 WARN 并自动回退到原有的 720P 完整 MP4，不会把播放器留在失败的 1080P 请求上。

CDN 地址在写入日志前会删除 query 参数，避免泄露临时 token、deadline 等鉴权信息。

完整网络测试：

```bash
cargo test fmp4_mux::tests::live_mux_target_video -- --ignored --nocapture
```

媒体检查：

```bash
ffprobe -v error \
  -show_entries stream=codec_name,codec_type,width,height,sample_rate,channels:format=duration,size \
  -of json target/bilibili-1080-mux-test.mp4
```

验收条件：

- 同一个 MP4 中恰好包含 H.264 视频和 AAC 音频。
- 视频为 1920×1080。
- 音频为 48 kHz 双声道。
- 两轨时长差小于一个 AAC frame。
- 输出字节数与根据 `sidx` 计算的 `Content-Length` 完全一致。

## 相关代码

- `src/bilibili_parser.rs`：720P durl 与匿名 1080P DASH 选择。
- `src/fmp4_mux.rs`：box 解析、初始化段合并、片段交错。
- `src/media_server.rs`：DLNA HTTP 响应与混流入口。
- `src/cast/dlna_caster.rs`：清晰度状态和即时重载。

## 标准与参考资料

- [W3C ISO BMFF Byte Stream Format](https://www.w3.org/TR/mse-byte-stream-format-isobmff/)：初始化段及 `moof + mdat` 媒体段结构。
- [ISO/IEC 14496-12:2026](https://www.iso.org/obp/ui?_escaped_fragment_=iso%3Astd%3Aiso-iec%3A14496%3A-12%3Aed-8%3Av1%3Aen)：ISO Base Media File Format box 与 movie fragment 定义。
- [MPEG-DASH 标准入口](https://www.mpeg.org/standards/MPEG-DASH/)：DASH 对 ISO BMFF 和 MPEG-2 TS 的承载模型。
- [Microsoft DLNA HTTP Transport 说明](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-dlnhnd/ef75437e-3cc0-44f6-a8d7-66ca62b0f979)：DLNA 内容通过 HTTP 传输的要求。
- `b-api/docs/video/videostream_url.md`：Bilibili `qn`、`fnval`、`try_look` 与 DASH 响应字段说明。
