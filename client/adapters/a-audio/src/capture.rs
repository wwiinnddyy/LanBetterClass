//! 真设备采集后端（cpal）。
//!
//! 这是整个仓库里唯一碰平台音频 API 的地方——按架构约定，平台差异必须关在适配器进程里，
//! 客户端核心不认识"麦克风"这个概念。
//!
//! 四条现场约束：
//! - 一律混成单声道：ASR 不需要双声道，弱机上却要多一倍字节与拷贝；
//! - 跟随设备原生采样率，不偷偷重采样——真实采样率随每个分段上报。params 里的 sample_rate
//!   只是偏好，与设备不一致时会在 session.open 里明说；
//! - 回调里只分配一次、只 send 一次：不写盘、不加锁、不等待；
//! - 没有输入设备时明确报错并保持不产出：宁可这个源从导出的健康表里整个缺席，
//!   也不要造一条"看起来在工作"的假流。
//!
//! 缓冲区刻意按 100 ms 申请：一体机上同时跑白板、浏览器与采集，cpal 默认缓冲被文档
//! 明确警告"可能小得让回调来不及处理而掉帧"。实测默认值下真出现过 WASAPI overrun，
//! 所以这里宁可牺牲一点延迟换"一节课不缺音频"，申请失败再退回默认。

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FrameCount, InputCallbackInfo, Stream, StreamConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

/// 已打开的采集流。Stream 被 drop 即停止回调，所以它必须和接收端同生命周期。
pub struct Captured {
    _stream: Stream,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: String,
    pub device: String,
    errors: Arc<AtomicUsize>,
}

impl Captured {
    /// 后端报过的流错误次数。要随 session.close 上报——只写在 stderr 里的话，
    /// 课后没人会翻，而"掉过帧"直接影响能不能拿时长下结论。
    pub fn stream_errors(&self) -> usize {
        self.errors.load(Ordering::SeqCst)
    }
}

/// 交织采样按声道数平均成单声道（已解码成 i16 的情况，例如 fixture 回放）。
pub fn downmix(data: &[i16], channels: u16) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return data.to_vec();
    }
    data.chunks_exact(ch)
        .map(|f| (f.iter().map(|s| *s as i32).sum::<i32>() / ch as i32) as i16)
        .collect()
}

/// 从设备原生格式一次转换 + 一次落地成单声道 i16。
///
/// 只分配一次（先算出帧数再 with_capacity），因为这条路径跑在实时音频线程上；
/// 先转成中间 Vec 再混音的写法在 48k 立体声下每回调两次堆分配，实测能换来 overrun。
pub fn to_mono<T>(data: &[T], channels: u16, conv: fn(&T) -> i16) -> Vec<i16> {
    let ch = channels.max(1) as usize;
    let frames = data.len() / ch;
    let mut out = Vec::with_capacity(frames);
    if ch == 1 {
        out.extend(data.iter().map(conv));
        return out;
    }
    for f in data.chunks_exact(ch) {
        let sum: i32 = f.iter().map(|s| conv(s) as i32).sum();
        out.push((sum / ch as i32) as i16);
    }
    out
}

/// 打开默认输入设备，把混好的单声道 i16 帧推进 `tx`。
pub fn open(tx: Sender<Vec<i16>>) -> Result<Captured, String> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(|| {
        format!(
            "没有可用的音频输入设备（领夹麦/阵列麦是否接好？系统是否授权麦克风？可选设备 {:?}）",
            list_inputs()
        )
    })?;
    let device_name = device.to_string();
    let sc = device.default_input_config().map_err(|e| format!("查询默认输入配置失败：{e}"))?;
    let format = format!("{:?}", sc.sample_format());
    let cfg: StreamConfig = sc.into();
    let (sample_rate, channels) = (cfg.sample_rate, cfg.channels);
    let errors = Arc::new(AtomicUsize::new(0));
    let tx = Arc::new(tx);

    // 采样格式由设备决定，逐个单态化转换；认不出的格式直接拒，不做"大概是 PCM"的猜测。
    // 类型参数写死而不让编译器从闭包反推：闭包形参的类型在推断顺序上救不了 T。
    let stream = match format.as_str() {
        "I16" => build::<i16>(&device, cfg, &tx, &errors, |s| *s)?,
        "F32" => build::<f32>(&device, cfg, &tx, &errors, |s| (s * 32_767.0).clamp(-32_768.0, 32_767.0) as i16)?,
        "U16" => build::<u16>(&device, cfg, &tx, &errors, |s| (*s as i32 - 32_768) as i16)?,
        other => return Err(format!("暂不支持设备采样格式 {other}（只支持 I16/F32/U16）")),
    };
    Ok(Captured { _stream: stream, sample_rate, channels, format, device: device_name, errors })
}

fn build<T>(
    device: &cpal::Device,
    cfg: StreamConfig,
    tx: &Arc<Sender<Vec<i16>>>,
    errors: &Arc<AtomicUsize>,
    conv: fn(&T) -> i16,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + Send + 'static,
{
    let ch = cfg.channels.max(1);
    // 先按 100 ms 申请；设备不接受（超范围）再退回默认，至少能采到。
    let wide = StreamConfig { buffer_size: BufferSize::Fixed((cfg.sample_rate / 10) as FrameCount), ..cfg };
    match try_build(&wide, device, tx, errors, ch, conv) {
        Ok(s) => Ok(s),
        Err(e) => {
            eprintln!("[a-audio] 申请 100ms 缓冲失败（{e}），退回设备默认缓冲");
            try_build(&cfg, device, tx, errors, ch, conv)
        }
    }
}

fn try_build<T>(
    cfg: &StreamConfig,
    device: &cpal::Device,
    tx: &Arc<Sender<Vec<i16>>>,
    errors: &Arc<AtomicUsize>,
    ch: u16,
    conv: fn(&T) -> i16,
) -> Result<Stream, String>
where
    T: cpal::SizedSample + Send + 'static,
{
    let tx2 = tx.clone();
    let cb = move |data: &[T], _: &InputCallbackInfo| {
        // 接收端停了（下课/退出）就直接丢：绝不阻塞音频线程，那是把整台机器的实时性搭进去。
        let _ = tx2.send(to_mono(data, ch, conv));
    };
    let err = {
        let errors = errors.clone();
        move |e: cpal::Error| {
            errors.fetch_add(1, Ordering::SeqCst);
            eprintln!("[a-audio] 采集流错误（累计 {}）：{e}", errors.load(Ordering::SeqCst));
        }
    };
    let stream = device
        .build_input_stream(*cfg, cb, err, Some(Duration::from_secs(3)))
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
    fn device_native_formats_land_on_the_same_mono_path() {
        // 三条转换路径都要落到同一个单声道 i16 结果上。
        let stereo_i16 = [100i16, -100, 200, 400];
        assert_eq!(to_mono(&stereo_i16, 2, |s| *s), vec![0, 300]);
        let stereo_f32 = [1.0f32, 0.5, -0.25, 0.75];
        let m = to_mono(&stereo_f32, 2, |s| (s * 32_767.0).clamp(-32_768.0, 32_767.0) as i16);
        assert_eq!(m.len(), 2, "两帧立体声要出两个单声道采样");
        assert!((m[0] as f32 - 32_767.0 * 0.75).abs() < 2.0, "左右平均：{}", m[0]);
        let u16_stereo = [32_768u16, 32_768, 33_068, 32_768];
        assert_eq!(to_mono(&u16_stereo, 2, |s| (*s as i32 - 32_768) as i16), vec![0, 150]);
    }

    #[test]
    fn odd_tail_is_dropped_instead_of_panicking() {
        // 设备偶尔会给出不整除的长度：宁可丢最后半个帧，也不要 panic 掉一节课。
        assert_eq!(to_mono(&[100i16, 200, 300], 2, |s| *s), vec![150]);
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
