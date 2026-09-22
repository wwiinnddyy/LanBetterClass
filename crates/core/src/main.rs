//! 采集核心。它只认识"事件"，不认识任何具体数据源。
//!
//! 三件事构成它的全部职责：起子进程并监督、按 envelope 落盘、把一节课导出成
//! AI 能消费的 `ai_payload.json`。平台差异（DXGI / PipeWire / WASAPI）一律关在
//! 适配器里，所以新增一个学校环境不需要重编译这里。

mod protocol;
mod store;
mod supervisor;
mod timeline;

use classagent_schema::{kinds, Admit, Command, Envelope, LessonInfo, PROTO};
use protocol::shorten;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use store::Store;
use supervisor::{discover, Inbound, Status, Supervisor};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = argv.first().filter(|a| !a.starts_with("--")).map(|s| s.as_str()).unwrap_or("run");
    let opts = parse_opts(if argv.first().map(|a| a.starts_with("--")).unwrap_or(true) { argv.as_slice() } else { &argv[1..] });
    let data = PathBuf::from(opts.val("data").unwrap_or("data"));
    let result = match cmd {
        "run" => run(opts, data),
        "status" => status(data),
        "export" => export(opts, data),
        other => {
            eprintln!("未知命令 {other}\n用法：classagent-core [run|status|export] [--data DIR] [--adapters DIR]");
            eprintln!("  run    --lesson FILE.json  启动即开课；控制台可输入 start/stop/status/quit");
            eprintln!("         --max-seconds N     N 秒后自动收尾退出（脚本化验证用）");
            eprintln!("  export --lesson ID [--out PATH]   导出 AI 载荷");
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("[core] 错误：{e}");
        std::process::exit(1);
    }
}

/// 新type 而不是 `type Opts = HashMap<...>`：对别名写 inherent impl 就是给别的
/// crate 的类型定义方法，E0116。
#[derive(Default)]
struct Opts {
    m: HashMap<String, String>,
}

impl Opts {
    fn val(&self, key: &str) -> Option<&str> {
        self.m.get(key).map(|s| s.as_str())
    }

    fn num(&self, key: &str) -> Option<u64> {
        self.m.get(key).and_then(|v| v.parse().ok())
    }
}

fn parse_opts(args: &[String]) -> Opts {
    let mut m = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(key) = a.strip_prefix("--") {
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                m.insert(key.to_string(), args[i + 1].clone());
                i += 2;
                continue;
            }
            m.insert(key.to_string(), "1".to_string());
        }
        i += 1;
    }
    Opts { m }
}

fn run(opts: Opts, data: PathBuf) -> std::io::Result<()> {
    let adapters_dir = PathBuf::from(opts.val("adapters").unwrap_or("adapters.d"));
    let mut store = Store::open(&data)?;
    let (specs, skipped) = discover(&adapters_dir)?;
    for s in &skipped {
        eprintln!("[core] 未装载 {s}");
    }
    if specs.is_empty() {
        eprintln!("[core] {} 下没有启用的 *.adapter.json，将只等待控制台指令", adapters_dir.display());
    }

    let (tx, rx): (Sender<Inbound>, Receiver<Inbound>) = mpsc::channel();
    let mut sup = Supervisor::new(tx.clone(), store.root.clone());
    for spec in specs {
        if let Err(e) = sup.launch(spec) {
            eprintln!("[core] {e}");
        }
    }

    {
        let tx2 = tx.clone();
        thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(l) => {
                        if tx2.send(Inbound::Console { line: l }).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    drop(tx);

    // 核心自己写日志用独立的 seq 空间，由 Store 保管（见 Store::note）。
    let mut quit = false;
    let deadline = opts.num("max-seconds").map(|s| Instant::now() + Duration::from_secs(s));
    let mut last_report = Instant::now();
    let pending_lesson: Option<LessonInfo> = match opts.val("lesson") {
        Some(f) => match std::fs::read_to_string(f) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => Some(lesson_from_value(&v, None)),
                Err(e) => {
                    eprintln!("[core] {f} 不是合法 JSON（{e}），忽略");
                    None
                }
            },
            Err(e) => {
                eprintln!("[core] 读不到 {f}（{e}），忽略");
                None
            }
        },
        None => None,
    };
    if let Some(info) = pending_lesson {
        start_lesson(&mut store, &mut sup, info);
    }

    while !quit {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Inbound::Line { adapter_id, value }) => {
                let pending = sup.find(&adapter_id).map(|h| h.status == Status::Pending).unwrap_or(false);
                if pending {
                    match serde_json::from_value::<Admit>(value.clone()) {
                        Ok(a) => {
                            store.register_adapter(&adapter_id, a.manifest.budget.clone());
                            let _ = store.note(
                                kinds::CORE_ADMIT,
                                serde_json::json!({ "adapter_id": adapter_id, "manifest": a.manifest }),
                            );
                            match sup.handle(&adapter_id) {
                                Some(h) => {
                                    if let Err(e) = h.admit(&a) {
                                        eprintln!("[core] {adapter_id} 拒绝装载：{e}");
                                    }
                                }
                                None => eprintln!("[core] {adapter_id} 报了 Admit 但没有对应句柄，忽略"),
                            }
                            if let Some(h) = sup.find(&adapter_id) {
                                if h.status == Status::Ready {
                                    let produces = h.manifest.clone().map(|m| m.produces).unwrap_or_default();
                                    let needs = h.manifest.clone().map(|m| m.needs_lesson).unwrap_or(false);
                                    eprintln!("[core] {adapter_id} 就绪 produces={:?} needs_lesson={needs}", produces.join(","));
                                    if !needs || store.active_lesson().is_some() {
                                        if let Some(li) = store.active_lesson().map(|m| m.info.clone()) {
                                            let _ = sup.send(&adapter_id, Command::StartLesson { lesson: li });
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => eprintln!("[core] {adapter_id} 首行不是 Admit：{}", shorten(&e.to_string(), 200)),
                    }
                } else {
                    match serde_json::from_value::<Envelope>(value.clone()) {
                        Ok(env) => {
                            let out = store.append(&adapter_id, &env)?;
                            if let Some(lost) = out.gap {
                                eprintln!("[core] {adapter_id} seq 空洞：丢 {lost} 条（status={:?}）", out.status);
                            }
                            if out.kill {
                                eprintln!("[core] {adapter_id} 超出预算，已终止且不再拉起");
                                sup.kill(&adapter_id, "超出预算");
                            }
                        }
                        Err(e) => {
                            let _ = store.append_rejected(&adapter_id, &value, &e.to_string());
                            eprintln!("[core] {adapter_id} 一条事件解析失败，已原样保留");
                        }
                    }
                }
            }
            Ok(Inbound::Eof { adapter_id }) => {
                // 只报不杀：真正判活死交给 reap()，避免把"自己退出"的适配器误判成崩溃。
                eprintln!("[core] {adapter_id} 关闭了 stdout");
            }
            Ok(Inbound::Err { adapter_id, msg }) => {
                eprintln!("[core] {adapter_id} 读取错误：{}", shorten(&msg, 200));
            }
            Ok(Inbound::Console { line }) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                let (head, rest) = match line.find(' ') {
                    Some(i) => (line[..i].to_string(), line[i + 1..].to_string()),
                    None => (line.clone(), String::new()),
                };
                match head.as_str() {
                    "start" => {
                        let prev = store.active_lesson_id().map(|s| s.to_string());
                        let v: serde_json::Value = serde_json::from_str(&rest).unwrap_or(serde_json::json!({}));
                        let info = lesson_from_value(&v, prev.filter(|p| !p.is_empty()).map(|p| p.to_string()));
                        start_lesson(&mut store, &mut sup, info);
                    }
                    "stop" => {
                        stop_lesson(&mut store, &mut sup, "console");
                    }
                    "status" => print_status(&store, &sup),
                    "quit" | "exit" => quit = true,
                    other => eprintln!("[core] 未知指令 {other}（可用：start/stop/status/quit）"),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        for (id, code) in sup.reap() {
            let will_restart = sup.find(&id).map(|h| h.status == Status::Backoff).unwrap_or(false);
            eprintln!("[core] {id} 退出（{code:?}）{}", if will_restart { "，退避后重启" } else { "" });
            if will_restart {
                store.note_respawn(&id)?;
            }
        }
        for up in sup.respawn_due() {
            eprintln!("[core] {up} 已重启");
        }
        store.tick()?;

        if let Some(d) = deadline {
            if Instant::now() >= d {
                eprintln!("[core] 到达 --max-seconds，收尾");
                stop_lesson(&mut store, &mut sup, "max_seconds");
                quit = true;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            print_status(&store, &sup);
        }
    }

    if store.active_lesson().is_some() {
        stop_lesson(&mut store, &mut sup, "core-exit");
    }
    sup.shutdown("core 退出");
    store.flush()?;
    eprintln!("[core] 已收尾，数据在 {}", store.root.display());
    Ok(())
}

fn start_lesson(store: &mut Store, sup: &mut Supervisor, info: LessonInfo) {
    let id = info.lesson_id.clone();
    match store.begin_lesson(info) {
        Ok(()) => {
            eprintln!("[core] 开课 {id}，blob 目录 {}", store.active_lesson().map(|m| m.info.blob_dir.clone()).unwrap_or_default());
            let li = store.active_lesson().map(|m| m.info.clone());
            let mut sent = 0usize;
            for h in sup.ids() {
                if let Some(li) = &li {
                    if sup.send(&h, Command::StartLesson { lesson: li.clone() }).is_ok() {
                        sent += 1;
                    }
                }
            }
            eprintln!("[core] StartLesson 已送达 {sent} 个适配器");
        }
        Err(e) => eprintln!("[core] 开课失败：{e}"),
    }
}

fn stop_lesson(store: &mut Store, sup: &mut Supervisor, reason: &str) {
    if let Some(m) = store.active_lesson() {
        let id = m.info.lesson_id.clone();
        let stats = store.stats();
        let (ev, bytes): (u64, u64) = stats.iter().fold((0, 0), |a, s| (a.0 + s.1.events, a.1 + s.1.bytes));
        eprintln!("[core] 收课 {id}：{ev} 条事件，{bytes} 字节；导出用 --lesson {id}");
        for h in sup.ids() {
            let _ = sup.send(&h, Command::StopLesson { lesson_id: id.clone(), reason: reason.to_string() });
        }
    }
    if let Err(e) = store.end_lesson(reason) {
        eprintln!("[core] 收尾写盘失败：{e}");
    }
}

fn print_status(store: &Store, sup: &Supervisor) {
    let lesson = store.active_lesson().map(|m| m.info.lesson_id.clone()).unwrap_or_else(|| "—".into());
    println!(
        "[core] {} 课={lesson} proto=v{PROTO}",
        hhmmss(classagent_schema::utc_ms())
    );
    for (id, st) in store.stats() {
        let status = sup.find(&id).map(|h| format!("{:?}", h.status)).unwrap_or_else(|| "未装载".into());
        let note = sup.find(&id).and_then(|h| h.note.clone()).unwrap_or_default();
        println!(
            "  {id:<16} {status:<9} ev={:<8} {:>7} 缺口={} 超预算={} {}",
            st.events,
            human_bytes(st.bytes),
            st.gaps,
            st.exceeded,
            note
        );
    }
    for h in sup.status_table() {
        if !store.stats().iter().any(|(id, _)| *id == h.0) {
            println!("  {:<16} {:?} {}", h.0, h.1, h.2.unwrap_or_default());
        }
    }
}

fn hhmmss(utc_ms: u64) -> String {
    let s = (utc_ms / 1000) % 86_400;
    format!("{:02}:{:02}:{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

fn human_bytes(b: u64) -> String {
    if b < 1024 {
        format!("{b}B")
    } else if b < 1024 * 1024 {
        format!("{:.0}KB", b as f64 / 1024.0)
    } else {
        format!("{:.1}MB", b as f64 / 1048576.0)
    }
}

/// 宽容解析：允许只给 `{"subject":"数学"}` 这种最小输入。
/// 直接 `from_value::<LessonInfo>` 会因为缺字段整条失败，那会让现场手测很痛。
fn lesson_from_value(v: &serde_json::Value, prev: Option<String>) -> LessonInfo {
    let now_ms = classagent_schema::utc_ms();
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    LessonInfo {
        lesson_id: s("lesson_id").unwrap_or_else(|| format!("L{}", now_ms / 1000)),
        prev_lesson_id: s("prev_lesson_id").or(prev),
        subject: s("subject"),
        class: s("class"),
        teacher: s("teacher"),
        started_at_utc_ms: v.get("started_at_utc_ms").and_then(|x| x.as_u64()).unwrap_or(now_ms),
        courseware: v
            .get("courseware")
            .and_then(|x| serde_json::from_value::<Vec<classagent_schema::CoursewareRef>>(x.clone()).ok())
            .unwrap_or_default(),
        blob_dir: String::new(),
        params: v.get("params").cloned().unwrap_or(serde_json::Value::Null),
    }
}

fn status(data: PathBuf) -> std::io::Result<()> {
    let ids = store::lesson_ids(&data);
    if ids.is_empty() {
        println!("{} 下还没有任何课", data.display());
        return Ok(());
    }
    for id in ids {
        let meta = store::read_meta(&data, &id)?;
        let size = std::fs::metadata(data.join("lessons").join(&id).join("events.ndjson")).map(|m| m.len()).unwrap_or(0);
        match meta {
            Some(m) => {
                let dur = m
                    .ended_core_mono_us
                    .map(|e| (e - m.started_core_mono_us) / 1_000_000)
                    .map(|s| format!("{s}s"))
                    .unwrap_or_else(|| "进行中".into());
                println!(
                    "{id}  {} {}  {:>6}  停止={}",
                    m.info.subject.clone().unwrap_or_else(|| "-".into()),
                    m.info.class.clone().unwrap_or_else(|| "-".into()),
                    human_bytes(size),
                    m.stop_reason.clone().unwrap_or_else(|| "-".into()),
                );
                println!("   时长={dur} 课件={} blob={}", m.info.courseware.len(), m.info.blob_dir);
            }
            None => println!("{id} 缺 meta.json（事件 {}）", human_bytes(size)),
        }
    }
    Ok(())
}

fn export(opts: Opts, data: PathBuf) -> std::io::Result<()> {
    let id = match opts.val("lesson") {
        Some(v) => v.to_string(),
        None => {
            eprintln!("export 需要 --lesson ID");
            std::process::exit(2);
        }
    };
    let id = id.to_string();
    let meta = store::read_meta(&data, &id)?;
    let meta = match meta {
        Some(m) => m,
        None => {
            eprintln!("{id} 没有 meta.json，无法确定时间原点");
            std::process::exit(2);
        }
    };
    let (records, bad) = store::read_records(&data, &id)?;
    if records.is_empty() {
        eprintln!("{id} 没有事件，导出的载荷会是空 track");
    }
    let payload = timeline::build(&meta, &records);
    let text = serde_json::to_string_pretty(&payload)?;
    let out_path = match opts.val("out") {
        Some(p) => PathBuf::from(p),
        None => data.join("lessons").join(&id).join("ai_payload.json"),
    };
    let mut f = std::io::BufWriter::new(std::fs::File::create(&out_path)?);
    f.write_all(text.as_bytes())?;
    f.flush()?;
    println!(
        "[core] 导出 {}：track={} ink={} 语句={} 书写中讲话={}ms 坏行={}",
        out_path.display(),
        payload.track.len(),
        payload.stats.strokes,
        payload.stats.utterances,
        payload.stats.writing_while_speaking_ms,
        bad
    );
    for w in &payload.warnings {
        println!("[core] 警告：{w}");
    }
    for (s, h) in &payload.sources {
        if h.silent {
            println!("[core] 源 {s} 声明了 {:?} 却全程无事件", h.declared);
        }
    }
    Ok(())
}
