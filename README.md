# 与[ktv-song-web](https://github.com/StarFreedomX/ktv-song-web)搭配的命令行DLNA投屏软件

[开发者文档](docs/DEVELOPER.md)
## 使用方式

输入搭建好的[ktv-song-web](https://github.com/StarFreedomX/ktv-song-web)服务的网址（含对应房间编号），如`http://ktv.example.com/101`，随后选择搜索到的DLNA设备，即可使用。

## 功能

跟随网页的正在播放曲目进行投屏，结束自动切歌。也可以在网页端操作进行切歌。

命令行支持`Ctrl+P`暂停/继续播放

## 环境变量设置

- `KTV_SYNC_MODE`：同步模式，支持`WS`（WebSocket, 默认）和`POLLING`（轮询）。设置为`WS`时会使用WebSocket连接进行实时同步，延迟更低~~(对ktv-song-web服务器压力更小)~~。
- `RUST_LOG`：日志等级设置，有`error`、`warn`、`info`、`debug`等，参考[env_logger文档](https://docs.rs/env_logger/latest/env_logger/)。
- `KTV_NICKNAME`：设置投屏设备的名称。
- `KEEP_ALIVE_INTERVAL`：连接Keep-Alive间隔，单位秒，默认30秒。

## 手机上怎么用

1. 下载并安装[Termux](https://termux.com/)。
2. 从[这里](https://github.com/aspromise/ktv-casting/releases)下载最新的`ktv-casting-aarch64-linux-android`可执行文件。
建议可以直接在Termux中使用`curl -LO <下载链接>`命令下载。以`v0.1.5`版本为例，命令如下：
```bash
curl -LO https://github.com/aspromise/ktv-casting/releases/download/v0.1.5/ktv-casting-aarch64-linux-android
```
3. 赋予可执行权限：
```bash
chmod +x ktv-casting-aarch64-linux-android
```
4. 运行程序：
```bash
./ktv-casting-aarch64-linux-android
```

