//! 把多源事件压成一条 AI 能读的时间轴。
//!
//! 这是"数据怎么交给 AI"的答案所在，三个判断写在这里：
//!
//! 1. **绝不把墨迹点列直接喂模型。** 模型看坐标序列没有意义，只会烧 token。
//!    点列在这里被折叠成一句人话（第几页、什么时候、写了多久、在板面哪个区域），
//!    原始点仍留在 `events.ndjson` 里，需要复现笔迹时再取。
//! 2. **不同源按时间交错成一条 track**，所以"老师说这句话时黑板上正在写什么"
//!    是模型自己能看见的结构，而不是我们事后猜的。
//! 3. **产出是四份不同投影，不是一次大 prompt。** 观察日志要的是 3 秒采样式编码
//!    （规则活），笔记要的是语义压缩（模型活）。混在一个 prompt 里会互相污染。

use classagent_schema::{
    kinds, AudioChunk, EvalRecord, Keyframe, LessonInfo, LessonMeta, PageActivate, RecordStatus, StrokeCommit,
    StoredRecord, Utterance,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
pub struct TrackItem {
    pub t0_ms: u64,
    pub t1_ms: u64,
    pub source: String,
    pub kind: String,
    /// 供模型阅读的一句话表述。
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LessonStats {
    pub duration_ms: u64,
    pub utterances: usize,
    pub teacher_ms: u64,
    pub student_ms: u64,
    pub speech_ratio: f64,
    pub longest_silence_ms: u64,
    pub strokes: usize,
    pub ink_time_ms: u64,
    pub erases: usize,
    pub pages_touched: usize,
    pub writing_while_speaking_ms: u64,
    pub keyframes: usize,
    pub audio_chunks: usize,
    pub audio_bytes: u64,
    pub eval_records: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceHealth {
    pub declared: Vec<String>,
    pub events: u64,
    pub accepted: u64,
    pub gaps: u64,
    pub lost_events: u64,
    pub over_budget: u64,
    pub rejected: u64,
    pub duplicates: u64,
    /// 声明会产出却一条都没有——现场排障最先看这个。
    pub silent: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiPayload {
    pub proto: u8,
    pub generated_at_utc_ms: u64,
    pub lesson: LessonInfo,
    pub prev_lesson_id: Option<String>,
    pub sources: HashMap<String, SourceHealth>,
    pub stats: LessonStats,
    pub track: Vec<TrackItem>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Default)]
struct Accum {
    declared: HashMap<String, Vec<String>>,
    events: HashMap<String, u64>,
    accepted: HashMap<String, u64>,
    gaps: HashMap<String, u64>,
    lost: HashMap<String, u64>,
    over: HashMap<String, u64>,
    rejected: HashMap<String, u64>,
    dup: HashMap<String, u64>,
}

pub fn build(meta: &LessonMeta, records: &[StoredRecord]) -> AiPayload {
    let mut acc = Accum::default();
    let mut track: Vec<TrackItem> = Vec::with_capacity(records.len());
    let mut stats = LessonStats::default();
    let mut warnings = Vec::new();
    let mut notes = Vec::new();

    let mut origin: HashMap<String, u64> = HashMap::new();
    let mut canvas: Option<(f64, f64)> = None;
    let mut ink_spans: Vec<(u64, u64)> = Vec::new();
    let mut speech_spans: Vec<(u64, u64)> = Vec::new();
    let mut pages: HashSet<String> = HashSet::new();
    let mut last_speech_end: Option<u64> = None;
    let mut longest_silence: u64 = 0;
    let mut audio_bytes = 0u64;

    for r in records {
        *acc.events.entry(r.adapter_id.clone()).or_insert(0) += 1;
        match r.status {
            RecordStatus::Accepted => *acc.accepted.entry(r.adapter_id.clone()).or_insert(0) += 1,
            RecordStatus::GapBefore => {
                *acc.gaps.entry(r.adapter_id.clone()).or_insert(0) += 1;
                *acc.lost.entry(r.adapter_id.clone()).or_insert(0) += r.gap.unwrap_or(0);
            }
            RecordStatus::OverBudget => *acc.over.entry(r.adapter_id.clone()).or_insert(0) += 1,
            RecordStatus::Rejected => *acc.rejected.entry(r.adapter_id.clone()).or_insert(0) += 1,
            RecordStatus::Duplicate => *acc.dup.entry(r.adapter_id.clone()).or_insert(0) += 1,
        }

        let t = t_of(r, meta, &mut origin);
        let env = &r.envelope;
        match env.kind.as_str() {
            kinds::CORE_ADMIT => {
                // 记下"这个源声称会产出什么"，课后没见到就能标成 silent。
                let produces = env
                    .payload
                    .get("manifest")
                    .and_then(|m| m.get("produces"))
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                acc.declared.insert(r.adapter_id.clone(), produces);
            }
            kinds::SESSION_OPEN => {
                let w = env.payload.get("canvas_w").and_then(|v| v.as_f64());
                let h = env.payload.get("canvas_h").and_then(|v| v.as_f64());
                if let (Some(w), Some(h)) = (w, h) {
                    canvas = Some((w, h));
                }
            }
            kinds::INK_PAGE_ACTIVATE => {
                if let Ok(p) = serde_json::from_value::<PageActivate>(env.payload.clone()) {
                    pages.insert(p.page_id.clone());
                    track.push(TrackItem {
                        t0_ms: t,
                        t1_ms: t,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("切到第 {} 页（page_id={}）", p.index + 1, p.page_id),
                        refs: vec![p.page_id],
                    });
                }
            }
            kinds::INK_STROKE_COMMIT => {
                if let Ok(s) = serde_json::from_value::<StrokeCommit>(env.payload.clone()) {
                    let t1 = t + s.duration_ms;
                    stats.strokes += 1;
                    stats.ink_time_ms += s.duration_ms;
                    pages.insert(s.page_id.clone());
                    ink_spans.push((t, t1));
                    track.push(TrackItem {
                        t0_ms: t,
                        t1_ms: t1,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: describe_stroke(&s, canvas),
                        refs: vec![s.stroke_id, s.page_id],
                    });
                }
            }
            kinds::INK_STROKE_DELETE => {
                stats.erases += 1;
                let reason = env.payload.get("reason").and_then(|v| v.as_str()).unwrap_or("write").to_string();
                let sid = env.payload.get("stroke_id").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                track.push(TrackItem {
                    t0_ms: t,
                    t1_ms: t,
                    source: r.adapter_id.clone(),
                    kind: env.kind.clone(),
                    text: format!("撤销/擦除一笔（{reason}）：{sid}"),
                    refs: vec![sid],
                });
            }
            kinds::ASR_UTTERANCE => {
                if let Ok(u) = serde_json::from_value::<Utterance>(env.payload.clone()) {
                    stats.utterances += 1;
                    let dur = u.t1_ms.saturating_sub(u.t0_ms);
                    if u.speaker.starts_with("teacher") {
                        stats.teacher_ms += dur;
                    } else {
                        stats.student_ms += dur;
                    }
                    speech_spans.push((u.t0_ms, u.t1_ms));
                    if let Some(prev) = last_speech_end {
                        let gap = u.t0_ms.saturating_sub(prev);
                        if gap > longest_silence {
                            longest_silence = gap;
                        }
                    }
                    last_speech_end = Some(last_speech_end.map(|p| p.max(u.t1_ms)).unwrap_or(u.t1_ms));
                    track.push(TrackItem {
                        t0_ms: u.t0_ms,
                        t1_ms: u.t1_ms,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("{}：{}", u.speaker, u.text),
                        refs: Vec::new(),
                    });
                }
            }
            kinds::AUDIO_CHUNK => {
                if let Ok(c) = serde_json::from_value::<AudioChunk>(env.payload.clone()) {
                    stats.audio_chunks += 1;
                    audio_bytes += c.len;
                    track.push(TrackItem {
                        t0_ms: c.t0_ms,
                        t1_ms: c.t0_ms + c.dur_ms,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("音频分段 {}ms（{}，{} 字节）", c.dur_ms, c.codec, c.len),
                        refs: vec![c.blob],
                    });
                }
            }
            kinds::SCREEN_KEYFRAME => {
                if let Ok(k) = serde_json::from_value::<Keyframe>(env.payload.clone()) {
                    stats.keyframes += 1;
                    let where_ = k.matched_page_id.clone().unwrap_or_else(|| "未匹配到课件页".to_string());
                    track.push(TrackItem {
                        t0_ms: k.t_ms,
                        t1_ms: k.t_ms,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("屏幕关键帧（触发={}，{where_}）", k.trigger),
                        refs: vec![k.blob],
                    });
                }
            }
            kinds::COURSEWARE_PAGE => {
                let page = env.payload.get("page_index").and_then(|v| v.as_u64()).map(|v| v.to_string());
                track.push(TrackItem {
                    t0_ms: t,
                    t1_ms: t,
                    source: r.adapter_id.clone(),
                    kind: env.kind.clone(),
                    text: format!("课件翻到第 {} 页", page.unwrap_or_else(|| "?".into())),
                    refs: Vec::new(),
                });
            }
            kinds::EVAL_RECORD => {
                if let Ok(e) = serde_json::from_value::<EvalRecord>(env.payload.clone()) {
                    stats.eval_records += 1;
                    track.push(TrackItem {
                        t0_ms: e.t_ms,
                        t1_ms: e.t_ms,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("评估记录（{}）：{}", e.instrument, e.answers),
                        refs: Vec::new(),
                    });
                }
            }
            other => {
                // 新 kind 不需要改核心就能落盘；这里只保证它在 track 里可见。
                track.push(TrackItem {
                    t0_ms: t,
                    t1_ms: t,
                    source: r.adapter_id.clone(),
                    kind: env.kind.clone(),
                    text: format!("未识别事件 {other}：{}", short(&env.payload)),
                    refs: Vec::new(),
                });
            }
        }
    }

    stats.audio_bytes = audio_bytes;
    stats.pages_touched = pages.len();
    stats.longest_silence_ms = longest_silence;
    stats.duration_ms = meta
        .ended_core_mono_us
        .map(|e| e.saturating_sub(meta.started_core_mono_us) / 1000)
        .unwrap_or_else(|| track.iter().map(|i| i.t1_ms).max().unwrap_or(0));
    let dur = stats.duration_ms.max(1);
    stats.speech_ratio = (stats.teacher_ms + stats.student_ms) as f64 / dur as f64;
    stats.writing_while_speaking_ms = overlap_ms(&ink_spans, &speech_spans);

    if let Some(m) = meta.info.params.get("expected_adapters").and_then(|v| v.as_array()) {
        for id in m.iter().filter_map(|v| v.as_str()) {
            if !acc.events.contains_key(id) {
                warnings.push(format!("期望的适配器 {id} 全程没有产出任何事件"));
            }
        }
    }

    track.sort_by(|a, b| a.t0_ms.cmp(&b.t0_ms).then_with(|| a.kind.cmp(&b.kind)));

    notes.push("track 里的 ink 条目是折叠后的描述，原始点列仍在 events.ndjson。".into());
    notes.push("若要做 FIAC/iFIAS 式 3 秒编码，请对 track 做规则采样后再交模型判类，不要让模型自己数时间。".into());

    AiPayload {
        proto: classagent_schema::PROTO,
        generated_at_utc_ms: classagent_schema::utc_ms(),
        lesson: meta.info.clone(),
        prev_lesson_id: meta.info.prev_lesson_id.clone(),
        sources: health(&acc),
        stats,
        track,
        warnings,
        notes,
    }
}

fn health(acc: &Accum) -> HashMap<String, SourceHealth> {
    let mut out = HashMap::new();
    for (id, events) in &acc.events {
        let declared = acc.declared.get(id).cloned().unwrap_or_default();
        out.insert(
            id.clone(),
            SourceHealth {
                declared: declared.clone(),
                events: *events,
                accepted: acc.accepted.get(id).copied().unwrap_or(0),
                gaps: acc.gaps.get(id).copied().unwrap_or(0),
                lost_events: acc.lost.get(id).copied().unwrap_or(0),
                over_budget: acc.over.get(id).copied().unwrap_or(0),
                rejected: acc.rejected.get(id).copied().unwrap_or(0),
                duplicates: acc.dup.get(id).copied().unwrap_or(0),
                silent: !declared.is_empty() && acc.accepted.get(id).copied().unwrap_or(0) == 0,
            },
        );
    }
    out
}

/// 时间归一：优先用事件自带的课堂毫秒，其次用该适配器自己的单调钟相对第一条的偏移，
/// 最后才退回核心接收时间（它含投递抖动，只够用来排序，不够做对齐）。
fn t_of(r: &StoredRecord, meta: &LessonMeta, origin: &mut HashMap<String, u64>) -> u64 {
    if let Some(t) = r.envelope.t_event_ms {
        return t;
    }
    if r.envelope.t_mono_us > 0 {
        let first = *origin.entry(r.adapter_id.clone()).or_insert(r.envelope.t_mono_us);
        return r.envelope.t_mono_us.saturating_sub(first) / 1000;
    }
    r.t_core_mono_us.saturating_sub(meta.started_core_mono_us) / 1000
}

fn describe_stroke(s: &StrokeCommit, canvas: Option<(f64, f64)>) -> String {
    let region = match (s.bbox, canvas) {
        (Some(b), Some((cw, ch))) if cw > 0.0 && ch > 0.0 => {
            let cx = (b[0] + b[2]) / 2.0;
            let cy = (b[1] + b[3]) / 2.0;
            format!("{}", grid(cx / cw, cy / ch))
        }
        (Some(b), _) => format!("包围盒 [{}, {}, {}, {}]", b[0], b[1], b[2], b[3]),
        (None, _) => "位置未知".to_string(),
    };
    let mut out = format!(
        "{} 页写下一笔：{}，{} 点，用时 {}ms，位于{}",
        s.page_id,
        s.tool,
        s.points.len(),
        s.duration_ms,
        region
    );
    if let Some(d) = &s.decimation {
        out.push_str(&format!("；抽稀={d}"));
    }
    out
}

/// 板面九宫格。模型不需要坐标，需要"写在黑板哪个位置、是不是留着"。
fn grid(nx: f64, ny: f64) -> &'static str {
    let col = if nx < 0.38 { '左' } else if nx > 0.62 { '右' } else { '中' };
    let row = if ny < 0.38 { "上" } else if ny > 0.62 { "下" } else { "中" };
    match (col, row) {
        ('左', "上") => "左上",
        ('中', "上") => "正上",
        ('右', "上") => "右上",
        ('左', "中") => "左侧",
        ('中', "中") => "中央",
        ('右', "中") => "右侧",
        ('左', _) => "左下",
        ('中', _) => "正下",
        _ => "右下",
    }
}

fn overlap_ms(a: &[(u64, u64)], b: &[(u64, u64)]) -> u64 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut bs: Vec<(u64, u64)> = b.iter().copied().collect();
    bs.sort_by_key(|s| s.0);
    let mut total = 0u64;
    for &(s, e) in a {
        for &(bs0, be0) in &bs {
            if bs0 > e {
                break;
            }
            if be0 > s {
                total += e.min(be0).saturating_sub(s.max(bs0));
            }
        }
    }
    total
}

fn short(v: &serde_json::Value) -> String {
    let s = v.to_string();
    let t: String = s.chars().take(160).collect();
    if t.chars().count() < s.chars().count() {
        format!("{t}…")
    } else {
        t
    }
}
