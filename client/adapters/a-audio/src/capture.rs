//! 真设备采集后端（cpal）。
//!
//! 这是整个仓库里唯一碰平台音频 API 的地方——按架构约定，平台差异必须关在适配器进程里，
//! 客户端核心不认识"麦克风"这个概念。
//!
//! 三条现场约束：
//! - 一律混成单声道：ASR 不需要双声道，弱机上却要多一倍字节与拷贝；
//! - 跟随设备原生采样率，不偷偷重采样——真实采样率随每个分段上报。宁可让下游知道
//!   "这是 48k"，也不要拿一段假装 16k 的音频去转写；params 里的 sample_rate 只是偏好，
//!   与设备不一致时会在 session.open 里明说。
//! - 没有输入设备时明确报错并保持不产出：宁可这个源从导出的健康表里整个缺席，
//!   也不要造一条"看起来在工作"的假流。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{InputCallbackInfo, Stream, StreamConfig};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// 已打开的采集流。Stream 被 drop 即停止回调，所以它必须和接收端同生命周期。
pub struct Captured {
    _stream: Stream,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: String,
    pub device: String,
}

/// 交织采样按声道数平均成单声道。
pub fn downmix(data: &[i16], channels: u16) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return data.to_vec();
    }
    data.chunks_exact(ch)
        .map(|f| {
            let sum: i32 = f.iter().map(|s| *s as i32).sum();
            (sum / ch as i32) as i16
        })
        .collect()
}

/// 打开默认输入设备，把混好的单声道 i16 帧推进 `tx`。
pub fn open(tx: Sender<Vec<i16>>) -> Result<Captured, String> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(|| {
        format!("没有可用的音频输入设备（领夹麦/阵列麦是否接好？系统是否授权麦克风？可选设备 {:?}）", list_inputs())
    })?;
    let device_name = device.to_string();
    let sc = device.default_input_config().map_err(|e| format!("查询默认输入配置失败：{e}"))?;
    let format = format!("{:?}", sc.sample_format());
    let cfg: StreamConfig = sc.into();
    let (sample_rate, channels) = (cfg.sample_rate, cfg.channels);

    // 采样格式由设备决定，逐个单态化转换；认不出的格式直接拒，不做"大概是 PCM"的猜测。
    let stream = match format.as_str() {
        "I16" => build::<i16>(&device, cfg, |s| *s, tx)?,
        "F32" => build::<f32>(&device, cfg, |s| (s * 32_767.0).clamp(-32_768.0, 32_767.0) as i16, tx)?,
        "U16" => build::<u16>(&device, cfg, |s| (*s as i32 - 32_768) as i16, tx)?,
        other => return Err(format!("暂不支持设备采样格式 {other}（只支持 I16/F32/U16）")),
    };
    Ok(Captured { _stream: stream, sample_rate, channels, format, device: device_name })
}

fn build<T>(
    device: &cpal::Device,
    cfg: StreamConfig,
    conv: fn(&T) -> i16,
    tx: Sender<Vec<i16>>,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + Send + 'static,
{
    let ch = cfg.channels.max(1);
    let err = |e: cpal::Error| eprintln!("[a-audio] 采集流错误：{e}");
    let cb = move |data: &[T], _: &InputCallbackInfo| {
        // 音频线程里只做一次转换 + 一次 send：不写盘、不加锁、不等待。
        let v: Vec<i16> = data.iter().map(conv).collect();
        let _ = tx.send(downmix(&v, ch));
    };
    let stream = device
        .build_input_stream(cfg, cb, err, Some(Duration::from_secs(3)))
        .map_err(|e| format!("打开采集流失败（{} 声道 @{sample_rate}Hz）：{e}", cfg.channels, sample_rate = cfg.sample_rate))?;
    stream.play().map_err(|e| format!("启动采集流失败：{e}"))?;
    Ok(stream)
}

/// 现场排障用：列出可选输入设备（无设备时的错误信息里也带上它）。
pub fn list_inputs() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(it) => it.map(|d| d.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_is_averaged_not_summed() {
        // 相加会让电平翻倍，VAD 门限就形同虚设了。
        assert_eq!(downmix(&[100, -100, 200, 400, 0, 0], 2), vec![0, 300, 0]);
    }

    #[test]
    fn quad_stream_downmixes_frame_by_frame() {
        assert_eq!(downmix(&[10i16, 20, 30, 40, -10, -20, -30, -40], 4), vec![25, -25]);
    }

    #[test]
    fn mono_passes_through_and_zero_channels_does_not_panic() {
        let v = [1i16, -2, 3];
        assert_eq!(downmix(&v, 1), v.to_vec());
        assert_eq!(downmix(&v, 0), v.to_vec(), "声道数 0 要按单声道处理，不能除零");
    }

    #[test]
    fn no_input_device_yields_an_actionable_message() {
        // CI runner 上没有声卡：这条路径必须返回可读错误，而不是 panic，也不是"成功但没数据"。
        let (tx, _rx) = std::sync::mpsc::channel::<Vec<i16>>();
        match open(tx) {
            Err(e) => assert!(
                e.contains("设备") || e.contains("配置") || e.contains("格式"),
                "错误要指出下一步查什么：{e}"
            ),
            // 有真实麦克风的开发机上会打开成功；这里不产出事件，只要求不 panic。
            Ok(c) => assert!(c.sample_rate > 0, "上报的采样率必须是设备真实值"),
        }
    }
}
