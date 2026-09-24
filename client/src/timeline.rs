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
    /// 音频段的自证数据（rms/peak/speech_ms…）。
    ///
    /// 只有报了的源才填：人话那句是给模型读的，但观察端要把每段电平画成横条、
    /// 要把门限线叠上去，光靠"音频分段 1360ms"不够。没报的源这里就是没字段——
    /// 不是 0（"没采"和"采到 0"是两回事）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LessonStats {
    /// 课堂时间轴的长度（轨道上最远的事件结尾）。所有占比类指标都以它为分母。
    pub duration_ms: u64,
    /// 采集进程自己的墙钟时长。和 duration_ms 不一致就说明源是加速的、或采集停过。
    pub wall_ms: u64,
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
    /// 关键帧 blob 的字节总数。“要不要给抓屏加体积上限”这种决定，看的是这个数。
    pub keyframe_bytes: u64,
    pub audio_chunks: usize,
    pub audio_bytes: u64,
    /// 音频里真正是语音的时长总和。注意它来自 audio.chunk 本身而不是收课记录：
    /// 适配器挂在半路时就没有 close，但已经落盘的段依然是证据。
    pub audio_speech_ms: u64,
    /// 采集流报错次数之和（录音掉帧、抓屏后端重建…）。大于 0 就不该拿时长下结论。
    pub stream_errors: u64,
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
    /// 崩溃后被重启的次数。跨过这些边界的 seq 是新的，不能和前面比大小。
    pub restarts: u64,
    /// 声明会产出却一条都没有——现场排障最先看这个。
    pub silent: bool,
    /// 适配器自己报的收课统计（采了多少段、掉了几次帧）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<CloseStats>,
    /// 这一节实际生效的采集参数（由 session.open 自述）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vad: Option<VadSnapshot>,
    /// 抓屏源的开场自述（后端、显示器、生效参数）。故意留成 Value 而不是结构体：
    /// gdi 报 dpi_aware、dxgi 报 rebuilds，钉成同一份字段就会给没报的那一侧补 0。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<serde_json::Value>,
    /// 收课记录里 CloseStats 认不下的那些键（抓屏报的 polls / unchanged / throttled / capped）。
    /// 自动摘出来，是为了让“新适配器报了新事实”不必回头改核心——与开放 kind 同一条理由。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_extra: Option<serde_json::Value>,
}

/// 一个源在这一节课里报过的 `session.close` 汇总。
///
/// 多条会累加：将来支持当堂改门限后，一节课里会有好几代采集，每代一条 close。
/// 不累加就只能看见最后一段，而"整节课到底采到了多少"恰恰是这句要回答的问题。
#[derive(Debug, Clone, Serialize)]
pub struct CloseStats {
    /// 收到过几条 close。大于 1 意味着这一节用过不止一套参数。
    pub closes: u32,
    pub chunks: u64,
    pub bytes: u64,
    pub silent_chunks: u64,
    pub voiced_ms: u64,
    pub dropped_short: u64,
    pub stream_errors: u64,
    /// 采到的音频时长（与 wall_ms 对比才能知道源是不是加速的或停过）。
    pub audio_ms: u64,
    pub wall_ms: u64,
    /// 适配器自己报的失败原因（例如 blob 写不进去）。
    pub error: Option<String>,
}

/// 本节课真正生效的采集参数。磁盘上的声明可以被改，这份快照才是
/// "刚才那 45 分钟是用什么采出来的"——调参后两者不一致就是"还没生效"的凭据。
#[derive(Debug, Clone, Serialize)]
pub struct VadSnapshot {
    pub input: Option<String>,
    pub device: Option<String>,
    pub sample_rate: Option<u64>,
    pub frame_ms: u64,
    pub rms_open: f64,
    pub rms_close: f64,
    pub hangover_ms: u64,
    pub preroll_ms: u64,
    pub min_speech_ms: u64,
    pub max_segment_ms: u64,
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
    restart: HashMap<String, u64>,
    close: HashMap<String, CloseStats>,
    vad: HashMap<String, VadSnapshot>,
    screen: HashMap<String, serde_json::Value>,
    close_extra: HashMap<String, serde_json::Value>,
}

/// 上一代事件流的结尾与下一代起点之间的留白。必须大于单条事件自身的跨度
/// （一句话 1.8 秒），否则接缝会把两代叠回去。
const RESUME_GAP_MS: u64 = 3_000;

pub fn build(meta: &LessonMeta, records: &[StoredRecord]) -> AiPayload {
    let mut acc = Accum::default();
    let mut track: Vec<TrackItem> = Vec::with_capacity(records.len());
    let mut stats = LessonStats::default();
    let mut warnings = Vec::new();
    let mut notes = Vec::new();

    // 适配器重启后它自己的课堂时间会从 0 重新开始，直接落到同一根轴上就会和重启前
    // 那段叠在一起（守卫里三次 spawn 就造出了 3 份重叠时间轴，把 overlap 顶到超过
    // 总书写量）。用 core.respawn 的边界把每一代接在上一代之后。
    let mut boundaries: HashMap<String, Vec<u64>> = HashMap::new();
    for r in records {
        if r.envelope.kind == kinds::CORE_RESPAWN {
            if let Some(target) = r.envelope.payload.get("adapter_id").and_then(|v| v.as_str()) {
                boundaries.entry(target.to_string()).or_default().push(r.t_core_mono_us);
            }
        }
    }

    #[derive(Default)]
    struct Cursor {
        next: usize,
        base: u64,
        max_seen: u64,
    }
    let mut cursors: HashMap<String, Cursor> = HashMap::new();

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

        let (t_local, is_local) = t_of(r, meta, &mut origin);
        let base = {
            let c = cursors.entry(r.adapter_id.clone()).or_default();
            if let Some(v) = boundaries.get(&r.adapter_id) {
                while c.next < v.len() && v[c.next] <= r.t_core_mono_us {
                    // max_seen 已经是平移后的绝对时间，这里是赋值不是累加：
                    // 累加会把上一代用过的 base 再加一遍，接缝被推到几百秒之外。
                    c.base = c.max_seen + RESUME_GAP_MS;
                    c.next += 1;
                }
            }
            if is_local {
                let e = c.base + t_local;
                if e > c.max_seen {
                    c.max_seen = e;
                }
                c.base
            } else {
                0
            }
        };
        let t = base + t_local;
        let env = &r.envelope;
        match env.kind.as_str() {
            kinds::CORE_ADMIT => {
                // 记下"这个源声称会产出什么"，课后没见到就能标成 silent。
                // 这条记录是核心写的（adapter_id=core），所以归属必须取载荷里的源，
                // 否则会把别人的 produces 挂到 core 头上。
                let target = env
                    .payload
                    .get("adapter_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(r.adapter_id.as_str())
                    .to_string();
                let produces = env
                    .payload
                    .get("manifest")
                    .and_then(|m| m.get("produces"))
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                acc.declared.insert(target, produces);
            }
            kinds::CORE_RESPAWN => {
                // 重启不算进 track：它是关于采集过程本身的事实，不是课堂里发生的事。
                if let Some(t) = env.payload.get("adapter_id").and_then(|v| v.as_str()) {
                    *acc.restart.entry(t.to_string()).or_insert(0) += 1;
                }
            }
            kinds::SESSION_OPEN => {
                let w = env.payload.get("canvas_w").and_then(|v| v.as_f64());
                let h = env.payload.get("canvas_h").and_then(|v| v.as_f64());
                if let (Some(w), Some(h)) = (w, h) {
                    canvas = Some((w, h));
                }
                // 录音源自述了本节实际生效的门限与设备。后到的覆盖先到的：一节课里
                // 真重启过就该看最新那一代，而 close.closes > 1 会同时说明换过参数。
                if let Some(v) = vad_snapshot(&env.payload) {
                    acc.vad.insert(r.adapter_id.clone(), v);
                }
                if let Some(v) = screen_snapshot(&env.payload) {
                    acc.screen.insert(r.adapter_id.clone(), v);
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
                        detail: None,
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
                        detail: None,
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
                    detail: None,
                });
            }
            kinds::ASR_UTTERANCE => {
                if let Ok(u) = serde_json::from_value::<Utterance>(env.payload.clone()) {
                    stats.utterances += 1;
                    let (t0, t1) = (u.t0_ms + base, u.t1_ms + base);
                    let dur = t1.saturating_sub(t0);
                    if u.speaker.starts_with("teacher") {
                        stats.teacher_ms += dur;
                    } else {
                        stats.student_ms += dur;
                    }
                    speech_spans.push((t0, t1));
                    if let Some(prev) = last_speech_end {
                        let gap = t0.saturating_sub(prev);
                        if gap > longest_silence {
                            longest_silence = gap;
                        }
                    }
                    last_speech_end = Some(last_speech_end.map(|p| p.max(t1)).unwrap_or(t1));
                    track.push(TrackItem {
                        t0_ms: t0,
                        t1_ms: t1,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("{}：{}", u.speaker, u.text),
                        refs: Vec::new(),
                        detail: None,
                    });
                }
            }
            kinds::AUDIO_CHUNK => {
                if let Ok(c) = serde_json::from_value::<AudioChunk>(env.payload.clone()) {
                    stats.audio_chunks += 1;
                    audio_bytes += c.len;
                    // 语音时长从段里加，不等 close：适配器挂在半路时根本没有收课记录，
                    // 但已经落盘的段依旧是证据。
                    stats.audio_speech_ms +=
                        env.payload.get("speech_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                    track.push(TrackItem {
                        t0_ms: c.t0_ms + base,
                        t1_ms: c.t0_ms + base + c.dur_ms,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("音频分段 {}ms（{}，{} 字节）", c.dur_ms, c.codec, c.len),
                        refs: vec![c.blob],
                        // 观察端要画电平横条与门限参考线，靠上面那句人话不够。只带真报了名的字段——
                        // blob/len/codec/sample_rate/channels 是 AudioChunk 本身就要求的，
                        // 再带一份进 detail 会让"这个源什么都没报"永远不成立。
                        detail: pick(
                            &env.payload,
                            &["rms", "peak", "speech_ms", "trigger", "source"],
                        ),
                    });
                }
            }
            kinds::SCREEN_KEYFRAME => {
                if let Ok(k) = serde_json::from_value::<Keyframe>(env.payload.clone()) {
                    stats.keyframes += 1;
                    // 字节从每条事件累加，不等 close：适配器挂在半路时根本没有收课记录，
                    // 但已经落盘的图依旧是证据（与 audio_bytes 同一条理由）。
                    stats.keyframe_bytes += k.len;
                    let where_ = k.matched_page_id.clone().unwrap_or_else(|| "未匹配到课件页".to_string());
                    track.push(TrackItem {
                        t0_ms: k.t_ms + base,
                        t1_ms: k.t_ms + base,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("屏幕关键帧（触发={}，{where_}）", k.trigger),
                        refs: vec![k.blob],
                        // 观察端要排时间线、要说清"这张比上一张差多少"，光靠上面那句人话不够。
                        // 只带真报了名的字段：没报的不能补 0，否则"这个源什么也没说"永不成立。
                        detail: pick(&env.payload, &["trigger", "source", "dist", "mad", "width", "height", "dirty"]),
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
                    detail: None,
                });
            }
            kinds::EVAL_RECORD => {
                if let Ok(e) = serde_json::from_value::<EvalRecord>(env.payload.clone()) {
                    stats.eval_records += 1;
                    track.push(TrackItem {
                        t0_ms: e.t_ms + base,
                        t1_ms: e.t_ms + base,
                        source: r.adapter_id.clone(),
                        kind: env.kind.clone(),
                        text: format!("评估记录（{}）：{}", e.instrument, e.answers),
                        refs: Vec::new(),
                        detail: None,
                    });
                }
            }
            kinds::SESSION_CLOSE => {
                // 和 core.respawn 同理：收课统计是关于采集过程本身的事实，不是课堂上
                // 发生的事。塞进 track 还会顺手改掉 duration_ms（那是 max(t1)），
                // 于是"课有多长"被"进程跑了多久"污染。所以它只进每个源的健康表。
                let cur = close_of(&env.payload);
                if let Some(x) = close_extras(&env.payload) {
                    // 后到的覆盖先到的：一节课重启过就有好几代，而 close.closes > 1
                    // 已经说明换过参数，这里只需留最新一代的额外事实。
                    acc.close_extra.insert(r.adapter_id.clone(), x);
                }
                match acc.close.get_mut(&r.adapter_id) {
                    Some(prev) => add_close(prev, cur),
                    None => {
                        acc.close.insert(r.adapter_id.clone(), cur);
                    }
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
                    detail: None,
                });
            }
        }
    }

    stats.audio_bytes = audio_bytes;
    stats.pages_touched = pages.len();
    stats.longest_silence_ms = longest_silence;
    // 流错误次数只有一个来源：各源自己报的收课记录。再汇总进 stats，摘要与看板
    // 就不必各自去遍历 sources 算一遍（那些地方很容易算不一样）。
    stats.stream_errors = acc.close.values().map(|c| c.stream_errors).sum();
    for (id, c) in acc.close.iter() {
        if c.stream_errors > 0 {
            // 措辞不指名"录音"：这个字段现在有两个主人（a-audio 掉帧、a-screen 后端重建），
            // 把抓屏的重建说成掉帧会把人引向完全错的一头。
            warnings.push(format!(
                "{id} 报过 {} 次采集流错误（掉帧 / 后端重建）：跨过这些时刻的时长与时机类结论不成立",
                c.stream_errors
            ));
        }
        if let Some(e) = &c.error {
            warnings.push(format!("{id} 收课时报了错：{e}"));
        }
    }
    // 两个时钟分开记：轨道长度是"这节课有多长"，墙钟是"采集进程跑了多久"。
    // 真课堂上两者相等；采集器中途崩过、或机器睡过，它们就会岔开，
    // 那时任何拿墙钟当分母的占比都会算出离谱数字——所以这里宁肯显式报出来。
    stats.wall_ms = meta
        .ended_core_mono_us
        .map(|e| e.saturating_sub(meta.started_core_mono_us) / 1000)
        .unwrap_or(0);
    stats.duration_ms = track.iter().map(|i| i.t1_ms).max().unwrap_or(0);
    let dur = stats.duration_ms.max(1);
    stats.speech_ratio = (stats.teacher_ms + stats.student_ms) as f64 / dur as f64;
    if stats.wall_ms > 0 && stats.duration_ms > stats.wall_ms * 3 / 2 {
        warnings.push(format!(
            "课堂时间轴 {}s 比采集进程墙钟 {}s 长：源是加速的，或采集曾经停过——两个数不能混用",
            stats.duration_ms / 1000,
            stats.wall_ms / 1000
        ));
    }
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
                restarts: acc.restart.get(id).copied().unwrap_or(0),
                close: acc.close.get(id).cloned(),
                vad: acc.vad.get(id).cloned(),
                screen: acc.screen.get(id).cloned(),
                close_extra: acc.close_extra.get(id).cloned(),
                silent: !declared.is_empty() && acc.accepted.get(id).copied().unwrap_or(0) == 0,
            },
        );
    }
    out
}

/// 时间归一：优先用事件自带的课堂毫秒，其次用该适配器自己的单调钟相对第一条的偏移，
/// 最后才退回核心接收时间（它含投递抖动，只够用来排序，不够做对齐）。
/// 第二个返回值表示这个时间是不是"适配器自己的"——只有它才需要按重启边界整体平移。
fn t_of(r: &StoredRecord, meta: &LessonMeta, origin: &mut HashMap<String, u64>) -> (u64, bool) {
    if let Some(t) = r.envelope.t_event_ms {
        return (t, true);
    }
    if r.envelope.t_mono_us > 0 {
        let first = *origin.entry(r.adapter_id.clone()).or_insert(r.envelope.t_mono_us);
        return (r.envelope.t_mono_us.saturating_sub(first) / 1000, true);
    }
    (r.t_core_mono_us.saturating_sub(meta.started_core_mono_us) / 1000, false)
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

/// 只把"确实报了名"的字段带进 track。缺字段与字段为 0 是两回事：
/// 前者是源没报，后者是真的采到了静音，把两者混成一个 0 就是在造证据。
fn pick(src: &serde_json::Value, keys: &[&str]) -> Option<serde_json::Value> {
    let mut out = serde_json::Map::new();
    for k in keys {
        if let Some(v) = src.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    (!out.is_empty()).then_some(serde_json::Value::Object(out))
}

/// 解析一条 `session.close`。缺字段一律当 0：a-audiofile 只报 chunks/bytes/wall_s，
/// a-audio 报全套——同一份导出必须两种都能读，不能因谁少报一项就吞掉整条。
fn close_of(p: &serde_json::Value) -> CloseStats {
    let num = |k: &str| p.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let wall_s = p.get("wall_s").and_then(|v| v.as_f64()).unwrap_or(0.0);
    CloseStats {
        closes: 1,
        chunks: num("chunks"),
        bytes: num("bytes"),
        silent_chunks: num("silent_chunks"),
        voiced_ms: num("voiced_ms"),
        dropped_short: num("dropped_short"),
        stream_errors: num("stream_errors"),
        audio_ms: num("audio_ms"),
        // 两个适配器对"跑了多久"用了不同单位：wall_ms（毫秒）与 wall_s（秒）。
        wall_ms: num("wall_ms").max((wall_s * 1000.0) as u64),
        error: p.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()),
    }
}

/// 多条 close 累加。一节课里重启过就有好几代，每代只报自己那一段；
/// 取最后一条会把"整节课采到了多少"错报成"最后一代采到了多少"。
fn add_close(dst: &mut CloseStats, c: CloseStats) {
    dst.closes += 1;
    dst.chunks += c.chunks;
    dst.bytes += c.bytes;
    dst.silent_chunks += c.silent_chunks;
    dst.voiced_ms += c.voiced_ms;
    dst.dropped_short += c.dropped_short;
    dst.stream_errors += c.stream_errors;
    dst.audio_ms += c.audio_ms;
    dst.wall_ms += c.wall_ms;
    // 留住最早那条错：后面的失败往往是前一个的连带后果。
    if dst.error.is_none() {
        dst.error = c.error;
    }
}

/// 从 `session.open` 的自述里取本节实际生效的采集参数。没有 vad 块就不造快照：
/// 白板源的 open 里只有画布尺寸，硬凑一个全 0 的门限会让人以为它也在采音频。
fn vad_snapshot(p: &serde_json::Value) -> Option<VadSnapshot> {
    let v = p.get("vad")?;
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64());
    Some(VadSnapshot {
        input: p.get("input").and_then(|x| x.as_str()).map(|s| s.to_string()),
        device: p.get("device").and_then(|x| x.as_str()).map(|s| s.to_string()),
        sample_rate: p.get("sample_rate").and_then(|x| x.as_u64()),
        frame_ms: num("frame_ms")? as u64,
        rms_open: num("rms_open")?,
        rms_close: num("rms_close")?,
        hangover_ms: num("hangover_ms")? as u64,
        preroll_ms: num("preroll_ms")? as u64,
        min_speech_ms: num("min_speech_ms")? as u64,
        max_segment_ms: num("max_segment_ms")? as u64,
    })
}

/// 抓屏源的开场自述。整份留着，只把 source / lesson_id 摘掉（健康表外面已经有一份）。
/// 判据是载荷里有没有非空的 `screen` 参数对象：只有真的抓屏源会带它，
/// 所以录音源不会被误认成抓屏源。
fn screen_snapshot(p: &serde_json::Value) -> Option<serde_json::Value> {
    if p.get("screen")?.as_object()?.is_empty() {
        return None;
    }
    let mut out = p.as_object()?.clone();
    out.remove("source");
    out.remove("lesson_id");
    Some(serde_json::Value::Object(out))
}

/// `CloseStats` 已经认下的键。剩下的原样进 `close_extra`，不丢——
/// 一个适配器自创的收课事实（抓屏的 polls / unchanged / throttled / capped）
/// 不该因为核心不认识它而消失。
const CLOSE_KNOWN: &[&str] = &[
    "source",
    "lesson_id",
    "chunks",
    "bytes",
    "silent_chunks",
    "voiced_ms",
    "dropped_short",
    "stream_errors",
    "audio_ms",
    "wall_ms",
    "wall_s",
    "error",
];

fn close_extras(p: &serde_json::Value) -> Option<serde_json::Value> {
    let m = p.as_object()?;
    let mut out = serde_json::Map::new();
    for (k, v) in m {
        if !CLOSE_KNOWN.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    (!out.is_empty()).then_some(serde_json::Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::{build, pick};
    use classagent_schema::{kinds, Envelope, LessonInfo, LessonMeta, RecordStatus, StoredRecord};

    fn meta() -> LessonMeta {
        LessonMeta {
            info: LessonInfo {
                lesson_id: "L-test".into(),
                prev_lesson_id: None,
                subject: None,
                class: None,
                teacher: None,
                started_at_utc_ms: 0,
                courseware: vec![],
                blob_dir: "data/lessons/L-test/blobs".into(),
                params: serde_json::json!({}),
            },
            started_core_mono_us: 0,
            ended_core_mono_us: Some(60_000_000),
            stop_reason: None,
        }
    }

    fn rec(id: &str, seq: u64, kind: &str, t_ms: u64, payload: serde_json::Value) -> StoredRecord {
        StoredRecord {
            global_seq: seq,
            adapter_id: id.into(),
            t_core_utc_ms: 0,
            t_core_mono_us: t_ms * 1_000,
            status: RecordStatus::Accepted,
            gap: None,
            envelope: Envelope::new(seq, kind, Some(t_ms), payload),
            raw: None,
        }
    }

    fn open() -> StoredRecord {
        rec(
            "a-audio",
            1,
            kinds::SESSION_OPEN,
            0,
            serde_json::json!({
                "input": "device", "device": "麦克风", "sample_rate": 48000,
                "vad": {"frame_ms": 20, "rms_open": 45.0, "rms_close": 35.0, "hangover_ms": 500,
                         "preroll_ms": 150, "min_speech_ms": 300, "max_segment_ms": 10000}
            }),
        )
    }

    fn chunk(seq: u64, t0: u64, extra: serde_json::Value) -> StoredRecord {
        let mut p = serde_json::json!({
            "blob": format!("audio-{t0:012}ms.wav"), "len": 130604, "codec": "pcm_s16le",
            "sample_rate": 48000, "channels": 1, "t0_ms": t0, "dur_ms": 2000, "silent": false
        });
        for (k, v) in extra.as_object().into_iter().flat_map(|m| m.iter()) {
            p[k] = v.clone();
        }
        rec("a-audio", seq, kinds::AUDIO_CHUNK, t0, p)
    }

    #[test]
    fn close_is_no_longer_an_unrecognized_event() {
        let records = vec![
            open(),
            chunk(2, 1_000, serde_json::json!({"rms": 117.0, "peak": 1193, "speech_ms": 1360, "trigger": "vad"})),
            rec(
                "a-audio",
                3,
                kinds::SESSION_CLOSE,
                8_000_000,
                serde_json::json!({"chunks": 1, "bytes": 130604, "voiced_ms": 1360, "dropped_short": 1,
                                   "stream_errors": 0, "audio_ms": 7900, "wall_ms": 8000, "error": null}),
            ),
        ];
        let p = build(&meta(), &records);
        assert!(
            !p.track.iter().any(|i| i.text.contains("未识别事件")),
            "收课记录不该以「未识别事件 + 截断」的形式交给模型：{:?}",
            p.track.iter().map(|i| i.text.clone()).collect::<Vec<_>>()
        );
        let h = &p.sources["a-audio"];
        let c = h.close.clone().expect("收课统计要进健康表");
        assert_eq!((c.closes, c.chunks, c.bytes, c.voiced_ms, c.dropped_short), (1, 1, 130604, 1360, 1));
        assert_eq!(c.stream_errors, 0);
        assert_eq!(p.stats.stream_errors, 0);
        assert_eq!(p.stats.audio_speech_ms, 1360);
        // 收课记录自己的时间戳是"进程跑了多久"，不能把它当成"课有多长"。
        assert_eq!(p.stats.duration_ms, 3_000, "close 不能把时间轴拉长");
        let v = h.vad.as_ref().expect("session.open 的自述要能被读出来");
        assert_eq!((v.rms_open, v.rms_close, v.max_segment_ms), (45.0, 35.0, 10000));
        assert_eq!(v.sample_rate, Some(48000));
    }

    #[test]
    fn a_sparse_close_from_another_adapter_reads_as_zeros() {
        // a-audiofile 只报 chunks/bytes/wall_s：少报的项必须是 0，而不是抱掉整条。
        let records = vec![rec(
            "a-audiofile",
            1,
            kinds::SESSION_CLOSE,
            5_000,
            serde_json::json!({"source": "a-audiofile", "chunks": 7, "bytes": 220500, "wall_s": 12.34}),
        )];
        let p = build(&meta(), &records);
        let c = p.sources["a-audiofile"].close.clone().expect("残缺的 close 也是一条统计");
        assert_eq!((c.chunks, c.bytes, c.wall_ms, c.voiced_ms, c.stream_errors), (7, 220500, 12340, 0, 0));
        assert!(p.warnings.is_empty(), "没掉帧就不该报警：{:?}", p.warnings);
    }

    #[test]
    fn several_generations_in_one_lesson_add_up() {
        // 将来当堂改门限会一节课里好几代；取最后一条会把"整节课采到多少"错报成
        // "最后一代采到多少"，而掉帧次数也会被吞掉一半。
        let close = |seq: u64, chunks: u64, errs: u64| {
            rec(
                "a-audio",
                seq,
                kinds::SESSION_CLOSE,
                seq * 1000,
                serde_json::json!({"chunks": chunks, "bytes": chunks * 100, "voiced_ms": chunks * 900,
                                   "dropped_short": 1, "stream_errors": errs, "audio_ms": 1000, "wall_ms": 1000}),
            )
        };
        let p = build(&meta(), &vec![close(1, 2, 1), close(2, 3, 2)]);
        let c = p.sources["a-audio"].close.clone().unwrap();
        assert_eq!((c.closes, c.chunks, c.voiced_ms, c.stream_errors), (2, 5, 4500, 3));
        assert_eq!(p.stats.stream_errors, 3);
        assert!(
            // 钉新措辞，而且钉上源名：“报过 N 次”属于哪个源，才是这条警告有用的前提。
            p.warnings.iter().any(|w| w.contains("a-audio") && w.contains("报过 3 次采集流错误")),
            "掉过帧必须写进「不能下什么结论」，且要说是哪个源：{:?}",
            p.warnings
        );
    }

    #[test]
    fn audio_detail_only_carries_reported_fields() {
        let with = build(
            &meta(),
            &vec![chunk(1, 1_000, serde_json::json!({"rms": 251.0, "speech_ms": 8820}))],
        );
        let d = with.track[0].detail.clone().expect("报了 rms 就该看得见");
        assert_eq!(d["rms"], serde_json::json!(251.0));
        assert!(d.get("peak").is_none(), "没报的字段不能凭空补一个 0");

        let without = build(&meta(), &vec![chunk(1, 1_000, serde_json::json!({}))]);
        assert!(without.track[0].detail.is_none(), "旧版源什么都没报时不该出现空对象");
        assert_eq!(without.stats.audio_speech_ms, 0);
    }

    #[test]
    fn screen_source_reports_backend_params_and_detail() {
        // 抓屏源进导出时要能看到三件事：用的哪条后端、本节课生效的参数、
        // 每张关键帧"比上一张差多少"。缺任何一个，观察端就只能在人话上猜。
        let open = rec(
            "a-screen",
            1,
            kinds::SESSION_OPEN,
            0,
            serde_json::json!({
                "source": "a-screen", "lesson_id": "L-test", "input": "device",
                "backend": "gdi", "monitor": 0, "width": 1920, "height": 1080, "dpi_aware": true,
                "screen": { "poll_ms": 200, "min_dist": 6, "min_mad": 4 }
            }),
        );
        let kf = rec(
            "a-screen",
            2,
            kinds::SCREEN_KEYFRAME,
            4_000,
            serde_json::json!({
                "blob": "screen-000000004000ms-00002.png", "len": 4096, "t_ms": 4_000,
                "trigger": "phash", "dist": 11, "mad": 7, "width": 1920, "height": 1080,
                "dirty": [120, 40, 800, 600], "source": "a-screen"
            }),
        );
        let close = rec(
            "a-screen",
            3,
            kinds::SESSION_CLOSE,
            9_000,
            serde_json::json!({
                "source": "a-screen", "chunks": 1, "bytes": 4096, "wall_ms": 9_000,
                "polls": 45, "unchanged": 40, "throttled": 3, "capped": 0, "stream_errors": 2
            }),
        );
        let p = build(&meta(), &vec![open, kf, close]);
        let h = &p.sources["a-screen"];
        let s = h.screen.clone().expect("抓屏自述要进健康表");
        assert_eq!(s["backend"], "gdi");
        assert_eq!(s["screen"]["min_dist"], 6);
        assert!(s.get("source").is_none(), "自述里不该再留一份 source");
        let x = h.close_extra.clone().expect("CloseStats 认不下的收课字段要原样留着");
        assert_eq!(x["polls"], serde_json::json!(45));
        assert_eq!(x["unchanged"], serde_json::json!(40));
        assert_eq!(x["throttled"], serde_json::json!(3));
        assert_eq!(p.stats.keyframes, 1);
        assert_eq!(p.stats.keyframe_bytes, 4096);
        let d = p.track.iter().find(|t| t.kind == kinds::SCREEN_KEYFRAME).unwrap().detail.clone().unwrap();
        // 三项得分开比：凑成一个元组就要把 Value 从索引里搬出来，而 Value 不是 Copy。
        assert_eq!(d["dist"], serde_json::json!(11));
        assert_eq!(d["trigger"], serde_json::json!("phash"));
        assert_eq!(d["dirty"], serde_json::json!([120, 40, 800, 600]));
        // 抓屏后端的重建不许被说成"录音掉帧"。
        assert!(p.warnings.iter().any(|w| w.contains("采集流错误") && w.contains("a-screen")), "{:?}", p.warnings);
        assert!(!p.warnings.iter().any(|w| w.contains("录音")), "抓屏的错不该安到录音头上：{:?}", p.warnings);
    }

    #[test]
    fn an_unmeasured_field_stays_null_instead_of_becoming_zero() {
        // 开场那一帧没有上一帧可比，适配器报的是 null。导出必须原样带出去：
        // 换成 0 会被读成“完全没变”，整个抹掉会被读成“这个源没报这个字段”。
        let v = serde_json::json!({ "trigger": "open", "dist": null, "mad": null });
        let d = pick(&v, &["trigger", "dist", "mad"]).unwrap();
        let m = d.as_object().unwrap();
        assert!(m.contains_key("dist") && m["dist"].is_null(), "null 不能被动丢掉：{d}");
        assert!(m.contains_key("mad") && m["mad"].is_null(), "{d}");
        assert_eq!(d["trigger"], "open");
    }

    #[test]
    fn audio_only_close_has_no_extra_and_no_screen_block() {
        // 反向断言：新增的两个字段不许给只有录音的课凭空多出东西来。
        let p = build(&meta(), &vec![chunk(1, 1_000, serde_json::json!({"rms": 300.0}))]);
        let h = &p.sources["a-audio"];
        assert!(h.screen.is_none(), "录音源不该有抓屏自述");
        assert!(h.close_extra.is_none(), "只有通用字段的 close 不该产生 extra");
        assert_eq!(p.stats.keyframes, 0);
    }

    #[test]
    fn non_audio_rows_keep_their_exact_shape() {
        // detail 用 skip_serializing_if：其他行的导出字节必须和加字段之前一样，
        // 否则服务端存过的历史 payload 与新生成的会对不上。
        let p = build(&meta(), &vec![rec("a-fake", 1, kinds::INK_STROKE_DELETE, 10, serde_json::json!({"reason": "undo"}))]);
        let s = serde_json::to_string(&p.track[0]).unwrap();
        assert!(!s.contains("detail"), "不该给非音频行凭空加字段：{s}");
        assert!(!s.contains("close"));
    }
}
