#[allow(non_snake_case)]
use crate::ENGINE_STATE;
use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jlong, jobjectArray, jsize};
use log::info;

// 1. 日志初始化
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_initLogging(
    _env: JNIEnv,
    _class: JClass,
    level: jint,
) {
    let log_level = match level {
        0 => log::LevelFilter::Error,
        1 => log::LevelFilter::Warn,
        2 => log::LevelFilter::Info,
        _ => log::LevelFilter::Debug,
    };
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log_level)
            .with_tag("RUST_KTV"),
    );
    // 顺便把 Crypto 也初始化
    let _ = rustls::crypto::ring::default_provider().install_default();

    info!("Android 日志与 Crypto 模块初始化完成");
}

// 2. 搜索接口
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_searchDevices(
    mut env: JNIEnv,
    _class: JClass,
) -> jobjectArray {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dlna_devices = rt.block_on(crate::discover_devices_core());

    let cls = env
        .find_class("zju/bangdream/ktv/casting/DlnaDeviceItem")
        .unwrap();
    let array = env
        .new_object_array(dlna_devices.len() as jsize, &cls, JObject::null())
        .unwrap();
    for (i, d) in dlna_devices.iter().enumerate() {
        let name = env.new_string(&d.friendly_name).unwrap();
        let loc = env.new_string(&d.location).unwrap();
        let item = env
            .new_object(
                &cls,
                "(Ljava/lang/String;Ljava/lang/String;)V",
                &[(&name).into(), (&loc).into()],
            )
            .unwrap();
        env.set_object_array_element(&array, i as jsize, item)
            .unwrap();
    }
    array.into_raw()
}

// 3. 核心初始化接口 (支持重复调用以更换设备)
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_startEngine(
    mut env: JNIEnv,
    _class: JClass,
    base_url: JString,
    room_id: jlong,
    target_location: JString,
) {
    let base_url_str: String = env.get_string(&base_url).unwrap().into();
    let loc_str: String = env.get_string(&target_location).unwrap().into();
    let room_id_u64 = room_id as u64;

    std::thread::spawn(move || {
        // 创建 Runtime
        let rt = tokio::runtime::Runtime::new().unwrap();

        // 在这个 Runtime 里跑异步初始化
        // 我们需要把 rt 的所有权传给 start_engine_core，所以这里用 handle 来 block_on
        let handle = rt.handle().clone();
        handle.block_on(async {
            crate::start_engine_core(base_url_str, room_id_u64, loc_str, rt).await;
        });
    });
}

// 4. 接口：重置引擎 (UI 点击重新选择设备时调用)
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_resetEngine(
    _env: JNIEnv,
    _class: JClass,
) {
    crate::reset_engine();
}

// 5. 数据接口：获取当前播放进度
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_queryProgress(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            return ctx.rt.block_on(crate::get_current_progress()) as jlong;
        }
    }
    -1
}

// 6. 数据接口：获取当前歌曲总时长
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_queryTotalDuration(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            return ctx.rt.block_on(async {
                if let Some(playing) = ctx.playlist_manager.get_song_playing().await {
                    if let Some(&d) = ctx.duration_cache.lock().await.get(&playing) {
                        return d as jlong;
                    }
                }
                0
            });
        }
    }
    0
}

// 7. 控制接口：切歌
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_nextSong(_env: JNIEnv, _class: JClass) {
    crate::trigger_next_song();
}

// 8. 控制接口：播放/暂停 切换
// 返回 1 表示播放，0 表示暂停，失败返回 -1
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_togglePause(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            return match ctx.rt.block_on(crate::toggle_pause_core()) {
                Ok(is_playing) => {
                    if is_playing {
                        1
                    } else {
                        0
                    }
                }
                Err(_) => -1,
            };
        }
    }
    -1
}

// 9. 控制接口：音量调节
// 返回设置后的音量，失败返回 -1
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_setVolume(
    _env: JNIEnv,
    _class: JClass,
    volume: jint,
) -> jint {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            return match ctx.rt.block_on(crate::set_volume_core(volume as u32)) {
                Ok(v) => v as jint,
                Err(_) => -1,
            };
        }
    }
    -1
}

// 10. 控制接口：获取当前音量
// 返回当前音量，失败返回 -1
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub extern "C" fn Java_zju_bangdream_ktv_casting_RustEngine_getVolume(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    if let Ok(guard) = ENGINE_STATE.read() {
        if let Some(ctx) = guard.as_ref() {
            return match ctx.rt.block_on(crate::get_volume_core()) {
                Ok(v) => v as jint,
                Err(_) => -1,
            };
        }
    }
    -1
}
