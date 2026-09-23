//! 能量门限 VAD（语音活动分段）——不是模型，是规则。
//!
//! 为什么放在采集端做：现场必须把连续音频切成"一个话轮一段"的可消费单位，
//! 否则一节课 45 分钟要么是一个 300 MB 的整文件，要么是一串没有语义的固定块。
//! 真分句交给云端 ASR 的词级时间戳，这里只保证三件事：
//! 1. 话轮边界带上预滚和尾巴，起音不被切掉（ASR 最怕这个）；
//! 2. 单段长度有上限，弱机的内存和 blob 体积都受控；
//! 3. 门限用迟滞（开/关两个值），否则老师一停顿就抖出几十个碎段。
//!
//! 全部是纯函数：喂进 i16 帧、吐出分段，因此 CI 上能用合成音频逐个钉死。

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct VadConfig {
    /// 判定用的帧长。20 ms 是语音的常规分析窗。
    pub frame_ms: u64,
    /// 开段门限（RMS，按 i16 满量程 32767 计）。
    pub rms_open: f64,
    /// 关段门限，必须低于开门限，两者之间就是迟滞带。
    pub rms_close: f64,
    /// 低于关段门限持续多久才算"说完了"。这段时间会留在段尾。
    pub hangover_ms: u64,
    /// 开段时向前多带的缓冲，避免吃掉起音。
    pub preroll_ms: u64,
    /// 有效语音短于此长度的段直接丢弃（关门声、翻书）。
    pub min_speech_ms: u64,
    /// 单段上限，控制 blob 体积与内存。
    pub max_segment_ms: u64,
}

impl Default for VadConfig {
    fn default() -> Self {
        // 16 kHz 单声道下，rms_open=500 大约对应安静教室里能挑出讲话的电平。
        // 现场真正的噪声底要靠 params 调，这里给一个不会全程误开也不会全程漏开的起点。
        VadConfig {
            frame_ms: 20,
            rms_open: 500.0,
            rms_close: 300.0,
            hangover_ms: 350,
            preroll_ms: 120,
            min_speech_ms: 250,
            max_segment_ms: 8_000,
        }
    }
}

impl VadConfig {
    /// 从适配器的 params.vad 里取值；没写的字段用默认，写反了的做钳制。
    pub fn from_json(v: Option<&Value>) -> Self {
        let mut c = VadConfig::default();
        let Some(o) = v else { return c };
        let get = |k: &str| o.get(k).and_then(|x| x.as_f64());
        if let Some(x) = get("frame_ms").filter(|x| *x >= 5.0) { c.frame_ms = x as u64 }
        if let Some(x) = get("rms_open").filter(|x| *x > 0.0) { c.rms_open = x }
        if let Some(x) = get("rms_close").filter(|x| *x > 0.0) { c.rms_close = x }
        if let Some(x) = get("hangover_ms") { c.hangover_ms = x.max(0.0) as u64 }
        if let Some(x) = get("preroll_ms") { c.preroll_ms = x.max(0.0) as u64 }
        if let Some(x) = get("min_speech_ms") { c.min_speech_ms = x.max(0.0) as u64 }
        if let Some(x) = get("max_segment_ms").filter(|x| *x >= 500.0) { c.max_segment_ms = x as u64 }
        // 迟滞带不能反过来：关段门限高于开门限会让段永远关不掉。
        if c.rms_close >= c.rms_open {
            c.rms_close = c.rms_open * 0.6;
        }
        c
    }
}

/// 一个已切好的语音段：课堂时间轴上的位置 + 原始采样。
#[derive(Debug, Clone)]
pub struct Segment {
    pub t0_ms: u64,
    pub dur_ms: u64,
    /// 段内有效语音时长（不含 hangover 尾巴），用于判断这是不是一个真话轮。
    pub speech_ms: u64,
    pub rms: f64,
    pub peak: i16,
    pub samples: Vec<i16>,
}

pub fn frame_rms(frame: &[i16]) -> f64 {
    if frame.is_empty() {
        return 0.0;
    }
    let acc: f64 = frame.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (acc / frame.len() as f64).sqrt()
}

pub fn frame_peak(frame: &[i16]) -> i16 {
    frame.iter().fold(0i16, |m, s| m.max(s.abs()))
}

/// 逐帧推进的分段状态机。调用方负责把设备采样混成单声道并按 frame 交付。
pub struct Vad {
    cfg: VadConfig,
    sample_rate: u32,
    channels: u16,
    frame_samples: usize,
    /// 已吞掉的采样数（用于把位置换算成课堂毫秒）。
    consumed: usize,
    open: bool,
    pending: Vec<i16>,
    pending_t0: usize,
    speech_samples: usize,
    silence_samples: usize,
    preroll: std::collections::VecDeque<i16>,
    /// 累计丢掉的短段数：现场排障时"为什么没有音频"和"为什么全是碎段"是同一类问题。
    dropped_short: u64,
}

/// 帧长（采样个数），按实际采样率与声道数换算。
/// 调用方分帧与状态机必须用同一个数，否则时间轴会偷进偷出。
pub fn frame_samples_for(cfg: &VadConfig, sample_rate: u32, channels: u16) -> usize {
    let ch = channels.max(1) as u64;
    ((cfg.frame_ms * sample_rate as u64 * ch / 1_000).max(1)) as usize
}

impl Vad {
    pub fn new(cfg: VadConfig, sample_rate: u32, channels: u16) -> Self {
        let ch = channels.max(1) as u64;
        let frame_samples = frame_samples_for(&cfg, sample_rate, ch as u16);
        Vad {
            cfg,
            sample_rate,
            channels: ch as u16,
            frame_samples,
            consumed: 0,
            open: false,
            pending: Vec::new(),
            pending_t0: 0,
            speech_samples: 0,
            silence_samples: 0,
            preroll: std::collections::VecDeque::new(),
            dropped_short: 0,
        }
    }

    pub fn dropped_short(&self) -> u64 {
        self.dropped_short
    }

    /// 已经消化了多少课堂时间轴上的音频。下课时要拿它判断"采到的音频"
    /// 和"采集进程的墙钟"是不是一回事（源被限速过、或中途卡过，就会不一致）。
    pub fn consumed_ms(&self) -> u64 {
        self.ms(self.consumed)
    }

    fn ms(&self, samples: usize) -> u64 {
        // 先乘后除：44.1kHz 立体声每毫秒是 88.2 个采样，先算每毫秒采样数会被取整到 88，
        // 一小时漂 6 秒——跨源对齐（边讲边写）就是这么坏的。
        let per_second = self.sample_rate.max(1) as u64 * self.channels.max(1) as u64;
        samples as u64 * 1_000 / per_second
    }

    fn samples_for(&self, ms: u64) -> usize {
        let per_second = self.sample_rate.max(1) as u64 * self.channels.max(1) as u64;
        (ms * per_second / 1_000) as usize
    }

    /// 喂一帧（长度任意，通常 = frame_samples）。说完一段时返回它。
    pub fn push(&mut self, frame: &[i16]) -> Option<Segment> {
        if frame.is_empty() {
            return None;
        }
        let rms = frame_rms(frame);
        let start = self.consumed;
        self.consumed += frame.len();

        if self.open {
            self.pending.extend_from_slice(frame);
            if rms < self.cfg.rms_close {
                self.silence_samples += frame.len();
            } else {
                self.silence_samples = 0;
                self.speech_samples += frame.len();
            }
            let hung = self.silence_samples >= self.samples_for(self.cfg.hangover_ms);
            let too_long = self.ms(self.pending.len()) >= self.cfg.max_segment_ms;
            if hung || too_long {
                return Some(self.finish());
            }
            return None;
        }

        // 未开段：先看这一帧是不是起音，再决定它进不进预滚环。
        // 顺序很重要——先入环再拼 pending 会把起音采样算两遍，时长就虚高了。
        if rms >= self.cfg.rms_open {
            self.open = true;
            self.pending_t0 = start.saturating_sub(self.preroll.len());
            self.pending = std::mem::take(&mut self.preroll).into_iter().collect();
            self.pending.extend_from_slice(frame);
            self.speech_samples = frame.len();
            self.silence_samples = 0;
            return None;
        }
        let keep = self.samples_for(self.cfg.preroll_ms);
        self.preroll.extend(frame.iter().copied());
        while self.preroll.len() > keep {
            self.preroll.pop_front();
        }
        None
    }

    /// 下课/文件读完时调用：把还开着的段吐出来，不让最后那句话消失。
    pub fn flush(&mut self) -> Option<Segment> {
        if self.open {
            return Some(self.finish());
        }
        None
    }

    /// 收段。短于 min_speech_ms 的不算话轮，整段丢掉。
    fn finish(&mut self) -> Segment {
        let samples = std::mem::take(&mut self.pending);
        let t0 = self.ms(self.pending_t0);
        let speech_ms = self.ms(self.speech_samples);
        self.open = false;
        self.speech_samples = 0;
        self.silence_samples = 0;
        self.preroll.clear();
        let dur_ms = self.ms(samples.len());
        Segment {
            t0_ms: t0,
            dur_ms,
            speech_ms,
            rms: frame_rms(&samples),
            peak: frame_peak(&samples),
            samples,
        }
        // 注意：短段的丢弃由 push/finish 的调用方在返回后判断——见 accept()。
    }

    /// 该段是否值得落盘（够长）。太长的段永远保留，太短的按 min_speech_ms 淘汰。
    pub fn accept(&mut self, seg: &Segment) -> bool {
        if seg.speech_ms >= self.cfg.min_speech_ms {
            return true;
        }
        self.dropped_short += 1;
        false
    }
}

/// 把不定长的来样切成定长帧。
///
/// 设备回调一次给多少完全由后端决定（可能是 128 个采样，也可能 4096），
/// 而 VAD 的帧长直接决定时间分辨率，所以两者之间必须有一层攒帧缓冲。
/// 没这层，时间轴会随设备缓冲大小飘。
pub struct FrameStream {
    buf: Vec<i16>,
    frame_ms: u64,
}

impl FrameStream {
    pub fn new(frame_ms: u64) -> Self {
        FrameStream { buf: Vec::new(), frame_ms: frame_ms.max(1) }
    }

    pub fn frame_samples(&self, sample_rate: u32, channels: u16) -> usize {
        ((self.frame_ms * sample_rate.max(1) as u64 * channels.max(1) as u64 / 1_000).max(1)) as usize
    }

    pub fn feed(&mut self, samples: &[i16]) {
        self.buf.extend_from_slice(samples);
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// 攒够一帧就取走；不够返回 None（调用方去等下一批，而不是喂半个帧）。
    pub fn take(&mut self, n: usize) -> Option<Vec<i16>> {
        if self.buf.len() < n {
            return None;
        }
        Some(self.buf.drain(..n).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 16_000;

    /// 生成一段正弦"语音"：幅度决定电平，用它模拟老师讲话。
    fn tone(ms: u64, amp: f64, freq: f64) -> Vec<i16> {
        let n = (ms * SR as u64 / 1_000) as usize;
        (0..n)
            .map(|i| (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / SR as f64).sin()) as i16)
            .collect()
    }

    fn silence(ms: u64) -> Vec<i16> {
        // 不是纯 0：真实教室有噪声底，纯 0 会让"能过门限"这件事被高估。
        let n = (ms * SR as u64 / 1_000) as usize;
        (0..n).map(|i| ((i % 7) as f64 * 3.0 - 9.0) as i16).collect()
    }

    /// 把采样按帧推完，返回落盘的段（短段已按 min_speech_ms 淘汰）。
    fn run(cfg: VadConfig, stream: &[i16]) -> Vec<Segment> {
        let fs = frame_samples_for(&cfg, SR, 1);
        let mut vad = Vad::new(cfg, SR, 1);
        let mut out = Vec::new();
        for chunk in stream.chunks(fs) {
            if let Some(s) = vad.push(chunk) {
                if vad.accept(&s) {
                    out.push(s);
                }
            }
        }
        if let Some(s) = vad.flush() {
            if vad.accept(&s) {
                out.push(s);
            }
        }
        out
    }

    #[test]
    fn one_burst_becomes_one_segment_with_tails() {
        let mut s = silence(500);
        s.extend(tone(700, 9_000.0, 220.0));
        s.extend(silence(1_000));
        let segs = run(VadConfig::default(), &s);
        assert_eq!(segs.len(), 1, "一段话应该只出一个段：{segs:?}");
        let g = &segs[0];
        // 起点在 500ms 之后、且不早于预滚边界；时长覆盖讲话本体并带 hangover 尾巴。
        assert!(g.t0_ms >= 380 && g.t0_ms <= 500, "预滚应该把起音带进来：t0={}ms", g.t0_ms);
        assert!(g.dur_ms >= 700 && g.dur_ms <= 1_200, "尾巴应该带上 hangover 但不吞掉整段静音：{}", g.dur_ms);
        assert!(g.speech_ms >= 600, "有效语音时长要接近讲话本体：{}", g.speech_ms);
    }

    #[test]
    fn a_short_pause_inside_speech_does_not_split_the_turn() {
        let mut s = silence(300);
        s.extend(tone(600, 9_000.0, 180.0));
        s.extend(silence(150)); // 短于 hangover(350ms)
        s.extend(tone(600, 9_000.0, 180.0));
        s.extend(silence(1_200));
        let segs = run(VadConfig::default(), &s);
        assert_eq!(segs.len(), 1, "150ms 的犹豫不该把一句话说成两句：{segs:?}");
        assert!(segs[0].speech_ms >= 1_100, "两段讲话都要被计入语音：{}", segs[0].speech_ms);
    }

    #[test]
    fn a_real_pause_splits_turns() {
        let mut s = silence(300);
        s.extend(tone(600, 9_000.0, 180.0));
        s.extend(silence(1_200)); // 长于 hangover
        s.extend(tone(600, 9_000.0, 180.0));
        s.extend(silence(1_200));
        let segs = run(VadConfig::default(), &s);
        assert_eq!(segs.len(), 2, "停顿后的下一句必须是新段：{segs:?}");
        assert!(segs[1].t0_ms > segs[0].t0_ms + 1_000, "两段的间隔要能看出来");
    }

    #[test]
    fn door_slam_is_dropped_and_counted() {
        let mut s = silence(1_000);
        s.extend(tone(80, 20_000.0, 90.0)); // 又短又响
        s.extend(silence(1_000));
        let cfg = VadConfig::default();
        let fs = frame_samples_for(&cfg, SR, 1);
        let mut vad = Vad::new(cfg, SR, 1);
        let mut kept = Vec::new();
        for chunk in s.chunks(fs) {
            if let Some(seg) = vad.push(chunk) {
                if vad.accept(&seg) {
                    kept.push(seg);
                }
            }
        }
        if let Some(seg) = vad.flush() {
            if vad.accept(&seg) {
                kept.push(seg);
            }
        }
        assert!(kept.is_empty(), "80ms 的撞击不该被当成话轮：{kept:?}");
        assert_eq!(vad.dropped_short(), 1, "但要被记一笔，否则现场无从解释为什么没音频");
    }

    #[test]
    fn continuous_speech_is_capped_by_max_segment() {
        let s = tone(25_000, 9_000.0, 200.0);
        let segs = run(VadConfig::default(), &s);
        assert!(segs.len() >= 3, "25 秒连续讲话要按上限切开：{}", segs.len());
        for g in &segs {
            assert!(g.dur_ms <= 8_500, "单段不得超过上限：{}", g.dur_ms);
        }
        let covered: u64 = segs.iter().map(|g| g.dur_ms).sum();
        assert!(covered > 24_000, "切开不能丢掉内容：{covered}ms");
    }

    #[test]
    fn flush_returns_the_tail_sentence() {
        // 下课铃在说话中途响：最后这句必须落盘，否则"最后一问"永远丢失。
        let s = tone(1_000, 9_000.0, 200.0);
        let cfg = VadConfig::default();
        let fs = frame_samples_for(&cfg, SR, 1);
        let mut vad = Vad::new(cfg, SR, 1);
        for chunk in s.chunks(fs) {
            vad.push(chunk);
        }
        let tail = vad.flush().expect("还开着的段要在下课时吐出");
        assert!(tail.dur_ms >= 900, "尾段长度：{}", tail.dur_ms);
        assert!(vad.flush().is_none(), "flush 之后再 flush 不该重复出段");
    }

    #[test]
    fn levels_and_peaks_are_reported() {
        let segs = run(VadConfig::default(), &tone(900, 12_000.0, 150.0));
        assert_eq!(segs.len(), 1);
        // 正弦的 RMS = 峰值/√2
        assert!(segs[0].peak > 11_000, "峰值：{}", segs[0].peak);
        let expected = segs[0].peak as f64 / 2f64.sqrt();
        assert!((segs[0].rms - expected).abs() / expected < 0.15, "rms={} 期望≈{expected}", segs[0].rms);
    }

    #[test]
    fn inverted_thresholds_are_clamped_instead_of_hanging_open() {
        let j = serde_json::json!({ "rms_open": 400.0, "rms_close": 900.0 });
        let c = VadConfig::from_json(Some(&j));
        assert!(c.rms_close < c.rms_open, "关段门限被钳到开门限以下：{} < {}", c.rms_close, c.rms_open);
    }

    #[test]
    fn timing_is_derived_from_the_actual_stream_format() {
        // 44.1 kHz 立体声：一帧 882 个采样（441 对）= 10 ms。位置换算不能假设 16k 单声道。
        let vad = Vad::new(VadConfig::default(), 44_100, 2);
        assert_eq!(vad.ms(882), 10);
        assert_eq!(vad.samples_for(20), 44_100 * 2 * 20 / 1_000);
    }

    #[test]
    fn onset_samples_are_not_counted_twice() {
        // 起音那一帧要进 pending，但不能既从预滚环里拿一份、再塞一份：
        // 重一次开头就会冒出一个爆音样的重复，时长也会虚高 20ms。
        let mut stream = silence(300);
        stream.extend(tone(400, 9_000.0, 200.0));
        let segs = run(VadConfig::default(), &stream);
        assert_eq!(segs.len(), 1);
        let g = &segs[0];
        // 预滚 120ms + 讲话 400ms = 520ms；每毫秒 16 个单声道采样。
        assert_eq!(g.dur_ms, 520, "段长要恰好等于预滚+讲话：{}", g.dur_ms);
        assert_eq!(g.samples.len(), (520 * SR as u64 / 1_000) as usize, "采样数与时长要一致");
        assert_eq!(g.speech_ms, 400, "语音时长不应把预滚里的静音算进去：{}", g.speech_ms);
        assert_eq!(g.t0_ms, 180, "300ms 静音 - 120ms 预滚：{}", g.t0_ms);
    }

    #[test]
    fn pure_silence_produces_nothing() {
        let segs = run(VadConfig::default(), &silence(5_000));
        assert!(segs.is_empty(), "噪声底不该被当成讲话：{segs:?}");
    }

    #[test]
    fn frames_are_reassembled_from_arbitrary_device_buffers() {
        // 设备回调给 300 个采样，VAD 要 320 一帧：先吐 300，再喂 50 才能成帧。
        let mut fs = FrameStream::new(20);
        assert_eq!(fs.frame_samples(16_000, 1), 320);
        fs.feed(&vec![1i16; 300]);
        assert!(fs.take(320).is_none(), "凑不够一帧不能送出去");
        assert_eq!(fs.buffered(), 300, "取不走时采样要原样留着");
        fs.feed(&vec![1i16; 50]);
        let f = fs.take(320).expect("现在该能取出一帧");
        assert_eq!(f.len(), 320);
        assert_eq!(fs.buffered(), 30, "多出的 30 个采样要留给下一帧");
    }

    #[test]
    fn frame_sizing_follows_the_real_stream_format() {
        let fs = FrameStream::new(20);
        assert_eq!(fs.frame_samples(48_000, 1), 960);
        assert_eq!(fs.frame_samples(48_000, 2), 1_920, "立体声同样本数翻倍");
        assert_eq!(fs.frame_samples(0, 0), 1, "非法参数不能算出 0 长帧（会死循环）");
    }

    #[test]
    fn consumed_ms_tracks_the_audio_not_the_wall_clock() {
        let cfg = VadConfig::default();
        let fs = frame_samples_for(&cfg, SR, 1);
        let mut vad = Vad::new(cfg, SR, 1);
        assert_eq!(vad.consumed_ms(), 0);
        for _ in 0..50 {
            vad.push(&silence(20));
        }
        assert_eq!(vad.consumed_ms(), 50 * fs as u64 * 1_000 / SR as u64);
        assert_eq!(vad.consumed_ms(), 1_000, "50 帧×20ms 就是 1 秒音频");
    }
}
