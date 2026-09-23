//! 远程服务端：接收采集客户端（classagent-core）推来的 `ai_payload`，落盘去重，
//! 并生成一份确定性的"交给 AI"请求单。与本地看板（core 的 serve）同源：
//! tiny_http，默认只绑 127.0.0.1；生产用反向代理加 TLS 并配 `--token`。
//!
//! 边界：服务端到此为止。真正的 ASR / LLM 调用是这一步之外的下游——这里不接模型、
//! 不出网，只把 `ai_request.json`（四次投影的计划 + 统计）落盘，供下游消费与 CI 断言。

use classagent_schema::{safe_component, IngestAck, LessonUpload, PROTO};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use tiny_http::{Header, Method, Request, Response, Server};

const JSON: &str = "application/json; charset=utf-8";

type Reply = (u16, &'static str, Vec<u8>);

struct Config {
    listen: String,
    data: PathBuf,
    token: Option<String>,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut listen = "127.0.0.1:8790".to_string();
    let mut data = PathBuf::from("server-data");
    let mut token: Option<String> = None;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--listen" => {
                if let Some(v) = argv.get(i + 1) {
                    listen = v.clone();
                }
                i += 2;
            }
            "--data" => {
                if let Some(v) = argv.get(i + 1) {
                    data = PathBuf::from(v);
                }
                i += 2;
            }
            "--token" => {
                if let Some(v) = argv.get(i + 1) {
                    token = Some(v.clone());
                }
                i += 2;
            }
            "--help" | "-h" => {
                print_usage();
                return;
            }
            other => {
                eprintln!("[server] 忽略未知参数 {other}");
                i += 1;
            }
        }
    }

    if let Err(e) = run(Config { listen, data, token }) {
        eprintln!("[server] 错误：{e}");
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!("用法：classagent-server [--listen HOST:PORT] [--data DIR] [--token SECRET]");
    eprintln!("  接收采集端 POST /api/ingest 的 LessonUpload，落盘 inbox/<id>.ai_payload.json，");
    eprintln!("  生成 ai/<id>.ai_request.json（四次投影计划）。只读接口：/health、/api/lessons、");
    eprintln!("  /api/lesson/<id>/payload、/api/lesson/<id>/ai-request。默认只绑 127.0.0.1。");
}

fn run(cfg: Config) -> std::io::Result<()> {
    fs::create_dir_all(cfg.data.join("inbox"))?;
    fs::create_dir_all(cfg.data.join("ai"))?;
    let server = Server::http(cfg.listen.as_str())
        .map_err(|e| std::io::Error::other(format!("监听 {} 失败：{e}", cfg.listen)))?;
    println!("[server] 监听   http://{}/", cfg.listen);
    println!("[server] 数据   {}", cfg.data.display());
    println!(
        "[server] 鉴权   {}",
        if cfg.token.is_some() { "token（X-ClassAgent-Token）" } else { "关闭：本地免鉴权" }
    );
    if !(cfg.listen.starts_with("127.") || cfg.listen.starts_with("localhost")) {
        println!("[server] ！监听地址对局域网可见，生产请加 TLS 反向代理并配 --token");
    }

    loop {
        match server.recv() {
            Ok(req) => handle(&cfg, req),
            Err(e) => {
                eprintln!("[server] 接收失败，退出：{e}");
                break;
            }
        }
    }
    Ok(())
}

fn handle(cfg: &Config, mut req: Request) {
    let url = req.url().to_string();
    let path = match url.split_once('?') {
        Some((p, _)) => p.to_string(),
        None => url,
    };
    let is_get = matches!(req.method(), &Method::Get);
    let is_post = matches!(req.method(), &Method::Post);

    let reply = if is_get {
        route(cfg, &path)
    } else if is_post {
        if path == "/api/ingest" {
            if !authorized(cfg, &req) {
                err(401, "缺少或错误的 X-ClassAgent-Token")
            } else {
                let mut body = Vec::new();
                match req.as_reader().read_to_end(&mut body) {
                    Ok(_) => ingest(cfg, &body),
                    Err(e) => err(400, &format!("读不到请求体：{e}")),
                }
            }
        } else {
            err(404, "没有这个写接口")
        }
    } else {
        err(405, "只支持 GET，以及 POST /api/ingest")
    };

    let (code, ctype, body) = reply;
    let resp = Response::from_data(body)
        .with_status_code(code)
        .with_header(header("Content-Type", ctype))
        .with_header(header("Cache-Control", "no-store"));
    let _ = req.respond(resp);
}

fn route(cfg: &Config, path: &str) -> Reply {
    if path == "/" || path == "/health" {
        return (
            200,
            JSON,
            to_vec(&json!({
                "server": "classagent-server",
                "proto": PROTO,
                "listen": cfg.listen,
                "data_dir": cfg.data.to_string_lossy(),
                "auth": cfg.token.is_some(),
                "received": count_lessons(cfg),
            })),
        );
    }
    if path == "/api/lessons" {
        return lessons(cfg);
    }
    if let Some(rest) = path.strip_prefix("/api/lesson/") {
        return lesson_route(cfg, rest);
    }
    err(404, "没有这个接口")
}

/// `/api/lesson/<id>[/payload|/ai-request]`
fn lesson_route(cfg: &Config, rest: &str) -> Reply {
    let mut seg = rest.splitn(2, '/');
    let id = seg.next().unwrap_or("");
    if !safe_component(id) {
        return err(400, "课程 id 不合法：只允许字母、数字、- _ .");
    }
    match seg.next() {
        None | Some("payload") => {
            let p = cfg.data.join("inbox").join(format!("{id}.ai_payload.json"));
            read_file(&p).map(|b| (200, JSON, b)).unwrap_or_else(|| err(404, "没有这节课"))
        }
        Some("ai-request") => {
            let p = cfg.data.join("ai").join(format!("{id}.ai_request.json"));
            read_file(&p).map(|b| (200, JSON, b)).unwrap_or_else(|| err(404, "没有 AI 请求单"))
        }
        Some(_) => err(404, "没有这个接口"),
    }
}

fn lessons(cfg: &Config) -> Reply {
    let mut out = Vec::new();
    let dir = cfg.data.join("inbox");
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".ai_payload.json") {
                out.push(json!({
                    "lesson_id": id,
                    "bytes": e.metadata().map(|m| m.len()).unwrap_or(0),
                }));
            }
        }
    }
    out.sort_by(|a, b| a["lesson_id"].as_str().unwrap_or("").cmp(b["lesson_id"].as_str().unwrap_or("")));
    (200, JSON, to_vec(&out))
}

/// 收一条 LessonUpload：校验 → 落盘（逐字节去重）→ 生成 AI 请求单。
fn ingest(cfg: &Config, body: &[u8]) -> Reply {
    let up: LessonUpload = match serde_json::from_slice(body) {
        Ok(u) => u,
        Err(e) => return err(400, &format!("上传体不是合法 LessonUpload：{e}")),
    };
    if up.proto != PROTO {
        return err(400, &format!("协议版本不符：客户端报 v{}，服务端 v{PROTO}", up.proto));
    }
    if !safe_component(&up.lesson_id) {
        return err(400, "课程 id 不合法");
    }

    let payload_bytes = serde_json::to_vec_pretty(&up.ai_payload).unwrap_or_else(|_| body.to_vec());
    let path = cfg.data.join("inbox").join(format!("{}.ai_payload.json", up.lesson_id));
    let (stored, deduped) = match fs::read(&path) {
        Ok(old) => (old != payload_bytes, old == payload_bytes),
        Err(_) => (true, false),
    };
    if stored {
        if let Err(e) = write_atomic(&path, &payload_bytes) {
            return err(500, &format!("写入失败：{e}"));
        }
    }

    // AI 请求单：确定性，不含任何模型调用。四次投影各一次，规则活与模型活不混。
    let stats = up.ai_payload.get("stats").cloned().unwrap_or(Value::Null);
    let sources: Vec<String> = up
        .ai_payload
        .get("sources")
        .and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let prev = up.ai_payload.get("prev_lesson_id").cloned().unwrap_or(Value::Null);
    let ai_req = json!({
        "proto": PROTO,
        "lesson_id": up.lesson_id,
        "received_at_utc_ms": classagent_schema::utc_ms(),
        "source": up.source,
        "payload_ref": format!("inbox/{}.ai_payload.json", up.lesson_id),
        "prev_lesson_id": prev,
        "stats": stats,
        "sources": sources,
        "projections": [
            { "name": "observe_log",       "mode": "rule_then_model", "note": "3 秒采样先机械编码（规则活），再让模型判类；不与语义压缩混进同一 prompt" },
            { "name": "lesson_notes",      "mode": "model",           "note": "语义压缩（模型活）" },
            { "name": "activity_analysis", "mode": "model",           "note": "活动结构分析" },
            { "name": "next_steps",        "mode": "model",           "note": "后续建议；衔接 prev_lesson_id 显式链，非向量检索" }
        ],
        "handoff": "服务端到此为止：真正的 ASR/LLM 调用是这一步之外的下游"
    });
    let req_path = cfg.data.join("ai").join(format!("{}.ai_request.json", up.lesson_id));
    if let Err(e) = write_atomic(&req_path, &serde_json::to_vec_pretty(&ai_req).unwrap_or_default()) {
        return err(500, &format!("写 AI 请求单失败：{e}"));
    }

    let ack = IngestAck {
        ok: true,
        proto: PROTO,
        lesson_id: up.lesson_id.clone(),
        stored,
        deduped,
        received_at_utc_ms: classagent_schema::utc_ms(),
        ai_request: req_path.to_string_lossy().into_owned(),
    };
    (200, JSON, to_vec(&ack))
}

fn authorized(cfg: &Config, req: &Request) -> bool {
    match &cfg.token {
        None => true,
        Some(t) => req
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("x-classagent-token"))
            .map(|h| h.value.as_str() == t.as_str())
            .unwrap_or(false),
    }
}

fn count_lessons(cfg: &Config) -> usize {
    fs::read_dir(cfg.data.join("inbox"))
        .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".ai_payload.json")).count())
        .unwrap_or(0)
}

fn read_file(p: &Path) -> Option<Vec<u8>> {
    fs::read(p).ok()
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)
}

fn header(name: &str, value: &str) -> Header {
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]);
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap_or_else(|_| ct.unwrap())
}

fn err(code: u16, msg: &str) -> Reply {
    (code, JSON, json!({ "error": msg }).to_string().into_bytes())
}

fn to_vec<T: serde::Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec_pretty(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use classagent_schema::safe_component;

    /// 服务端唯一挡住目录穿越的东西，必须能变红。
    #[test]
    fn path_traversal_is_rejected() {
        for bad in ["..", "../../etc/passwd", "a/b", "a\\b", "/etc/passwd", "a\0b", "", ".", "%2e%2e"] {
            assert!(!safe_component(bad), "不该放过 {bad:?}");
        }
        for ok in ["L-demo-0001", "lesson_1.json", "a-b_c"] {
            assert!(safe_component(ok), "不该误伤 {ok:?}");
        }
    }
}
