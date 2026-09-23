//! 追加式落盘与投递状态。
//!
//! v0 有意不引入 SQLite：一节课堂数据本来就是追加写的，`events.ndjson` + 每适配器
//! 一个 `last_seq` 就能表达幂等与缺口检测，而 rusqlite 的 bundled 构建恰好是这台
//! 机器上最贵的那个依赖。真要换的时候，消费者读的仍是同一批 `StoredRecord`。

use crate::protocol::shorten;
use classagent_schema::{kinds, Budget, Envelope, Exceed, LessonInfo, LessonMeta, RecordStatus, StoredRecord};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Store {
    pub root: PathBuf,
    state: State,
    writer: Option<BufWriter<File>>,
    writer_for: Option<String>,
    meta: Option<LessonMeta>,
    rates: HashMap<String, Rate>,
    budgets: HashMap<String, Budget>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    next_global_seq: u64,
    core_seq: u64,
    last_seq: HashMap<String, u64>,
    active_lesson: Option<String>,
}

#[derive(Debug, Clone)]
struct Rate {
    win_start: Instant,
    win_events: u32,
    win_bytes: u64,
    total_events: u64,
    total_bytes: u64,
    exceeded: u64,
    lost: u64,
}

impl Default for Rate {
    fn default() -> Self {
        Rate {
            win_start: Instant::now(),
            win_events: 0,
            win_bytes: 0,
            total_events: 0,
            total_bytes: 0,
            exceeded: 0,
            lost: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub status: RecordStatus,
    pub gap: Option<u64>,
    /// 调用方应当立刻杀掉这个适配器，并且不再拉起。
    pub kill: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AdapterStat {
    pub events: u64,
    pub bytes: u64,
    pub exceeded: u64,
    /// 因 seq 跳号而确认丢失的条数（不是跳号次数——次数在导出的健康表里另有 `gaps`）。
    pub lost_events: u64,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Store> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        fs::create_dir_all(root.join("lessons"))?;
        let state: State = match fs::read(root.join("state.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => State::default(),
        };
        let meta = state
            .active_lesson
            .as_ref()
            .and_then(|id| read_meta(&root, id).ok().flatten());
        Ok(Store { root, state, writer: None, writer_for: None, meta, rates: HashMap::new(), budgets: HashMap::new() })
    }

    pub fn register_adapter(&mut self, id: &str, budget: Budget) {
        self.budgets.insert(id.to_string(), budget);
        self.rates.entry(id.to_string()).or_default();
    }

    pub fn lesson_dir(&self, lesson_id: &str) -> PathBuf {
        self.root.join("lessons").join(lesson_id)
    }

    pub fn active_lesson(&self) -> Option<&LessonMeta> {
        self.meta.as_ref()
    }

    pub fn active_lesson_id(&self) -> Option<&str> {
        self.meta.as_ref().map(|m| m.info.lesson_id.as_str())
    }

    pub fn begin_lesson(&mut self, mut info: LessonInfo) -> std::io::Result<()> {
        self.end_lesson("replaced")?;
        let dir = self.lesson_dir(&info.lesson_id);
        let blob_dir = dir.join("blobs");
        fs::create_dir_all(&blob_dir)?;
        info.blob_dir = blob_dir.to_string_lossy().into_owned();
        let meta = LessonMeta {
            info: info.clone(),
            started_core_mono_us: classagent_schema::mono_us(),
            ended_core_mono_us: None,
            stop_reason: None,
        };
        write_atomic(&dir.join("meta.json"), &serde_json_to_vec(&meta)?)?;
        self.meta = Some(meta);
        self.state.active_lesson = Some(info.lesson_id);
        self.save_state()?;
        Ok(())
    }

    pub fn end_lesson(&mut self, reason: &str) -> std::io::Result<()> {
        let Some(meta) = self.meta.as_mut() else { return Ok(()) };
        meta.ended_core_mono_us = Some(classagent_schema::mono_us());
        meta.stop_reason = Some(reason.to_string());
        // 直接走字段：这里 self.meta 已被可变借用，调用 &self 的方法会撞借用检查。
        let dir = self.root.join("lessons").join(&meta.info.lesson_id);
        write_atomic(&dir.join("meta.json"), &serde_json_to_vec(meta)?)?;
        if let Some(w) = self.writer.as_mut() {
            w.flush()?;
        }
        self.writer = None;
        self.writer_for = None;
        self.meta = None;
        self.state.active_lesson = None;
        self.save_state()
    }

    /// 收一条事件。永不 panic，也永不静默丢数据：超预算和 seq 空洞都要落盘并计数。
    pub fn append(&mut self, adapter_id: &str, env: &Envelope) -> std::io::Result<Outcome> {
        let rec = StoredRecord {
            global_seq: 0,
            adapter_id: adapter_id.to_string(),
            t_core_utc_ms: classagent_schema::utc_ms(),
            t_core_mono_us: classagent_schema::mono_us(),
            status: RecordStatus::Accepted,
            gap: None,
            envelope: env.clone(),
            raw: None,
        };
        self.write_rec(adapter_id, rec)
    }

    /// 解析失败的行：原样保留，标 Rejected。宁可让下游看见坏数据，也不要无声消失。
    pub fn append_rejected(&mut self, adapter_id: &str, value: &serde_json::Value, err: &str) -> std::io::Result<Outcome> {
        let env = Envelope {
            proto: 0,
            seq: 0,
            kind: "core.parse_error".into(),
            t_mono_us: 0,
            t_event_ms: None,
            t_utc_ms: None,
            payload: value.clone(),
        };
        let rec = StoredRecord {
            global_seq: 0,
            adapter_id: adapter_id.to_string(),
            t_core_utc_ms: classagent_schema::utc_ms(),
            t_core_mono_us: classagent_schema::mono_us(),
            status: RecordStatus::Rejected,
            gap: None,
            envelope: env,
            raw: Some(shorten(err, 400)),
        };
        self.write_rec(adapter_id, rec)
    }

    fn write_rec(&mut self, adapter_id: &str, mut rec: StoredRecord) -> std::io::Result<Outcome> {
        let last = self.state.last_seq.get(adapter_id).copied().unwrap_or(0);
        let mut gap = None;
        if rec.status == RecordStatus::Accepted {
            let seq = rec.envelope.seq;
            if seq <= last {
                rec.status = RecordStatus::Duplicate;
            } else {
                if seq > last + 1 {
                    gap = Some(seq - last - 1);
                    rec.status = RecordStatus::GapBefore;
                    rec.gap = gap;
                }
                self.state.last_seq.insert(adapter_id.to_string(), seq);
            }
        }

        // 以下每一步的借用都在下一步之前结束：`&mut self.rates` 不能和
        // `self.ensure_writer(&mut self)` 同时存活，所以预算窗口分两次访问。
        let budget = self.budgets.get(adapter_id).cloned();
        rec.global_seq = self.state.next_global_seq.max(1);
        self.state.next_global_seq = rec.global_seq + 1;

        // 预算按事件本身的大小计，不按落盘行大小计：多出来的信封字段不是适配器的责任。
        let est = serde_json::to_string(&rec.envelope).map(|s| s.len() as u64).unwrap_or(0);
        let win = self.rates.get(adapter_id).map(|r| (r.win_events, r.win_bytes, r.win_start.elapsed()));
        let (we, wb) = match win {
            Some((e, b, age)) if age < std::time::Duration::from_secs(1) => (e, b),
            _ => (0, 0),
        };
        let (over, kill) = match budget {
            Some(b) => (
                (we + 1) > b.max_events_per_s || (wb + est) > b.max_bytes_per_s,
                b.on_exceed == Exceed::Kill,
            ),
            None => (false, false),
        };
        let kill = over && kill;
        if over && rec.status == RecordStatus::Accepted {
            rec.status = RecordStatus::OverBudget;
        }

        let line = serde_json::to_string(&rec)?;
        self.ensure_writer()?;
        if let Some(w) = self.writer.as_mut() {
            w.write_all(line.as_bytes())?;
            w.write_all(b"\n")?;
        }

        let wrote = line.len() as u64 + 1;
        {
            let r = self.rates.entry(adapter_id.to_string()).or_default();
            if r.win_start.elapsed() >= std::time::Duration::from_secs(1) {
                r.win_start = Instant::now();
                r.win_events = 0;
                r.win_bytes = 0;
            }
            r.win_events += 1;
            r.win_bytes += wrote;
            r.total_events += 1;
            r.total_bytes += wrote;
            if over {
                r.exceeded += 1;
            }
            if let Some(g) = gap {
                r.lost += g;
            }
        }

        Ok(Outcome { status: rec.status, gap, kill })
    }

    fn target_key(&self) -> String {
        match self.meta.as_ref() {
            Some(m) => m.info.lesson_id.clone(),
            None => "misc".to_string(),
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        if key == "misc" {
            self.root.join("misc.ndjson")
        } else {
            self.lesson_dir(key).join("events.ndjson")
        }
    }

    fn ensure_writer(&mut self) -> std::io::Result<()> {
        let key = self.target_key();
        if self.writer.is_some() && self.writer_for.as_deref() == Some(key.as_str()) {
            return Ok(());
        }
        if let Some(w) = self.writer.as_mut() {
            w.flush()?;
        }
        let path = self.path_for(&key);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        self.writer = Some(BufWriter::new(f));
        self.writer_for = Some(key);
        Ok(())
    }

    /// 每节课大约 2-10 万条事件，250ms 落一次盘的代价远小于每条 fsync。
    /// 崩溃时最坏丢这 250ms 的事件——不重，因为白板侧还会再补一次 stroke_commit。
    pub fn tick(&mut self) -> std::io::Result<()> {
        if let Some(w) = self.writer.as_mut() {
            w.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.tick()?;
        self.save_state()
    }

    pub fn save_state(&self) -> std::io::Result<()> {
        write_atomic(&self.root.join("state.json"), &serde_json_to_vec(&self.state)?)
    }

    pub fn stats(&self) -> Vec<(String, AdapterStat)> {
        let mut out: Vec<(String, AdapterStat)> = self
            .rates
            .iter()
            .map(|(id, r)| {
                (
                    id.clone(),
                    AdapterStat { events: r.total_events, bytes: r.total_bytes, exceeded: r.exceeded, lost_events: r.lost },
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn last_seq(&self, adapter_id: &str) -> u64 {
        self.state.last_seq.get(adapter_id).copied().unwrap_or(0)
    }

    pub fn known_source(&self, id: &str) -> bool {
        self.rates.contains_key(id)
    }

    /// 核心自己也要往日志里写事实（谁装载了、谁重启了），否则这些只在进程退出
    /// 前存在于内存里，事后无法解释为什么某段数据是残缺的。
    pub fn note(&mut self, kind: &str, payload: serde_json::Value) -> std::io::Result<Outcome> {
        self.state.core_seq += 1;
        let seq = self.state.core_seq;
        let env = Envelope::new(seq, kind, None, payload);
        self.append("core", &env)
    }

    /// 适配器重启后它自己的 seq 会从 1 重新开始；不清零的话，重启之后的每一条
    /// 都会被 gap 判定当成 Duplicate 收下，等于这节课后半段作废。
    pub fn note_respawn(&mut self, adapter_id: &str) -> std::io::Result<()> {
        self.state.last_seq.insert(adapter_id.to_string(), 0);
        self.note(kinds::CORE_RESPAWN, serde_json::json!({ "adapter_id": adapter_id }))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 读侧：导出 AI 载荷与排障用
// ---------------------------------------------------------------------------

pub fn lesson_ids(root: &Path) -> Vec<String> {
    let dir = root.join("lessons");
    let mut ids = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

pub fn read_meta(root: &Path, lesson_id: &str) -> std::io::Result<Option<LessonMeta>> {
    let path = root.join("lessons").join(lesson_id).join("meta.json");
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 坏行跳过并计数，不当成致命错误：采集日志是现场唯一的证据来源，
/// 一行写坏不能让整节课导不出来。
pub fn read_records(root: &Path, lesson_id: &str) -> std::io::Result<(Vec<StoredRecord>, u64)> {
    let path = root.join("lessons").join(lesson_id).join("events.ndjson");
    let f = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e),
    };
    let mut records = Vec::new();
    let mut bad = 0u64;
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<StoredRecord>(&line) {
            Ok(r) => records.push(r),
            Err(_) => bad += 1,
        }
    }
    Ok((records, bad))
}

fn serde_json_to_vec<T: Serialize>(v: &T) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(v).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, data)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&tmp, path)
}
