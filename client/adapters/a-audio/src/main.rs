//! 录音适配器：把教室里的声音接成 `audio.chunk` 事件流。
//!
//! 与 `a-audiofile` 的分工：那个先钉住"大块数据走 blob、消息只给引用"这条约定，
//! 这个负责真实现场——开输入设备、按话轮切段、按课堂时间轴对齐。两者产出的 kind
//! 完全一样，所以下游（timeline / digest / 云端 ASR 回灌）不需要区分来源。
//!
//! 两条输入路径共用同一套攒帧、VAD 与落盘代码：
//! - `source: "device"`（默认）走 cpal 真采集；
//! - `source: "fixture"` + `fixture: "某个.wav"` 回放录音文件。
//!   fixture 不是演示后门，而是"没有声卡的环境也能验证整条链路"这件事本身：
//!   CI runner 上没有麦克风，VAD 的切分正确性只能靠可复现的音频钉住。
//!
//! 只产出单声道 PCM/WAV，不做 Opus：编码要引入原生依赖和另一套失败模式，
//! 而 16 kHz 单声道约 1.9 MB/分钟，还在预算里。压缩留到下一轮。

mod capture;
mod vad;
mod wav;

use classagent_schema::{
    kinds, Admit, AudioChunk, Budget, Command, Envelope, Exceed, LessonInfo, Manifest, RestartPolicy, PROTO,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vad::{FrameStream, Segment, Vad, VadConfig};

const ID: &str = "a-audio";

type Out = Arc<Mutex<io::Stdout>>;
/// 来源初始化结果：采集源、真实采样率、session.open 的自述、失败原因。
type Origin = (Source, u32, Value, Option<String>);

fn main() {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let seq = Arc::new(AtomicU64::new(0));
    let lesson: Arc<Mutex<Option<LessonInfo>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));
    let cfg: Arc<Mutex<Value>> = Arc::new(Mutex::new(json!({})));

    let admit = Admit {
        proto: PROTO,
        adapter_id: ID.into(),
        version: "0.1.0".into(),
        manifest: Manifest {
            produces: vec![kinds::SESSION_OPEN.into(), kinds::AUDIO_CHUNK.into(), kinds::SESSION_CLOSE.into()],
            platforms: vec!["windows".into(), "linux".into()],
            needs_lesson: true,
            // 话轮粒度事件很稀；字节预算按 PCM 计，超了只记账，不静默丢音频。
            budget: Budget { max_events_per_s: 30, max_bytes_per_s: 400_000, on_exceed: Exceed::Log },
            restart: RestartPolicy { max_retries: 2, backoff_ms: 2_000 },
            notes: vec![
                "需要音频输入设备。无设备/未授权时整节不产出：这个源不会出现在导出的健康表里，stderr 给出原因与可选设备清单"
                    .into(),
                "blob 是单声道 PCM/WAV，未做 Opus：16 kHz 约 1.9 MB/分钟".into(),
                "params.fixture 指向 wav 时改走回放路径，供 CI 与无声卡环境验证同一条链路".into(),
                "不重采样：设备给什么采样率就按什么记，真实值随每个分段上报".into(),
            ],
        },
    };
    write_raw(&out, &serde_json::to_string(&admit).unwrap_or_else(|_| "{}".into()));

    {
        let (lesson, cfg, stop, quit) = (lesson.clone(), cfg.clone(), stop.clone(), quit.clone());
        std::thread::spawn(move || {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                let Ok(cmd) = serde_json::from_str::<Command>(&line) else {
                    eprintln!("[{ID}] 忽略无法解析的指令");
                    continue;
                };
                match cmd {
                    Command::Configure { data_dir, params } => {
                        *cfg.lock().unwrap() = params;
                        eprintln!("[{ID}] 已配置 data_dir={data_dir}");
                    }
                    Command::StartLesson { lesson: l } => *lesson.lock().unwrap() = Some(l),
                    Command::StopLesson { lesson_id, reason } => {
                        eprintln!("[{ID}] 收课 {lesson_id}（{reason}）");
                        stop.store(true, Ordering::SeqCst);
                    }
                    Command::Stop { reason } => {
                        eprintln!("[{ID}] 退出：{reason}");
                        // 不在这里 exit：直接退出会把已攒下的尾段和 session.close 一起丢。
                        // 只标个记号，交给主循环收完尾后再走。
                        quit.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            }
            std::process::exit(0);
        });
    }

    let mut session: Option<Session> = None;
    loop {
        if let Some(l) = lesson.lock().unwrap().take() {
            if let Some(mut s) = session.take() {
                s.finish(&out, &seq);
            }
            session = Some(Session::start(&out, &seq, cfg.lock().unwrap().clone(), l));
        }
        let mut closed = false;
        if let Some(s) = session.as_mut() {
            if stop.load(Ordering::SeqCst) {
                s.request_stop();
                stop.store(false, Ordering::SeqCst);
            }
            s.pump(&out, &seq);
            closed = s.done;
        } else {
            if quit.load(Ordering::SeqCst) {
                // 没有进行中的会话：没人在等尾段，可以直接走。
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if closed {
            if let Some(mut s) = session.take() {
                s.finish(&out, &seq);
            }
        }
        if quit.load(Ordering::SeqCst) {
            // 收课命令到了：先把当前会话刷完再退，不让最后一句消失在退出路上。
            if let Some(mut s) = session.take() {
                s.request_stop();
                s.pump(&out, &seq);
                s.finish(&out, &seq);
            }
            break;
        }
    }
    std::process::exit(0);
}

fn push(out: &Out, seq: &AtomicU64, kind: &str, t_event_ms: u64, payload: Value) {
    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let env = Envelope::new(n, kind, Some(t_event_ms), payload);
    if let Ok(text) = serde_json::to_string(&env) {
        write_raw(out, &text);
    }
}

fn write_raw(out: &Out, text: &str) {
    let mut g = out.lock().unwrap();
    let _ = g.write_all(text.as_bytes());
    let _ = g.write_all(b"\n");
    let _ = g.flush();
}

/// 采样从哪儿来。三种来源共用同一条 VAD 管线。
enum Source {
    Device { _cap: capture::Captured, rx: Receiver<Vec<i16>> },
    /// 回放：单声道 pcm、已读位置、起始墙钟、加速倍率
    Fixture { pcm: Arc<Vec<i16>>, pos: usize, t0: Instant, speed: f64 },
    /// 起不动（无设备、fixture 读不到）：只保持心跳，不产出。
    Dead,
}

impl Source {
    /// 往攒帧缓冲里补数据。返回 false 表示这条来源已耗尽（fixture 读完了）。
    fn fill(&mut self, frames: &mut FrameStream, want: usize, rate: u32) -> bool {
        match self {
            Source::Device { rx, .. } => {
                while frames.buffered() < want {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(v) => frames.feed(&v),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        // 发送端没了 = 采集线程死了。当作没有新音频，交给下课收尾，绝不假装有声音。
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            eprintln!("[{ID}] 采集流已断开，不再有新音频");
                            break;
                        }
                    }
                }
                true
            }
            Source::Fixture { pcm, pos, t0, speed } => {
                if *pos >= pcm.len() {
                    return false;
                }
                // 不限速的话 45 分钟会在两秒内灌完，节奏和预算就都测不出来了。
                let class_ms = *pos as f64 * 1_000.0 / rate.max(1) as f64;
                let due = *t0 + Duration::from_millis((class_ms / speed.max(1.0)) as u64);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                let end = (*pos + want).min(pcm.len());
                frames.feed(&pcm[*pos..end]);
                *pos = end;
                true
            }
            Source::Dead => {
                std::thread::sleep(Duration::from_millis(200));
                true
            }
        }
    }
}

/// 一节课的采集会话。
struct Session {
    lesson_id: String,
    blob_dir: PathBuf,
    rate: u32,
    frames: FrameStream,
    vad: Vad,
    emit_silence: bool,
    src: Source,
    started: Instant,
    chunks: u64,
    bytes: u64,
    voiced_ms: u64,
    silent_chunks: u64,
    failed: Option<String>,
    request_stop: bool,
    done: bool,
    closed: bool,
    /// session.open 到底有没有发出去。没开过场就不收场：
    /// 只要发了一条事件，这个源就会带着“已工作”的计数出现在导出里，
    /// 而“压根没跑起来”与“跑了但没采到”就被混成一回事了。
    opened: bool,
}

impl Session {
    fn start(out: &Out, seq: &AtomicU64, params: Value, l: LessonInfo) -> Session {
        let vcfg = VadConfig::from_json(params.get("vad"));
        let frame_ms = params.get("frame_ms").and_then(|v| v.as_u64()).unwrap_or(20);
        let want_rate = params.get("sample_rate").and_then(|v| v.as_u64()).unwrap_or(16_000) as u32;
        let emit_silence = params.get("emit_silence").and_then(|v| v.as_bool()).unwrap_or(false);
        let fixture = params.get("fixture").and_then(|v| v.as_str()).map(PathBuf::from);
        let use_fixture = match params.get("source").and_then(|v| v.as_str()) {
            Some("device") => false,
            Some("fixture") => true,
            _ => fixture.is_some(),
        };

        let (src, rate, note, failed) =
            if use_fixture { open_fixture(&params, fixture, want_rate) } else { open_device(want_rate) };

        let mut s = Session {
            lesson_id: l.lesson_id.clone(),
            blob_dir: PathBuf::from(&l.blob_dir),
            rate,
            frames: FrameStream::new(frame_ms),
            vad: Vad::new(vcfg.clone(), rate, 1),
            emit_silence,
            src,
            started: Instant::now(),
            chunks: 0,
            bytes: 0,
            voiced_ms: 0,
            silent_chunks: 0,
            failed,
            request_stop: false,
            done: false,
            closed: false,
            opened: false,
        };
        if s.failed.is_some() {
            // 刻意不发 session.open：健康表是按“有事件的源”建键的，一个全程没发事件的源
            // 会直接从表里缺席——“这个源今天没干活”必须看得见，而不是被一条开场事件盖过去。
            eprintln!("[{ID}] 无法开始采集：{}", s.failed.as_ref().unwrap());
            s.done = true;
            return s;
        }
        let mut note = note;
        if let Some(o) = note.as_object_mut() {
            o.insert("source".into(), json!(ID));
            o.insert("lesson_id".into(), json!(s.lesson_id));
            o.insert("channels".into(), json!(1));
            o.insert("codec".into(), json!("pcm_s16le(wav)"));
            o.insert("frame_ms".into(), json!(frame_ms));
            o.insert("vad".into(), vad_json(&vcfg));
        }
        s.opened = true;
        push(out, seq, kinds::SESSION_OPEN, 0, note);
        s
    }

    fn request_stop(&mut self) {
        self.request_stop = true;
    }

    /// 采集后端报过的流错误次数（掉帧、设备被拔等）。回放路径恒为 0。
    fn stream_errors(&self) -> usize {
        match &self.src {
            Source::Device { _cap, .. } => _cap.stream_errors(),
            _ => 0,
        }
    }

    fn pump(&mut self, out: &Out, seq: &AtomicU64) {
        if self.done {
            return;
        }
        let want = self.frames.frame_samples(self.rate, 1);
        let alive = self.src.fill(&mut self.frames, want, self.rate);
        // 写成 loop + match，不写 while let：`self.frames.take()` 的 &mut 借用会贯穿
        // 整个循环体，而体内又要 &mut self 去推 VAD 与发事件，两者直接冲突。
        loop {
            let frame = self.frames.take(want);
            let Some(frame) = frame else { break };
            if let Some(seg) = self.vad.push(&frame) {
                self.emit(out, seq, seg);
            }
        }
        if !alive || self.request_stop {
            if let Some(seg) = self.vad.flush() {
                self.emit(out, seq, seg);
            }
            self.done = true;
        }
    }

    /// 一个切好的段：写 blob + 报一条 audio.chunk。
    fn emit(&mut self, out: &Out, seq: &AtomicU64, seg: Segment) {
        let voiced = self.vad.accept(&seg);
        if voiced {
            self.voiced_ms += seg.speech_ms;
        } else if !self.emit_silence {
            // 短促噪音（关门、翻书）默认不落盘：否则 track 会被没有语义的碎段填满，
            // "这节课讲了什么"反而看不出来。
            return;
        }
        if seg.samples.is_empty() {
            return;
        }
        let name = format!("audio-{:012}ms.wav", seg.t0_ms);
        let len = match wav::write(&self.blob_dir.join(&name), self.rate, 1, &seg.samples) {
            Ok(n) => n,
            Err(e) => {
                // 写不进去就一条都别报：报了引用却没有文件，下游会在"有音频"和"能听"之间两难。
                eprintln!("[{ID}] 写 blob {name} 失败：{e}");
                self.failed = Some(format!("写 blob 失败：{e}"));
                self.done = true;
                return;
            }
        };
        let ck = AudioChunk {
            blob: name,
            len,
            codec: "pcm_s16le".into(),
            sample_rate: self.rate,
            channels: 1,
            t0_ms: seg.t0_ms,
            dur_ms: seg.dur_ms,
            silent: !voiced,
        };
        let mut payload = serde_json::to_value(&ck).unwrap_or(json!({}));
        if let Some(o) = payload.as_object_mut() {
            // AudioChunk 没有 deny_unknown_fields：多出来的是采集端自己算出来的证据，
            // 下游不认识也能原样存档，认识就能判"这教室是安静还是吵"。
            o.insert("rms".into(), json!(seg.rms.round()));
            o.insert("peak".into(), json!(seg.peak));
            o.insert("speech_ms".into(), json!(seg.speech_ms));
            o.insert("trigger".into(), json!("vad"));
            o.insert("source".into(), json!(ID));
        }
        push(out, seq, kinds::AUDIO_CHUNK, seg.t0_ms, payload);
        self.chunks += 1;
        self.bytes += len;
        if !voiced {
            self.silent_chunks += 1;
        }
    }

    fn finish(&mut self, out: &Out, seq: &AtomicU64) {
        if self.closed {
            return;
        }
        self.closed = true;
        if !self.opened {
            // 没开过场的会话不收场，也不写任何事件：让“本机没麦克风”这类情况
            // 以“这个源不在健康表里”的形式原样暴露给导出与看板。
            return;
        }
        let wall_ms = self.started.elapsed().as_millis() as u64;
        let stream_errors = self.stream_errors();
        push(
            out,
            seq,
            kinds::SESSION_CLOSE,
            wall_ms,
            json!({
                "source": ID, "lesson_id": self.lesson_id,
                "chunks": self.chunks, "bytes": self.bytes, "silent_chunks": self.silent_chunks,
                "voiced_ms": self.voiced_ms, "dropped_short": self.vad.dropped_short(),
                "audio_ms": self.vad.consumed_ms(), "wall_ms": wall_ms,
                // 掉过几次帧。必须进数据而不是只进 stderr：时长类结论（谁讲了多久、
                // 课堂话语占比）在掉帧的课上是不成立的，课后复盘得能看到这个前提。
                "stream_errors": stream_errors,
                "error": self.failed,
            }),
        );
        match &self.failed {
            Some(e) => eprintln!("[{ID}] 本节课未产出音频（{e}）"),
            None => eprintln!(
                "[{ID}] 收课 {}：{} 段 / {} 字节，语音 {}ms（静音段 {}，短促丢弃 {}，流错误 {}）",
                self.lesson_id, self.chunks, self.bytes, self.voiced_ms, self.silent_chunks,
                self.vad.dropped_short(), stream_errors
            ),
        }
    }
}

/// 真设备：后台音频线程把单声道帧推进 channel，主循环按帧喂 VAD。
fn open_device(want_rate: u32) -> Origin {
    let (tx, rx) = mpsc::channel::<Vec<i16>>();
    match capture::open(tx) {
        Ok(cap) => {
            let rate = cap.sample_rate;
            let note = json!({
                "input": "device", "device": cap.device, "sample_format": cap.format,
                "device_channels": cap.channels, "sample_rate": rate, "wanted_sample_rate": want_rate,
            });
            if rate != want_rate {
                eprintln!("[{ID}] 设备原生 {rate} Hz（params.sample_rate={want_rate}）：不重采样，按真实值记录");
            }
            (Source::Device { _cap: cap, rx }, rate, note, None)
        }
        Err(e) => (Source::Dead, want_rate, json!({ "input": "device" }), Some(e)),
    }
}

/// 回放一个 wav：与设备路径唯一的差别就是采样来自文件。
fn open_fixture(params: &Value, path: Option<PathBuf>, want_rate: u32) -> Origin {
    let Some(path) = path else {
        return (Source::Dead, want_rate, json!({ "input": "fixture" }), Some("source=fixture 但没给 params.fixture".into()));
    };
    match wav::read(&path) {
        Err(e) => (
            Source::Dead,
            want_rate,
            json!({ "input": "fixture", "path": path.display().to_string() }),
            Some(format!("读不到 fixture：{e}")),
        ),
        Ok(pcm) => {
            let rate = pcm.sample_rate;
            let mono = capture::downmix(&pcm.samples, pcm.channels);
            let frames = mono.len();
            let speed = params.get("speed").and_then(|v| v.as_f64()).unwrap_or(200.0).max(1.0);
            let note = json!({
                "input": "fixture", "path": path.display().to_string(),
                "file_channels": pcm.channels, "sample_rate": rate, "wanted_sample_rate": want_rate,
                "speed": speed, "file_samples": frames,
            });
            eprintln!(
                "[{ID}] 回放 {}：{rate} Hz，{} 声道→单声道，{frames} 采样，{speed}x 速度",
                path.display(),
                pcm.channels
            );
            (Source::Fixture { pcm: Arc::new(mono), pos: 0, t0: Instant::now(), speed }, rate, note, None)
        }
    }
}

fn vad_json(c: &VadConfig) -> Value {
    json!({
        "frame_ms": c.frame_ms, "rms_open": c.rms_open, "rms_close": c.rms_close,
        "hangover_ms": c.hangover_ms, "preroll_ms": c.preroll_ms,
        "min_speech_ms": c.min_speech_ms, "max_segment_ms": c.max_segment_ms,
    })
}
