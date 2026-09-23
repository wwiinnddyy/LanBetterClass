//! 采集核心。它只认识"事件"，不认识任何具体数据源。
//!
//! 三件事构成它的全部职责：起子进程并监督、按 envelope 落盘、把一节课导出成
//! AI 能消费的 `ai_payload.json`。平台差异（DXGI / PipeWire / WASAPI）一律关在
//! 适配器里，所以新增一个学校环境不需要重编译这里。

use classagent_client::{digest, protocol, push, serve, store, supervisor, timeline};

use classagent_schema::{kinds, Admit, Command, Envelope, LessonInfo, LessonUpload, PROTO};
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
        "digest" => digest_cmd(opts, data),
        "serve" => serve_cmd(opts, data),
        "push" => push_cmd(opts, data),
        other => {
            eprintln!("未知命令 {other}\n用法：classagent-client [run|status|export|digest|serve|push] [--data DIR] [--adapters DIR]");
            eprintln!("  run    --lesson FILE.json  启动即开课；控制台可输入 start/stop/status/quit");
            eprintln!("         --max-seconds N     N 秒后自动收尾退出（脚本化验证用）");
            eprintln!("  export --lesson ID [--out PATH]   导出 AI 载荷");
            eprintln!("  digest --lesson ID [--out PATH]   一节课的可读摘要（不接模型也能读）");
            eprintln!("  serve  [--host 127.0.0.1] [--port 8786] [--allow-write]");
            eprintln!("         本地看板：/ 是页面，/api/lesson/ID/digest、/stats、/blob/名字 是数据。");
            eprintln!("         默认只绑本机；只有 --allow-write 才接受改数据源开关的 POST。");
            eprintln!("  push   --lesson ID --server HOST:PORT [--token SECRET] [--path /api/ingest]");
            eprintln!("         把导出的 ai_payload 通过 HTTP POST 推给远程 classagent-server。");
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("[client] 错误：{e}");
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

    fn flag(&self, key: &str) -> bool {
        self.m.contains_key(key)
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
        eprintln!("[client] 未装载 {s}");
    }
    if specs.is_empty() {
        eprintln!("[client] {} 下没有启用的 *.adapter.json，将只等待控制台指令", adapters_dir.display());
    }

    let (tx, rx): (Sender<Inbound>, Receiver<Inbound>) = mpsc::channel();
    let mut sup = Supervisor::new(tx.clone(), store.root.clone());
    for spec in specs {
        if let Err(e) = sup.launch(spec) {
            eprintln!("[client] {e}");
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
                    eprintln!("[client] {f} 不是合法 JSON（{e}），忽略");
                    None
                }
            },
            Err(e) => {
                eprintln!("[client] 读不到 {f}（{e}），忽略");
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
            Ok(ib) => handle_inbound(ib, &mut store, &mut sup, &rx, &mut quit, false)?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        for (id, code) in sup.reap() {
            let will_restart = sup.find(&id).map(|h| h.status == Status::Backoff).unwrap_or(false);
            eprintln!("[client] {id} 退出（{code:?}）{}", if will_restart { "，退避后重启" } else { "" });
            if will_restart {
                store.note_respawn(&id)?;
            }
        }
        for up in sup.respawn_due() {
            eprintln!("[client] {up} 已重启");
        }
        store.tick()?;

        if let Some(d) = deadline {
            if Instant::now() >= d {
                eprintln!("[client] 到达 --max-seconds，收尾");
                stop_lesson(&mut store, &mut sup, &rx, &mut quit, "max_seconds", false);
                quit = true;
            }
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            print_status(&store, &sup);
        }
    }

    if store.active_lesson().is_some() {
        stop_lesson(&mut store, &mut sup, &rx, &mut quit, "core-exit", false);
    }
    sup.shutdown("core 退出");
    store.flush()?;
    eprintln!("[client] 已收尾，数据在 {}", store.root.display());
    Ok(())
}

/// 处理一条入站消息。
///
/// `nested` 表示此刻已经在等适配器收尾了：再收到 stop 只关门、不再等一轮，
/// 否则 stop → 等尾巴 → 又收到 stop 会递归下去。
fn handle_inbound(
    ib: Inbound,
    store: &mut Store,
    sup: &mut Supervisor,
    rx: &Receiver<Inbound>,
    quit: &mut bool,
    nested: bool,
) -> std::io::Result<()> {
    match ib {
        Inbound::Line { adapter_id, value } => {
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
                                    eprintln!("[client] {adapter_id} 拒绝装载：{e}");
                                }
                            }
                            None => eprintln!("[client] {adapter_id} 报了 Admit 但没有对应句柄，忽略"),
                        }
                        if let Some(h) = sup.find(&adapter_id) {
                            if h.status == Status::Ready {
                                let produces = h.manifest.clone().map(|m| m.produces).unwrap_or_default();
                                let needs = h.manifest.clone().map(|m| m.needs_lesson).unwrap_or(false);
                                eprintln!("[client] {adapter_id} 就绪 produces={:?} needs_lesson={needs}", produces.join(","));
                                if !needs || store.active_lesson().is_some() {
                                    if let Some(li) = store.active_lesson().map(|m| m.info.clone()) {
                                        let _ = sup.send(&adapter_id, Command::StartLesson { lesson: li });
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("[client] {adapter_id} 首行不是 Admit：{}", shorten(&e.to_string(), 200)),
                }
            } else {
                match serde_json::from_value::<Envelope>(value.clone()) {
                    Ok(env) => {
                        let out = store.append(&adapter_id, &env)?;
                        if let Some(lost) = out.gap {
                            eprintln!("[client] {adapter_id} seq 空洞：丢 {lost} 条（status={:?}）", out.status);
                        }
                        if out.kill {
                            eprintln!("[client] {adapter_id} 超出预算，已终止且不再拉起");
                            sup.kill(&adapter_id, "超出预算");
                        }
                    }
                    Err(e) => {
                        let _ = store.append_rejected(&adapter_id, &value, &e.to_string());
                        eprintln!("[client] {adapter_id} 一条事件解析失败，已原样保留");
                    }
                }
            }
        }
        Inbound::Eof { adapter_id } => {
            // 只报不杀：真正判活死交给 reap()，避免把"自己退出"的适配器误判成崩溃。
            eprintln!("[client] {adapter_id} 关闭了 stdout");
        }
        Inbound::Err { adapter_id, msg } => {
            eprintln!("[client] {adapter_id} 读取错误：{}", shorten(&msg, 200));
        }
        Inbound::Console { line } => {
            let line = line.trim().to_string();
            if line.is_empty() {
                return Ok(());
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
                    start_lesson(store, sup, info);
                }
                "stop" => stop_lesson(store, sup, rx, quit, "console", nested),
                "status" => print_status(store, sup),
                "quit" | "exit" => *quit = true,
                other => eprintln!("[client] 未知指令 {other}（可用：start/stop/status/quit）"),
            }
        }
    }
    Ok(())
}

/// 收课。顺序是有意安排的：先通知适配器，再把它们收尾时补发的那几条读进来，最后才关课。
///
/// 反过来做就会丢数据：课一关，落盘目标从 lessons/<id>/events.ndjson 切到 misc.ndjson，
/// 适配器那句"这节课到底采到了什么"的 session.close 就记到课外面去了。
fn stop_lesson(
    store: &mut Store,
    sup: &mut Supervisor,
    rx: &Receiver<Inbound>,
    quit: &mut bool,
    reason: &str,
    nested: bool,
) {
    let id = match store.active_lesson() {
        Some(m) => m.info.lesson_id.clone(),
        None => return,
    };
    for h in sup.ids() {
        let _ = sup.send(&h, Command::StopLesson { lesson_id: id.clone(), reason: reason.to_string() });
    }
    if !nested {
        drain_tail(store, sup, rx, quit);
    }
    let stats = store.stats();
    let (ev, bytes): (u64, u64) = stats.iter().fold((0, 0), |a, s| (a.0 + s.1.events, a.1 + s.1.bytes));
    eprintln!("[client] 收课 {id}：{ev} 条事件，{bytes} 字节；导出用 --lesson {id}");
    if let Err(e) = store.end_lesson(reason) {
        eprintln!("[client] 收尾写盘失败：{e}");
    }
}

/// 等适配器把收尾的尾巴送进来：攒在 VAD 里的最后一句，以及那条汇总用的 session.close。
///
/// 判据是"静默"而不是"进程退出"：适配器是发完才退的，等它就等于把一次正常的关机变成空转。
/// 400ms 静默足够跨一次磁盘写；3 秒是硬顶——再慢也不该让教师等关机等到怀疑人生。
fn drain_tail(store: &mut Store, sup: &mut Supervisor, rx: &Receiver<Inbound>, quit: &mut bool) {
    let hard = Instant::now() + Duration::from_millis(3_000);
    loop {
        let left = hard.saturating_duration_since(Instant::now()).min(Duration::from_millis(400));
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok(ib) => {
                if let Err(e) = handle_inbound(ib, store, sup, rx, quit, true) {
                    eprintln!("[client] 收尾时落盘失败：{e}");
                }
            }
            // 一个静默窗口没东西 = 尾巴到齐；断开 = 再也没人会发了。
            Err(_) => break,
        }
    }
}

fn start_lesson(store: &mut Store, sup: &mut Supervisor, info: LessonInfo) {
    let id = info.lesson_id.clone();
    match store.begin_lesson(info) {
        Ok(()) => {
            eprintln!("[client] 开课 {id}，blob 目录 {}", store.active_lesson().map(|m| m.info.blob_dir.clone()).unwrap_or_default());
            let li = store.active_lesson().map(|m| m.info.clone());
            let mut sent = 0usize;
            for h in sup.ids() {
                if let Some(li) = &li {
                    if sup.send(&h, Command::StartLesson { lesson: li.clone() }).is_ok() {
                        sent += 1;
                    }
                }
            }
            eprintln!("[client] StartLesson 已送达 {sent} 个适配器");
        }
        Err(e) => eprintln!("[client] 开课失败：{e}"),
    }
}

fn print_status(store: &Store, sup: &Supervisor) {
    let lesson = store.active_lesson().map(|m| m.info.lesson_id.clone()).unwrap_or_else(|| "—".into());
    println!(
        "[client] {} 课={lesson} proto=v{PROTO}",
        hhmmss(classagent_schema::utc_ms())
    );
    let mut total = 0u64;
    for (id, st) in store.stats() {
        total += st.events;
        // "core" 是核心自己的 seq 空间（admit / respawn 标记），不是子进程，没有句柄。
        let status = match id.as_str() {
            "core" => "自身".to_string(),
            _ => sup.find(&id).map(|h| format!("{:?}", h.status)).unwrap_or_else(|| "未装载".into()),
        };
        let note = sup.find(&id).and_then(|h| h.note.clone()).unwrap_or_default();
        println!(
            "  {id:<16} {status:<9} ev={:<8} {:>7} 丢失={} 超预算={} {}",
            st.events,
            human_bytes(st.bytes),
            st.lost_events,
            st.exceeded,
            note
        );
    }
    for h in sup.status_table() {
        if !store.known_source(&h.0) {
            println!("  {:<16} {:?} {}", h.0, h.1, h.2.unwrap_or_default());
        }
    }
    // 开着课却一条都没收到：这是现场最贵的一种失败，不能等下课再发现。
    if let Some(m) = store.active_lesson() {
        let age_s = classagent_schema::mono_us().saturating_sub(m.started_core_mono_us) / 1_000_000;
        if age_s >= 5 && total == 0 {
            println!("  ！开课 {age_s} 秒仍收到 0 条事件：检查 --adapters 目录是否存在、里面的程序能不能起来");
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
        "[client] 导出 {}：track={} ink={} 语句={} 书写中讲话={}ms 坏行={}",
        out_path.display(),
        payload.track.len(),
        payload.stats.strokes,
        payload.stats.utterances,
        payload.stats.writing_while_speaking_ms,
        bad
    );
    for w in &payload.warnings {
        println!("[client] 警告：{w}");
    }
    for (s, h) in &payload.sources {
        if h.silent {
            println!("[client] 源 {s} 声明了 {:?} 却全程无事件", h.declared);
        }
    }
    Ok(())
}

/// 一节课的可读摘要。看板上显示的就是这段文本，所以它先于 UI 被 CI 断言。
fn digest_cmd(opts: Opts, data: PathBuf) -> std::io::Result<()> {
    let id = match opts.val("lesson") {
        Some(v) => v.to_string(),
        None => {
            eprintln!("digest 需要 --lesson ID");
            std::process::exit(2);
        }
    };
    let meta = match store::read_meta(&data, &id)? {
        Some(m) => m,
        None => {
            eprintln!("{id} 没有 meta.json");
            std::process::exit(2);
        }
    };
    let (records, _bad) = store::read_records(&data, &id)?;
    let payload = timeline::build(&meta, &records);
    let text = digest::render(&payload);
    match opts.val("out") {
        Some(p) => {
            std::fs::write(p, text.as_bytes())?;
            println!("[client] 摘要已写入 {p}");
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// 本地看板服务。默认只绑 127.0.0.1，且只读。
fn serve_cmd(opts: Opts, data: PathBuf) -> std::io::Result<()> {
    let host = opts.val("host").unwrap_or("127.0.0.1");
    let port = opts.num("port").unwrap_or(8786);
    let adapters = PathBuf::from(opts.val("adapters").unwrap_or("adapters.d"));
    let allow_write = opts.flag("allow-write");
    std::fs::create_dir_all(&data)?;
    serve::run(serve::Config { data, adapters, listen: format!("{host}:{port}"), allow_write })
}

/// 客户端 → 服务端：读一节课，折叠成 ai_payload，HTTP POST 推给 classagent-server。
/// 只推折叠后的载荷，不推原始 events/blobs——原始证据留在采集端本机。
fn push_cmd(opts: Opts, data: PathBuf) -> std::io::Result<()> {
    let id = match opts.val("lesson") {
        Some(v) => v.to_string(),
        None => {
            eprintln!("push 需要 --lesson ID");
            std::process::exit(2);
        }
    };
    let server = match opts.val("server") {
        Some(v) => v.to_string(),
        None => {
            eprintln!("push 需要 --server HOST:PORT");
            std::process::exit(2);
        }
    };
    let token = opts.val("token").map(|s| s.to_string());
    let path = opts.val("path").unwrap_or("/api/ingest").to_string();

    // 优先推已导出的 ai_payload.json（字节稳定，重投才谈得上幂等）；
    // 没有就先在内存里折叠一份（不落盘，不污染 export 的产物）。
    let payload_path = data.join("lessons").join(&id).join("ai_payload.json");
    let ai_payload: serde_json::Value = if payload_path.exists() {
        serde_json::from_slice(&std::fs::read(&payload_path)?)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    } else {
        let meta = match store::read_meta(&data, &id)? {
            Some(m) => m,
            None => {
                eprintln!("{id} 既无 ai_payload.json 也无 meta.json，无法推送");
                std::process::exit(2);
            }
        };
        let (records, _bad) = store::read_records(&data, &id)?;
        let payload = timeline::build(&meta, &records);
        serde_json::to_value(&payload).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
    };

    let upload = LessonUpload {
        proto: PROTO,
        lesson_id: id.clone(),
        uploaded_at_utc_ms: classagent_schema::utc_ms(),
        source: format!("classagent-client {}", env!("CARGO_PKG_VERSION")),
        ai_payload,
    };
    let body = serde_json::to_vec(&upload).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let (code, resp) = push::post_json(&server, &path, &body, token.as_deref())?;
    println!("[push] {id} → {server}{path}  {} 字节  HTTP {code}", body.len());
    println!("{resp}");
    if !(200..=299).contains(&code) {
        return Err(std::io::Error::other(format!("服务端返回 HTTP {code}")));
    }
    Ok(())
}
