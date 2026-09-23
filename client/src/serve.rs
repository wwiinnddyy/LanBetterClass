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

/// 一次能写进 params 的上限。没有这条，一个手滑贴进来的巨型对象会让声明文件
/// 变成一个几十 MB 的 JSON——而它是每次开课都要重读一遍的东西。
const MAX_PARAMS_BYTES: usize = 8 * 1024;

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
                Ok(_) => patch_adapter(cfg, &path, &body),
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
                "server": "classagent-client serve",
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

/// `POST /api/adapter/<file>` body `{"enabled": bool}` 和/或 `{"params": {...}}`
///
/// 只许改这两项。`argv` / `cwd` / `id` / `platforms` 一律拒——`--allow-write` 的前提是
/// "本机教师可信"，而能改 argv 等于把声明文件换成"启动时替我执行任意程序"，
/// 那不再是配置接口，是本机命令执行入口。要换可执行文件必须人工改声明并重启客户端。
fn patch_adapter(cfg: &Config, path: &str, body: &[u8]) -> Reply {
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
    let merged = match merge_decl(file, &want) {
        Ok(m) => m,
        Err(e) => return err(400, &e),
    };
    let p = cfg.adapters.join(file);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => return err(404, "没有这个声明文件"),
    };
    let (out, enabled, params_changed) = match merge_decl_into(&text, &merged) {
        Ok(v) => v,
        Err(e) => return err(400, &e),
    };

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
    let mut resp = json!({ "file": file });
    if let Some(b) = enabled {
        resp["enabled"] = json!(b);
    }
    resp["note"] = json!(if params_changed {
        // 正在采集的这一节用的还是开课时下发的参数——"立刻生效"需要重启该源，
        // 而重启会把正在说的那一句切成两段。这句话必须如实，不许承诺做不到的事。
        "已写入：下一节课生效；本节课继续用开课时的参数"
    } else {
        "已写入：该源下次被启动时生效"
    });
    (200, JSON, to_vec(&resp))
}

/// 校验请求体，整理出"允许被合并进去的东西"。纯函数，所以四条边界能被单测钉住。
fn merge_decl(file: &str, want: &Value) -> Result<Value, String> {
    let obj = want
        .as_object()
        .ok_or_else(|| format!("{file}: 请求体必须是 JSON 对象，形如 {{\"enabled\": true}} 或 {{\"params\": {{…}}}}"))?;
    for k in obj.keys() {
        if k != "enabled" && k != "params" {
            return Err(format!(
                "{file}: 只能改 enabled/params，收到 {k}；换可执行文件请人工改声明并重启客户端"
            ));
        }
    }
    if obj.is_empty() {
        return Err(format!("{file}: 请求体是空对象，没有任何要改的字段"));
    }
    let mut out = serde_json::Map::new();
    // enabled 若出现就必须是严格 bool："1"/"yes" 静默当成 true，会把"我关掉了"这个
    // 判断建立在一个从没被理解的输入上。
    if let Some(v) = obj.get("enabled") {
        match v.as_bool() {
            Some(b) => {
                out.insert("enabled".into(), json!(b));
            }
            None => return Err(format!("{file}: enabled 必须是 true 或 false")),
        }
    }
    if let Some(v) = obj.get("params") {
        let p = v.as_object().ok_or_else(|| format!("{file}: params 必须是个对象"))?;
        if let Some(vad) = p.get("vad") {
            vad_is_sane(vad)?;
        }
        let size = serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX);
        if size > MAX_PARAMS_BYTES {
            return Err(format!("{file}: params 序列化后 {size} 字节，超过 {MAX_PARAMS_BYTES} 上限"));
        }
        out.insert("params".into(), v.clone());
    }
    Ok(Value::Object(out))
}

/// 迟滞带的上下沿只拦一类写错：`rms_close >= rms_open` 会被适配器**静默**钳成
/// `rms_open * 0.6`（见 a-audio/src/vad.rs），教师写完看不出自己填的被改过——
/// 所以必须在写盘前拦住。负值同理（会被 `filter(|x| *x > 0.0)` 静默忽略）。
/// 其余越界值交给适配器自己钳，服务端不抄第二份会漂移的校验。
fn vad_is_sane(vad: &Value) -> Result<(), String> {
    let o = vad.as_object().ok_or_else(|| "params.vad 必须是个对象".to_string())?;
    for k in ["rms_open", "rms_close"] {
        if let Some(v) = o.get(k) {
            if v.as_f64().map(|x| x.is_finite() && x < 0.0).unwrap_or(false) {
                return Err(format!("vad.{k} 不能是负数：{v}"));
            }
        }
    }
    if let (Some(o), Some(c)) = (
        o.get("rms_open").and_then(|v| v.as_f64()),
        o.get("rms_close").and_then(|v| v.as_f64()),
    ) {
        if c >= o {
            return Err(format!(
                "vad.rms_close（{c}）必须低于 vad.rms_open（{o}）：关段门限不低于开段门限时，语音段永远关不掉"
            ));
        }
    }
    Ok(())
}

/// 把已校验的补丁合并进声明文本。返回（新文本，enabled，是否改了 params）。
fn merge_decl_into(text: &str, merged: &Value) -> Result<(String, Option<bool>, bool), String> {
    let mut v: Value = serde_json::from_str(text).map_err(|e| format!("声明文件本身不是合法 JSON：{e}"))?;
    if !v.is_object() {
        return Err("声明文件必须是 JSON 对象".to_string());
    }
    let enabled = merged.get("enabled").and_then(|x| x.as_bool());
    if let Some(b) = enabled {
        v["enabled"] = json!(b);
    }
    let params_changed = merged.get("params").is_some();
    if params_changed {
        // merged 里只可能有 enabled 与 params 两个键（上面 merge_decl 已经把其它的拒掉了），
        // 所以这里可以直接整体深合并，不必再分一次字典。
        deep_merge(&mut v, merged);
    }
    let out = serde_json::to_string_pretty(&v).map_err(|e| format!("序列化失败：{e}"))?;
    Ok((out, enabled, params_changed))
}

/// 对象递归深合并，标量直接替换。
///
/// 必须是深合并：调参界面只想提交 `params.vad.rms_open` 一个数，浅赋值会把同级的
/// `source`/`fixture` 与 `tuning` 这类指引位一并抹掉——那是别人的配置。
fn deep_merge(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                match d.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => deep_merge(existing, v),
                    _ => {
                        d.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => *dst = src.clone(),
    }
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

    fn decl() -> Value {
        json!({
            "id": "a-audio",
            "enabled": true,
            "argv": ["$TARGET_DIR/a-audio"],
            "params": {
                "source": "fixture", "fixture": "x/一节课.wav", "speed": 20,
                "vad": { "rms_open": 500, "rms_close": 300, "min_speech_ms": 250 },
                "tuning": "现场调门限的指引，不属于本次提交"
            }
        })
    }

    /// 只改一个门限不能顺手删掉别人的配置。深合并是这个接口唯一的护栏。
    #[test]
    fn params_merge_is_deep_and_surgical() {
        let text = serde_json::to_string(&decl()).unwrap();
        let patch = merge_decl(
            "a-audio.adapter.json",
            &json!({ "params": { "vad": { "rms_open": 45.0 } } }),
        )
        .expect("只提交一个 vad 字段必须被接受");
        let (out, enabled, changed) = merge_decl_into(&text, &patch).unwrap();
        assert!(changed);
        assert!(enabled.is_none(), "没提交 enabled 就不该顺手写它");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["params"]["vad"]["rms_open"], json!(45.0));
        assert_eq!(v["params"]["vad"]["rms_close"], json!(300), "同级的其它门限必须原样保留");
        assert_eq!(v["params"]["source"], json!("fixture"));
        assert_eq!(v["params"]["fixture"], json!("x/一节课.wav"));
        assert_eq!(v["params"]["tuning"], decl()["params"]["tuning"], "调参指引位不能被抹掉");
        assert_eq!(v["argv"], decl()["argv"], "argv 无论如何不该被动");
        assert_eq!(v["enabled"], json!(true));
    }

    /// 这是安全闸门：能改 argv 的写接口等于本机任意命令执行。
    #[test]
    fn argv_and_unknown_keys_are_refused() {
        for body in [
            json!({ "argv": ["calc.exe"] }),
            json!({ "params": {}, "cwd": "C:\\Windows" }),
            json!({ "id": "other" }),
            json!({ "platforms": ["windows", "linux"] }),
            json!({ "enabled": true, "argv": [] }),
        ] {
            let err = merge_decl("a.adapter.json", &body).expect_err(&format!("{body:?} 必须被拒"));
            assert!(err.contains("只能改 enabled/params"), "报错要说清能改什么：{err}");
        }
    }

    #[test]
    fn types_are_checked_before_writing() {
        // 保住看板既有的严格 bool 语义："yes" 不能被当成 true 写盘。
        assert!(merge_decl("a.adapter.json", &json!({ "enabled": "yes" })).is_err());
        assert!(merge_decl("a.adapter.json", &json!({ "params": "x" })).is_err());
        assert!(merge_decl("a.adapter.json", &json!({ "params": { "vad": 3 } })).is_err());
        assert!(merge_decl("a.adapter.json", &json!({})).is_err(), "空对象不该被当成一次成功的写入");
        assert!(merge_decl("a.adapter.json", &json!("x")).is_err());
        assert!(merge_decl("a.adapter.json", &json!({ "enabled": false })).is_ok());
    }

    /// 写反了的迟滞带会被适配器静默钳成 rms_open*0.6，教师看不出被改过——
    /// 所以这一类必须在写盘前就拒掉。
    #[test]
    fn reversed_hysteresis_is_refused_but_partial_updates_are_not() {
        assert!(vad_is_sane(&json!({ "rms_open": 400.0, "rms_close": 900.0 })).is_err());
        assert!(vad_is_sane(&json!({ "rms_open": 400.0, "rms_close": 400.0 })).is_err());
        assert!(vad_is_sane(&json!({ "rms_open": -1.0 })).is_err());
        assert!(vad_is_sane(&json!({ "rms_close": -5 })).is_err());
        // 只改一个时不知道另一个，不能拦 —— 磁盘上还有现值，合完再由适配器钳。
        assert!(vad_is_sane(&json!({ "rms_open": 45.0 })).is_ok());
        assert!(vad_is_sane(&json!({ "min_speech_ms": 0 })).is_ok());
        // 超限值交给适配器，服务端不抄第二份会漂移的校验（frame_ms < 5 会被丢弃用默认）。
        assert!(vad_is_sane(&json!({ "frame_ms": 1, "max_segment_ms": 10 })).is_ok());
    }

    #[test]
    fn oversized_params_is_refused() {
        let big = json!({ "params": { "note": "x".repeat(MAX_PARAMS_BYTES + 10) } });
        let err = merge_decl("a.adapter.json", &big).expect_err("巨型 params 必须被拒");
        assert!(err.contains("上限"), "{err}");
    }

    #[test]
    fn a_broken_declaration_is_not_overwritten() {
        let err = merge_decl_into("{ not json", &json!({ "enabled": false })).unwrap_err();
        assert!(err.contains("不是合法 JSON"), "{err}");
        assert!(merge_decl_into("[1,2]", &json!({ "enabled": false })).unwrap_err().contains("对象"));
    }
}
