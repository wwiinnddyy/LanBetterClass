//! 文件音频适配器。
//!
//! 它不代表最终形态（真形态是 WASAPI / PipeWire 实时采集），但它把两件最容易
//! 出错的事先钉住了：分段 blob 的落盘命名，以及 `audio.chunk` 只报引用不塞字节。
//! 云端转写回来后，也用同一个适配器身份把 `asr.utterance` 灌进这条总线，
//! 于是对齐逻辑只有一套，不必为云端结果另开一条数据通路。

use classagent_schema::{
    kinds, Admit, AudioChunk, Budget, Command, Envelope, Exceed, LessonInfo, Manifest, RestartPolicy, PROTO,
};
use serde_json::json;
use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ID: &str = "a-audiofile";

type Out = Arc<Mutex<io::Stdout>>;

fn main() {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let seq = Arc::new(AtomicU64::new(0));
    let lesson: Arc<Mutex<Option<LessonInfo>>> = Arc::new(Mutex::new(None));
    let cfg: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(json!({})));

    let manifest = Manifest {
        produces: vec![kinds::SESSION_OPEN.into(), kinds::AUDIO_CHUNK.into(), kinds::SESSION_CLOSE.into()],
        platforms: vec!["windows".into(), "linux".into()],
        needs_lesson: true,
        // 音频分段是这条总线里最大的一路事件，预算单列，别和笔迹共用默认值。
        budget: Budget { max_events_per_s: 60, max_bytes_per_s: 200_000, on_exceed: Exceed::Log },
        restart: RestartPolicy { max_retries: 1, backoff_ms: 2_000 },
        notes: vec!["需要在 params.path 指向一个本地音频文件，否则全程静默".into()],
    };
    let admit = Admit { proto: PROTO, adapter_id: ID.into(), version: "0.1.0".into(), manifest };
    write_raw(&out, &serde_json::to_string(&admit).unwrap_or_else(|_| "{}".into()));

    {
        let (lesson, cfg) = (lesson.clone(), cfg.clone());
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
                    Command::StopLesson { lesson_id, reason } => eprintln!("[{ID}] 收课 {lesson_id}（{reason}）"),
                    Command::Stop { reason } => {
                        eprintln!("[{ID}] 退出：{reason}");
                        std::process::exit(0);
                    }
                }
            }
            std::process::exit(0);
        });
    }

    loop {
        if let Some(l) = lesson.lock().unwrap().take() {
            let params = cfg.lock().unwrap().clone();
            pump(&out, &seq, &params, &l);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn push(out: &Out, seq: &AtomicU64, kind: &str, t_event_ms: u64, payload: &serde_json::Value) {
    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let env = Envelope::new(n, kind, Some(t_event_ms), payload.clone());
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

fn pump(out: &Out, seq: &AtomicU64, c: &serde_json::Value, l: &LessonInfo) {
    let Some(path) = c.get("path").and_then(|v| v.as_str()) else {
        eprintln!("[{ID}] params.path 未配置，本节课不产出音频分段");
        return;
    };
    if l.blob_dir.is_empty() {
        eprintln!("[{ID}] 核心没给 blob_dir，无法写分段");
        return;
    }
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[{ID}] 打不开 {path}（{e}）");
            return;
        }
    };

    let codec = c.get("codec").and_then(|v| v.as_str()).unwrap_or("pcm_s16le").to_string();
    let sample_rate = c.get("sample_rate").and_then(|v| v.as_u64()).unwrap_or(16_000) as u32;
    let channels = c.get("channels").and_then(|v| v.as_u64()).unwrap_or(1) as u16;
    let chunk_ms = c.get("chunk_ms").and_then(|v| v.as_u64()).unwrap_or(1_000).max(1);
    // 未显式给定时按 s16le 推算：sr × ch × 2 字节/秒。
    let bytes_per_chunk = c
        .get("bytes_per_chunk")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| ((sample_rate as u64) * (channels as u64) * 2 * chunk_ms / 1_000).max(1))
        .max(1) as usize;
    let speed = c.get("speed").and_then(|v| v.as_f64()).unwrap_or(400.0).max(1.0);
    let ext = match c.get("ext").and_then(|v| v.as_str()) {
        Some(e) => e.to_string(),
        None => if codec.starts_with("pcm") { "pcm".to_string() } else { "bin".to_string() },
    };

    push(
        out,
        seq,
        kinds::SESSION_OPEN,
        0,
        &json!({ "source": ID, "path": path, "codec": codec, "sample_rate": sample_rate, "channels": channels, "chunk_ms": chunk_ms }),
    );

    let t0 = Instant::now();
    let mut t_ms: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut chunks: u64 = 0;
    let mut buf = vec![0u8; bytes_per_chunk];
    loop {
        let mut filled = 0usize;
        let mut io_failed = false;
        while filled < bytes_per_chunk {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("[{ID}] 读 {path} 出错（{e}），在 {t_ms}ms 处停下");
                    io_failed = true;
                    break;
                }
            }
        }
        // 已经读到的部分照样落盘：宁可最后一段短一点，也不要因为一次 IO 错误丢掉整段。
        if filled == 0 {
            break;
        }
        let name = format!("audio-{t_ms:012}ms.{ext}");
        let blob_path = Path::new(&l.blob_dir).join(&name);
        if let Err(e) = std::fs::write(&blob_path, &buf[..filled]) {
            eprintln!("[{ID}] 写 blob 失败（{e}），停止");
            break;
        }
        let dur_ms = ((filled as f64 / bytes_per_chunk as f64) * chunk_ms as f64) as u64;
        total_bytes += filled as u64;
        chunks += 1;
        let ck = AudioChunk {
            blob: name,
            len: filled as u64,
            codec: codec.clone(),
            sample_rate,
            channels,
            t0_ms: t_ms,
            dur_ms,
            silent: false,
        };
        push(out, seq, kinds::AUDIO_CHUNK, t_ms, &serde_json::to_value(&ck).unwrap_or(serde_json::Value::Null));
        t_ms += dur_ms.max(1);
        if io_failed {
            break;
        }

        let due = t0 + Duration::from_millis((t_ms as f64 / speed) as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
    }

    push(
        out,
        seq,
        kinds::SESSION_CLOSE,
        t_ms,
        &json!({ "source": ID, "chunks": chunks, "bytes": total_bytes, "wall_s": t0.elapsed().as_secs_f64() }),
    );
    eprintln!("[{ID}] 推完 {chunks} 段 / {total_bytes} 字节，课堂时长约 {t_ms}ms");
}
