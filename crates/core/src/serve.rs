//! 本地看板服务：只读为主，写操作要显式开 `--allow-write`。
//!
//! 两条不能让步的约束：
//! 1. **默认只绑 127.0.0.1**。要让别的设备来看必须显式给 `--host`，因为那同时打开
//!    了"局域网里任何设备都能读这所学校的课堂数据"这件事。
//! 2. **一切文件访问被关在 `data/lessons/<id>/blobs/` 里**。id 与文件名都要过
//!    `safe_component`，否则一个带 `..` 的 GET 就能读一体机上任意文件——这不是
//!    理论风险，而是这类"本地小工具"最常见的漏法。

use crate::{digest, store, timeline};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use tiny_http::{Header, Method, Request, Response, Server};

const HTML: &str = "text/html; charset=utf-8";
const JSON: &str = "application/json; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";

/// 前端是零构建的单文件，直接编进二进制：一体机上不用装 node，也不用带 dist 目录。
const PAGE: &str = include_str!("../web/index.html");

pub struct Config {
    pub data: PathBuf,
    pub adapters: PathBuf,
    pub listen: String,
    pub allow_write: bool,
}

type Reply = (u16, &'static str, Vec<u8>);

pub fn run(cfg: Config) -> std::io::Result<()> {
    let server = Server::http(cfg.listen.as_str())
        .map_err(|e| std::io::Error::other(format!("监听 {} 失败：{e}", cfg.listen)))?;
    println!("[serve] 看板   http://{}/", cfg.listen);
    println!("[serve] 数据   {}", cfg.data.display());
    println!(
        "[serve] 写操作 {}",
        if cfg.allow_write {
            "已开启（--allow-write）"
        } else {
            "关闭：当前只读，要改源开关请加 --allow-write"
        }
    );
    if !(cfg.listen.starts_with("127.") || cfg.listen.starts_with("localhost")) {
        println!("[serve] ！监听地址对局域网可见，同网络的任何设备都能读这些课堂数据");
    }

    loop {
        match server.recv() {
            Ok(req) => handle(&cfg, req),
            Err(e) => {
                eprintln!("[serve] 接收失败，退出：{e}");
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
        if !cfg.allow_write {
            err(403, "写操作未开启：启动时加 --allow-write")
        } else if path.starts_with("/api/adapter/") {
            let mut body = Vec::new();
            match req.as_reader().read_to_end(&mut body) {
                Ok(_) => toggle_adapter(cfg, &path, &body),
                Err(e) => err(400, &format!("读不到请求体：{e}")),
            }
        } else {
            err(404, "没有这个写接口")
        }
    } else {
        err(405, "只支持 GET，以及开启 --allow-write 后的 POST")
    };

    let (code, ctype, body) = reply;
    let resp = Response::from_data(body)
        .with_status_code(code)
        .with_header(header("Content-Type", ctype))
        // 看板是轮询的，缓存会让"这节课还在采"看起来像卡死。
        .with_header(header("Cache-Control", "no-store"));
    let _ = req.respond(resp);
}

/// 前端优先读磁盘上的 `web/index.html`，读不到才用编进二进制的那份。
/// 这样调样式不用重编译——但发布包里丢了文件也不会白屏。
fn page() -> Vec<u8> {
    match std::fs::read("web/index.html") {
        Ok(b) if b.len() > 500 => b,
        _ => PAGE.as_bytes().to_vec(),
    }
}

fn route(cfg: &Config, path: &str) -> Reply {
    if path == "/" || path == "/index.html" {
        return (200, HTML, page());
    }
    if path == "/api/health" {
        return (
            200,
            JSON,
            to_vec(&json!({
                "server": "classagent-core serve",
                "proto": classagent_schema::PROTO,
                "listen": cfg.listen,
                "data_dir": cfg.data.to_string_lossy(),
                "adapters_dir": cfg.adapters.to_string_lossy(),
                "allow_write": cfg.allow_write,
                "lessons": store::lesson_ids(&cfg.data).len(),
            })),
        );
    }
    if path == "/api/lessons" {
        return lessons(cfg);
    }
    if path == "/api/adapters" {
        return adapters(cfg);
    }
    if path.starts_with("/api/lesson/") {
        return lesson_route(cfg, path);
    }
    err(404, "没有这个接口")
}

/// `/api/lesson/<id>[/(digest|stats|blob/<name>)]`
fn lesson_route(cfg: &Config, path: &str) -> Reply {
    let rest = match path.strip_prefix("/api/lesson/") {
        Some(r) => r,
        None => return err(404, "路径不对"),
    };
    let mut seg = rest.splitn(3, '/');
    let id = seg.next().unwrap_or("");
    if !safe_component(id) {
        return err(400, "课程 id 不合法：只允许字母、数字、- _ .");
    }
    let payload = match build_payload(cfg, id) {
        Some(p) => p,
        None => return err(404, "没有这节课，或者它缺 meta.json"),
    };
    match seg.next() {
        None => (200, JSON, to_vec(&payload)),
        Some("stats") => (
            200,
            JSON,
            to_vec(&json!({
                "lesson_id": id,
                "stats": payload.stats,
                "sources": payload.sources,
                "warnings": payload.warnings,
                "track_len": payload.track.len(),
            })),
        ),
        Some("digest") => (200, TEXT, digest::render(&payload).into_bytes()),
        Some("blob") => blob(cfg, id, seg.next().unwrap_or("")),
        Some(_) => err(404, "没有这个接口"),
    }
}

fn blob(cfg: &Config, id: &str, name: &str) -> Reply {
    if !safe_component(name) {
        return err(400, "blob 名不合法：不允许路径分隔符与 ..");
    }
    let path = cfg.data.join("lessons").join(id).join("blobs").join(name);
    match std::fs::read(&path) {
        Ok(bytes) => (200, mime_of(name), bytes),
        Err(_) => err(404, "没有这个 blob"),
    }
}

fn lessons(cfg: &Config) -> Reply {
    let mut out = Vec::new();
    for id in store::lesson_ids(&cfg.data) {
        let meta = match store::read_meta(&cfg.data, &id) {
            Ok(Some(m)) => m,
            _ => continue,
        };
        let events_bytes = std::fs::metadata(cfg.data.join("lessons").join(&id).join("events.ndjson"))
            .map(|m| m.len())
            .unwrap_or(0);
        let wall_ms = meta
            .ended_core_mono_us
            .map(|e| e.saturating_sub(meta.started_core_mono_us) / 1000)
            .unwrap_or(0);
        out.push(json!({
            "lesson_id": id,
            "subject": meta.info.subject,
            "class": meta.info.class,
            "teacher": meta.info.teacher,
            "prev_lesson_id": meta.info.prev_lesson_id,
            "started_at_utc_ms": meta.info.started_at_utc_ms,
            "stop_reason": meta.stop_reason,
            "wall_ms": wall_ms,
            "events_bytes": events_bytes,
            "courseware": meta.info.courseware,
        }));
    }
    (200, JSON, to_vec(&out))
}

fn adapters(cfg: &Config) -> Reply {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&cfg.adapters) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".adapter.json") {
                continue;
            }
            let text = match std::fs::read_to_string(e.path()) {
                Ok(t) => t,
                Err(e2) => {
                    out.push(json!({ "file": name, "error": format!("读不到：{e2}") }));
                    continue;
                }
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => out.push(json!({
                    "file": name,
                    "id": v.get("id").cloned().unwrap_or(Value::Null),
                    "enabled": v.get("enabled").cloned().unwrap_or(json!(true)),
                    "platforms": v.get("platforms").cloned().unwrap_or(json!([])),
                    "argv": v.get("argv").cloned().unwrap_or(json!([])),
                    "params": v.get("params").cloned().unwrap_or(json!({})),
                })),
                Err(e2) => out.push(json!({ "file": name, "error": format!("声明解析失败：{e2}") })),
            }
        }
    }
    out.sort_by(|a, b| a["file"].as_str().unwrap_or("").cmp(b["file"].as_str().unwrap_or("")));
    (200, JSON, to_vec(&out))
}

/// `POST /api/adapter/<file>` body `{"enabled": true}`
fn toggle_adapter(cfg: &Config, path: &str, body: &[u8]) -> Reply {
    let file = match path.strip_prefix("/api/adapter/") {
        Some(f) => f,
        None => return err(404, "路径不对"),
    };
    if !safe_component(file) || !file.ends_with(".adapter.json") {
        return err(400, "只能改 adapters.d 下的 *.adapter.json");
    }
    let want: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return err(400, &format!("请求体不是 JSON：{e}")),
    };
    let enabled = match want.get("enabled").and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => return err(400, "请求体需要 {\"enabled\": true|false}"),
    };
    let p = cfg.adapters.join(file);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => return err(404, "没有这个声明文件"),
    };
    let mut v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return err(400, &format!("{file} 本身不是合法 JSON：{e}")),
    };
    if !v.is_object() {
        return err(400, "声明文件必须是 JSON 对象");
    }
    v["enabled"] = json!(enabled);
    let out = serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.clone());

    // 原子替换：改到一半被中断会留下一个"采不起来又看不懂"的现场。
    let tmp = p.with_extension("json.tmp");
    let wrote = std::fs::write(&tmp, out.as_bytes()).and_then(|_| {
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
        std::fs::rename(&tmp, &p)
    });
    if let Err(e) = wrote {
        return err(500, &format!("写入失败：{e}"));
    }
    (
        200,
        JSON,
        to_vec(&json!({
            "file": file,
            "enabled": enabled,
            "note": "核心重启该源或下次启动时生效；正在采集的这一节不受影响"
        })),
    )
}

fn build_payload(cfg: &Config, id: &str) -> Option<timeline::AiPayload> {
    let meta = store::read_meta(&cfg.data, id).ok().flatten()?;
    let (records, _bad) = store::read_records(&cfg.data, id).ok()?;
    Some(timeline::build(&meta, &records))
}

/// 路径片段白名单。`..`、`/`、`\`、NUL、绝对路径、盘符全进不来。
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 128
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "opus" => "audio/opus",
        "mp3" => "audio/mpeg",
        "json" => JSON,
        "txt" | "vtt" | "md" => TEXT,
        _ => "application/octet-stream",
    }
}

fn header(name: &str, value: &str) -> Header {
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..]);
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
    use super::*;

    /// 这个白名单是唯一挡住目录穿越的东西，必须能变红。
    #[test]
    fn path_traversal_is_rejected() {
        for bad in [
            "..",
            "../../etc/passwd",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "C:\\Windows\\system32\\config\\sam",
            "a\0b",
            "",
            ".",
            "%2e%2e%2fsecret",
            "....//....//x",
        ] {
            assert!(!safe_component(bad), "不该放过 {bad:?}");
        }
        for ok in ["audio-000000000900ms.pcm", "L-demo-0001", "a-fake.adapter.json"] {
            assert!(safe_component(ok), "不该误伤 {ok:?}");
        }
    }
}
