//! 模拟源。存在意义只有一个：在白板还没改造完、真机器还没到手之前，
//! 把"事件投递 → 缺口检测 → 落盘 → 时间轴对齐 → AI 载荷"整条路跑通。
//!
//! stdout 是协议通道，只能写 NDJSON；任何日志一律走 stderr。

use classagent_schema::{
    kinds, Admit, Budget, Command, Envelope, Exceed, LessonInfo, Manifest, PageActivate, RestartPolicy, StrokeCommit,
    StrokeDelete, Utterance, PROTO,
};
use serde_json::json;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ID: &str = "a-fake";

type Out = Arc<Mutex<io::Stdout>>;

fn main() {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let seq = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let lesson: Arc<Mutex<Option<LessonInfo>>> = Arc::new(Mutex::new(None));
    let cfg: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(json!({})));

    let manifest = Manifest {
        produces: vec![
            kinds::SESSION_OPEN.into(),
            kinds::INK_PAGE_ACTIVATE.into(),
            kinds::INK_STROKE_COMMIT.into(),
            kinds::INK_STROKE_DELETE.into(),
            kinds::ASR_UTTERANCE.into(),
        ],
        platforms: vec!["windows".into(), "linux".into()],
        needs_lesson: true,
        budget: Budget { max_events_per_s: 4_000, max_bytes_per_s: 4_000_000, on_exceed: Exceed::Degrade },
        restart: RestartPolicy { max_retries: 2, backoff_ms: 500 },
        notes: vec!["模拟源：数据全部由脚本生成，不能用于任何真实结论".into()],
    };
    let admit = Admit { proto: PROTO, adapter_id: ID.into(), version: "0.1.0".into(), manifest };
    // 第一行必须是 Admit，且不能和事件混用同一次写入。
    write_raw(&out, &serde_json::to_string(&admit).unwrap_or_else(|_| "{}".into()));

    {
        let (stop, lesson, cfg) = (stop.clone(), lesson.clone(), cfg.clone());
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
                    Command::StartLesson { lesson: l } => {
                        eprintln!("[{ID}] 收到开课 {}", l.lesson_id);
                        *lesson.lock().unwrap() = Some(l);
                    }
                    Command::StopLesson { lesson_id, reason } => {
                        eprintln!("[{ID}] 收到收课 {lesson_id}（{reason}）");
                    }
                    Command::Stop { reason } => {
                        eprintln!("[{ID}] 退出：{reason}");
                        stop.store(true, Ordering::SeqCst);
                        std::process::exit(0);
                    }
                }
            }
            // stdin 关闭 = 核心已经没了，别留孤儿进程。
            eprintln!("[{ID}] stdin 结束，退出");
            std::process::exit(0);
        });
    }

    while !stop.load(Ordering::SeqCst) {
        if let Some(l) = lesson.lock().unwrap().take() {
            run_lesson(&out, &seq, &cfg, &stop, &l);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_lesson(out: &Out, seq: &AtomicU64, cfg: &Arc<Mutex<serde_json::Value>>, stop: &AtomicBool, l: &LessonInfo) {
    let (speed, sim_minutes) = {
        let c = cfg.lock().unwrap();
        (
            c.get("speed").and_then(|v| v.as_f64()).unwrap_or(400.0).max(1.0),
            c.get("minutes").and_then(|v| v.as_f64()).unwrap_or(45.0),
        )
    };
    let class = l.class.clone().unwrap_or_else(|| "未命名".into());
    let subject = l.subject.clone().unwrap_or_else(|| "未命名".into());

    let t0 = Instant::now();
    let page_id = "p-fake-01";
    push(out, seq, kinds::SESSION_OPEN, 0, &json!({ "source": ID, "canvas_w": 1920.0, "canvas_h": 1080.0, "whiteboard_version": "simulated" }));

    let total_ms = (sim_minutes * 60_000.0) as u64;
    let step = 300u64;
    let mut t: u64 = 0;
    let mut n: u32 = 0;
    while t < total_ms && !stop.load(Ordering::SeqCst) {
        if n % 8 == 0 {
            let u = Utterance {
                t0_ms: t,
                t1_ms: t + 1_800,
                speaker: if n % 56 == 0 { "student" } else { "teacher" }.into(),
                text: format!("模拟句 {n}：{class} 的 {subject} 课，这里是第 {} 段讲解。", n / 5 + 1),
                confidence: Some(0.9),
                words: Vec::new(),
            };
            push(out, seq, kinds::ASR_UTTERANCE, t, &serde_json::to_value(&u).unwrap());
        }
        if n % 13 == 0 {
            let p = PageActivate { page_id: page_id.into(), index: n / 65, doc_id: None };
            push(out, seq, kinds::INK_PAGE_ACTIVATE, t, &serde_json::to_value(&p).unwrap());
        }
        if n % 13 == 3 {
            let s = StrokeCommit {
                stroke_id: format!("s-{n}"),
                page_id: page_id.into(),
                tool: if n % 143 == 3 { "highlighter" } else { "pen" }.into(),
                color: "#111111".into(),
                width: 3.0,
                layer: None,
                duration_ms: 380,
                points: (0..24)
                    .map(|i| [120.0 + i as f64 * 9.0, 200.0 + (i % 5) as f64 * 3.0, 0.35 + 0.02 * i as f64, i as f64 * 15.0])
                    .collect(),
                bbox: Some([120.0, 190.0, 336.0, 250.0]),
                decimation: Some("none".into()),
                viewport: Some([1.0, 0.0, 0.0]),
            };
            push(out, seq, kinds::INK_STROKE_COMMIT, t, &serde_json::to_value(&s).unwrap());
        }
        if n % 97 == 0 && n > 0 {
            let d = StrokeDelete { stroke_id: format!("s-{}", n - 1), page_id: page_id.into(), reason: "undo".into() };
            push(out, seq, kinds::INK_STROKE_DELETE, t + 200, &serde_json::to_value(&d).unwrap());
        }
        n += 1;
        t += step;
        // 按 1/speed 的墙钟节奏推进；落后了就立刻继续，宁可事件率被预算标记，
        // 也不要让一个模拟源真的跑 45 分钟。
        let due = t0 + Duration::from_millis((t as f64 / speed) as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
    }
    push(out, seq, kinds::SESSION_CLOSE, total_ms, &json!({ "source": ID, "simulated_ms": total_ms, "ticks": n }));
    eprintln!("[{ID}] 模拟课结束：{n} 个节拍，真实用时 {:.1}s", t0.elapsed().as_secs_f64());
}

fn push(out: &Out, seq: &AtomicU64, kind: &str, t_event_ms: u64, payload: &serde_json::Value) {
    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let env = Envelope::new(n, kind, Some(t_event_ms), payload.clone());
    match serde_json::to_string(&env) {
        Ok(text) => write_raw(out, &text),
        Err(e) => eprintln!("[{ID}] 事件序列化失败（{e}），已跳过"),
    }
}

fn write_raw(out: &Out, text: &str) {
    let mut g = out.lock().unwrap();
    let _ = g.write_all(text.as_bytes());
    let _ = g.write_all(b"\n");
    let _ = g.flush();
}
