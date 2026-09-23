//! 一节课的可读摘要：不接模型也能读。
//!
//! 刻意只做两件事：把已经算出来的量排成一页纸，以及**明说这一节采集不到什么**。
//! 后半句比前半句重要——观察日志最容易出的事故是把"没采到"读成"没发生"。
//! 看板与 CLI 共用这个函数，所以 CI 断言过的文本就是你在窗口里看到的文本。

use crate::timeline::AiPayload;
use classagent_schema::kinds;
use std::collections::BTreeMap;

pub const DEFAULT_BUCKET_MS: u64 = 60_000;

#[derive(Default)]
struct Bucket {
    speech_ms: u64,
    utterances: usize,
    strokes: usize,
    ink_ms: u64,
    erases: usize,
    keyframes: usize,
    pages: Vec<String>,
}

pub fn render(p: &AiPayload) -> String {
    render_with(p, DEFAULT_BUCKET_MS)
}

pub fn render_with(p: &AiPayload, bucket_ms: u64) -> String {
    let bucket_ms = bucket_ms.max(1_000);
    let st = &p.stats;
    let dur = st.duration_ms.max(1);
    let mut out = String::new();

    // ---- 抬头 ----
    let title = format!(
        "{} · {}",
        p.lesson.class.clone().unwrap_or_else(|| "班级未知".into()),
        p.lesson.subject.clone().unwrap_or_else(|| "学科未知".into())
    );
    out.push_str(&format!("{title}\n"));
    out.push_str(&format!(
        "教师 {}   课次 {}   开始 {}\n",
        p.lesson.teacher.clone().unwrap_or_else(|| "-".into()),
        p.lesson.lesson_id,
        utc_text(p.lesson.started_at_utc_ms),
    ));
    match &p.prev_lesson_id {
        Some(prev) => out.push_str(&format!("上一节 {prev}（摘要会接续该链）\n")),
        None => out.push_str("上一节 未指定（本节是这条链的第一节，无法接续前情）\n"),
    }
    out.push_str(&format!(
        "课堂时间轴 {}   采集进程 {}   事件总量 {}\n\n",
        mmss(dur),
        mmss(st.wall_ms),
        st.strokes + st.utterances + st.keyframes + st.audio_chunks + st.eval_records
    ));

    // ---- 一、量的分布 ----
    out.push_str("一、量的分布\n");
    out.push_str(&format!(
        "  讲话 {}（教师 {} / 学生 {}），最长一次静默 {}\n",
        mmss(st.teacher_ms + st.student_ms),
        mmss(st.teacher_ms),
        mmss(st.student_ms),
        mmss(st.longest_silence_ms)
    ));
    out.push_str(&format!(
        "  板书 {} 笔 / {}，其中 {}（{}）是边讲边写\n",
        st.strokes,
        mmss(st.ink_time_ms),
        mmss(st.writing_while_speaking_ms),
        pct(st.writing_while_speaking_ms, st.ink_time_ms.max(1))
    ));
    out.push_str(&format!(
        "  板面出现 {} 页，撤销或擦除 {} 次；屏幕证据帧 {} 张\n",
        st.pages_touched, st.erases, st.keyframes
    ));
    out.push_str(&format!(
        "  音频 {} 段 / {}；评估记录 {} 条\n\n",
        st.audio_chunks,
        human(st.audio_bytes),
        st.eval_records
    ));

    // ---- 二、逐格时间轴 ----
    let mut buckets: BTreeMap<u64, Bucket> = BTreeMap::new();
    let mut page_first: BTreeMap<&str, u64> = BTreeMap::new();
    let mut page_strokes: BTreeMap<&str, usize> = BTreeMap::new();
    for i in &p.track {
        let b = buckets.entry(i.t0_ms / bucket_ms * bucket_ms).or_default();
        match i.kind.as_str() {
            kinds::ASR_UTTERANCE => {
                b.speech_ms += i.t1_ms.saturating_sub(i.t0_ms);
                b.utterances += 1;
            }
            kinds::INK_STROKE_COMMIT => {
                b.strokes += 1;
                b.ink_ms += i.t1_ms.saturating_sub(i.t0_ms);
                if let Some(pg) = i.refs.get(1) {
                    page_first.entry(pg.as_str()).or_insert(i.t0_ms);
                    *page_strokes.entry(pg.as_str()).or_insert(0) += 1;
                    b.pages.push(pg.clone());
                }
            }
            kinds::INK_STROKE_DELETE => b.erases += 1,
            kinds::INK_PAGE_ACTIVATE => {
                if let Some(pg) = i.refs.first() {
                    page_first.entry(pg.as_str()).or_insert(i.t0_ms);
                    b.pages.push(pg.clone());
                }
            }
            kinds::SCREEN_KEYFRAME => b.keyframes += 1,
            _ => {}
        }
    }
    // 没有关键帧就不占一列：摘要要在窄窗口里读，横向滚动等于没有。
    let has_frames = st.keyframes > 0;
    out.push_str(&format!(
        "二、逐 {} 秒一格（{}）\n",
        bucket_ms / 1000,
        if has_frames { "说话% · 笔 · 擦 · 帧 · 页" } else { "说话% · 笔 · 擦 · 页" }
    ));
    for (t0, b) in &buckets {
        let bar = "#".repeat(((b.speech_ms as f64 / bucket_ms as f64) * 12.0).clamp(0.0, 12.0) as usize);
        let speech = (b.speech_ms as f64 * 100.0 / bucket_ms as f64) as u32;
        let line = if has_frames {
            format!(
                "  {} {:<12} 说话{:>3}% 笔{:>4} 擦{:>3} 帧{:>3}  {}",
                mmss(*t0), bar, speech, b.strokes, b.erases, b.keyframes, page_summary(&b.pages)
            )
        } else {
            format!(
                "  {} {:<12} 说话{:>3}% 笔{:>4} 擦{:>3}  {}",
                mmss(*t0), bar, speech, b.strokes, b.erases, page_summary(&b.pages)
            )
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.push('\n');

    // ---- 三、板面轨迹 ----
    out.push_str("三、板面轨迹（首次出现时间 · 该页笔数）\n");
    if page_first.is_empty() {
        out.push_str("  本节没有任何笔迹事件\n");
    } else {
        for (pg, t) in &page_first {
            out.push_str(&format!("  {pg:<16} {:>7} · {:>4} 笔\n", mmss(*t), page_strokes.get(*pg).copied().unwrap_or(0)));
        }
        out.push('\n');
    }

    // ---- 四、采集健康 ----
    out.push_str("四、采集健康（这一节的数据能支撑到哪一步）\n");
    let ids: Vec<&String> = {
        let mut v: Vec<&String> = p.sources.keys().collect();
        v.sort();
        v
    };
    for id in ids {
        let s = &p.sources[id.as_str()];
        let mut flags = Vec::new();
        if s.gaps > 0 {
            flags.push(format!("缺口 {} 处/丢 {} 条", s.gaps, s.lost_events));
        }
        if s.restarts > 0 {
            flags.push(format!("重启 {} 次", s.restarts));
        }
        if s.over_budget > 0 {
            flags.push(format!("超预算 {} 条", s.over_budget));
        }
        if s.duplicates > 0 {
            flags.push(format!("重复 {} 条", s.duplicates));
        }
        if s.rejected > 0 {
            flags.push(format!("解析失败 {} 条", s.rejected));
        }
        if s.silent {
            flags.push("声明了产出却全程无事件".into());
        }
        out.push_str(&format!(
            "  {id:<16} {:>6} 条  收下 {:>6}  {}",
            s.events,
            s.accepted,
            if flags.is_empty() { "正常".to_string() } else { flags.join("，") }
        ));
        out.push('\n');
    }
    for w in &p.warnings {
        out.push_str(&format!("  ！{w}\n"));
    }
    out.push('\n');

    // ---- 五、这一节不能下什么结论 ----
    out.push_str("五、按本轮采集，以下结论不能下\n");
    let mut gaps = Vec::new();
    if st.audio_chunks == 0 {
        gaps.push("没有录音：任何师生言语互动、提问层次、讲授占比都不成立");
    }
    if st.utterances == 0 {
        gaps.push("没有转写：有录音但还没出文字，话语类结论要等云端转写回灌");
    }
    if st.strokes == 0 {
        gaps.push("没有笔迹：板书结构、书写流畅性、板面留存都无依据");
    }
    if st.keyframes == 0 {
        gaps.push("没有屏幕证据：希沃课件那一页讲了什么无法回溯，只能靠课件原文件");
    }
    if st.eval_records == 0 {
        gaps.push("没有评估数据：达成度、错因、分层建议一律不做");
    }
    if p.lesson.courseware.is_empty() {
        gaps.push("没有课件文件：讲到的内容只能靠板面与转写推断，无法对齐教学进度");
    }
    if gaps.is_empty() {
        out.push_str("  （无：本轮五类数据齐备）\n");
    } else {
        for g in &gaps {
            out.push_str(&format!("  · {g}\n"));
        }
    }
    out
}

fn page_summary(pages: &[String]) -> String {
    let mut seen: Vec<&String> = Vec::new();
    for p in pages {
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    match seen.len() {
        0 => "—".to_string(),
        1 => seen[0].clone(),
        n => format!("{}…(+{})", seen[0], n - 1),
    }
}

fn mmss(ms: u64) -> String {
    let t = ms / 1000;
    if t >= 3600 {
        format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
    } else {
        format!("{:02}:{:02}", t / 60, t % 60)
    }
}

fn pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        "—".to_string()
    } else {
        format!("{:.0}%", part as f64 * 100.0 / whole as f64)
    }
}

fn human(b: u64) -> String {
    if b < 1024 {
        format!("{b}B")
    } else if b < 1024 * 1024 {
        format!("{:.0}KB", b as f64 / 1024.0)
    } else {
        format!("{:.1}MB", b as f64 / 1048576.0)
    }
}

fn utc_text(utc_ms: u64) -> String {
    let days = utc_ms / 86_400_000;
    let secs = (utc_ms / 1000) % 86_400;
    // 只够用：日历换算留给需要精确日期的场合，这里只要能对齐"哪一节课"。
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}Z", secs / 3600, (secs / 60) % 60)
}

/// Howard Hinnant 的 civil_from_days 算法，避免为显示日期引入 chrono。
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
