//! 屏幕关键帧适配器：把一体机上的那一屏接成 `screen.keyframe` 事件流。
//!
//! 与 `a-audio` 同构，连"为什么这么分层"都一样：大块字节直接写进 `blobs/`，
//! 消息通道上只走引用；采集来源可以是真桌面，也可以是回放；两条来源共用同一套
//! 判定与落盘代码，所以下游（timeline / digest / 观察端）不必区分谁采的。
//!
//! 两条输入路径：
//! - `source: "device"`（默认）真抓屏，后端由 `params.capture` 选（auto / gdi / dxgi）；
//! - `source: "fixture"` + `fixture_dir: "某个 PNG 目录"` 按文件名次序回放。
//!   CI runner 上没有桌面，"变化检测到底有没有生效"这种判断只能靠可复现的图钉住——
//!   这与 a-audio 靠 wav 回放是同一件事，不是演示后门。
//!
//! 只在"画面真的变了"时落盘，绝不做连续视频：两道门限各拦一类误判（见 phash.rs），
//! 再加一道 `min_interval_ms` 节流——滚一条长页面时没有节流会在一秒里灌进五张
//! 几乎一样的图，track 被淹掉，"这节课讲了什么"反而看不出来。

mod capture;
mod gray;
mod phash;
mod pngio;

use capture::{Backend, Frame};
use classagent_schema::{
    kinds, Admit, Budget, Command, Envelope, Exceed, Keyframe, LessonInfo, Manifest, RestartPolicy, PROTO,
};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ID: &str = "a-screen";
/// 变化区 bbox 用的网格。与哈希网格分开：哈希答"变没变"（粗到不会被光标闪动触发），
/// bbox 答"变在哪"（细到能说清是左边那道算式还是右边那张图）。
const BBOX_COLS: usize = 16;
const BBOX_ROWS: usize = 9;
/// bbox 的单格容差：抗锯齿与压缩噪声都在这之下。
const BBOX_TOL: u8 = 8;

type Out = Arc<Mutex<io::Stdout>>;

fn main() {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let seq = Arc::new(AtomicU64::new(0));
    let lesson: Arc<Mutex<Option<LessonInfo>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));
    let cfg: Arc<Mutex<Value>> = Arc::new(Mutex::new(json!({})));

    let admit = Admit {
        proto: PROTO,
        adapter_id: ID.into(),
        version: "0.1.0".into(),
        manifest: Manifest {
            produces: vec![kinds::SESSION_OPEN.into(), kinds::SCREEN_KEYFRAME.into(), kinds::SESSION_CLOSE.into()],
            // Linux 也允许起：设备抓屏在那边会被明确拒绝（于是该源从健康表缺席），
            // 而 fixture 回放在哪个平台上都跑得动。
            platforms: vec!["windows".into(), "linux".into()],
            needs_lesson: true,
            // 事件率比录音还低。这里的字节预算指 JSON 行本身，blob 不计入——
            // 所以体积闸门在 params.screen.max_bytes_per_lesson 上，不靠它。
            budget: Budget { max_events_per_s: 10, max_bytes_per_s: 4_000, on_exceed: Exceed::Log },
            restart: RestartPolicy { max_retries: 2, backoff_ms: 2_000 },
            notes: vec![
                "需要桌面会话：无显示器 / RDP 下 gdi 可能给黑屏、dxgi 通常直接不可用；capture=auto 会退回 gdi 并把原因自述进 session.open".into(),
                "Linux 只支持 fixture 回放，设备抓屏未实现（X11 / PipeWire 是另一次改动）".into(),
                "blob 是 PNG（RGB，关掉行内预测）；默认不降采样，一节课的体积随画面变化次数与 min_interval_ms 走".into(),
                "两道门限各拦一类误判：min_dist=dHash 汉明距离，min_mad=网格平均亮度差（整页从黑变白只有后者看得见）".into(),
                "只采变化触发的关键帧，不做连续视频".into(),
            ],
        },
    };
    write_raw(&out, &serde_json::to_string(&admit).unwrap_or_else(|_| "{}".into()));

    {
        let (lesson, cfg, stop, quit) = (lesson.clone(), cfg.clone(), stop.clone(), quit.clone());
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
                    Command::StartLesson { lesson: l } => *lesson.lock().unwrap() = Some(l),
                    Command::StopLesson { lesson_id, reason } => {
                        eprintln!("[{ID}] 收课 {lesson_id}（{reason}）");
                        stop.store(true, Ordering::SeqCst);
                    }
                    Command::Stop { reason } => {
                        eprintln!("[{ID}] 退出：{reason}");
                        // 不在这里 exit：收课那一帧与 session.close 还没发完。
                        quit.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            }
            std::process::exit(0);
        });
    }

    let mut session: Option<Session> = None;
    loop {
        if let Some(l) = lesson.lock().unwrap().take() {
            if let Some(mut s) = session.take() {
                s.finish(&out, &seq);
            }
            session = Some(Session::start(&out, &seq, cfg.lock().unwrap().clone(), l));
        }
        let mut closed = false;
        if let Some(s) = session.as_mut() {
            if stop.load(Ordering::SeqCst) {
                s.request_stop();
                stop.store(false, Ordering::SeqCst);
            }
            s.pump(&out, &seq);
            closed = s.done;
        } else if quit.load(Ordering::SeqCst) {
            // 没有进行中的会话：没人在等尾帧，可以直接走。
            break;
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
        if closed {
            if let Some(mut s) = session.take() {
                s.finish(&out, &seq);
            }
        }
        if quit.load(Ordering::SeqCst) {
            // 收课命令到了：先把最后一帧与收课记录发完再退，别让下课那一刻的画面消失。
            if let Some(mut s) = session.take() {
                s.request_stop();
                s.pump(&out, &seq);
                s.finish(&out, &seq);
            }
            break;
        }
    }
    std::process::exit(0);
}

fn push(out: &Out, seq: &AtomicU64, kind: &str, t_event_ms: u64, payload: Value) {
    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let env = Envelope::new(n, kind, Some(t_event_ms), payload);
    if let Ok(text) = serde_json::to_string(&env) {
        write_raw(out, &text);
    }
}

fn write_raw(out: &Out, text: &str) {
    let mut g = out.lock().unwrap();
    let _ = g.write_all(text.as_bytes());
    let _ = g.write_all(b"\n");
    let _ = g.flush();
}

/// 一节课的采集参数。全部有默认值：声明里没写的键不是 0，是"用默认"。
#[derive(Clone, Debug)]
struct Cfg {
    capture: String,
    monitor: usize,
    poll_ms: u64,
    min_interval_ms: u64,
    min_dist: u32,
    min_mad: u32,
    max_width: usize,
    max_frames: u64,
    max_bytes: u64,
    emit_dirty: bool,
}

impl Default for Cfg {
    fn default() -> Self {
        // 默认按"尽量密"：200ms 问一次、500ms 落一帧、原尺寸、不设帧数与体积上限。
        // 弱机上把 max_width 设成 1280、poll_ms 设成 500，体积能掉一个数量级。
        Cfg {
            capture: "auto".into(),
            monitor: 0,
            poll_ms: 200,
            min_interval_ms: 500,
            min_dist: 6,
            min_mad: 4,
            max_width: 0,
            max_frames: 0,
            max_bytes: 0,
            emit_dirty: true,
        }
    }
}

impl Cfg {
    /// `params.screen.*` 是调参界面写的那一层；顶层留 capture / monitor 这些"怎么采"的开关。
    fn from_params(p: &Value) -> Cfg {
        let d = Cfg::default();
        let s = p.get("screen").cloned().unwrap_or_else(|| json!({}));
        let num = |k: &str, dflt: u64| s.get(k).and_then(|v| v.as_u64()).unwrap_or(dflt);
        Cfg {
            capture: p.get("capture").and_then(|v| v.as_str()).unwrap_or("auto").to_string(),
            monitor: p.get("monitor").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            poll_ms: num("poll_ms", d.poll_ms).max(1),
            min_interval_ms: num("min_interval_ms", d.min_interval_ms),
            // 越界一律钳而不是拒：写 999 的人要看到的是"被钳到 64"（session.open 自述里
            // 就是这个数），而不是一句报错加一整节课没有关键帧。
            min_dist: num("min_dist", d.min_dist as u64).min(phash::MAX_DIST as u64) as u32,
            min_mad: num("min_mad", d.min_mad as u64).min(255) as u32,
            max_width: num("max_width", d.max_width as u64) as usize,
            max_frames: num("max_frames_per_lesson", d.max_frames),
            max_bytes: num("max_bytes_per_lesson", d.max_bytes),
            emit_dirty: s.get("emit_dirty").and_then(|v| v.as_bool()).unwrap_or(d.emit_dirty),
        }
    }

    /// 自述用：观察端拿它和磁盘声明对照，才知道"改好了但还没生效"。
    fn json(&self) -> Value {
        json!({
            "poll_ms": self.poll_ms,
            "min_interval_ms": self.min_interval_ms,
            "min_dist": self.min_dist,
            "min_mad": self.min_mad,
            "max_width": self.max_width,
            "max_frames_per_lesson": self.max_frames,
            "max_bytes_per_lesson": self.max_bytes,
            "emit_dirty": self.emit_dirty,
            "bbox": format!("{BBOX_COLS}x{BBOX_ROWS}"),
        })
    }
}

/// 一次采样的产物：帧 + **课堂时间**。时间必须来自来源自己——回放是按虚拟时钟走的，
/// 拿墙钟的话一整节课会被压进几毫秒，节流与门限全都测不出来。
struct Sample {
    at_ms: u64,
    frame: Frame,
}

enum Next {
    /// 拿到一帧
    Got(Sample),
    /// 这一刻没有新画面（dxgi 说"还没有帧"）
    Quiet,
    /// 这一帧读坏了（fixture 里混进了解不开的文件）：跳过它，别丢一节课
    Bad,
    /// 来源耗尽（回放假结束了）
    Ended,
}

enum Source {
    Device { cap: Box<dyn Backend>, t0: Instant },
    /// 回放：按文件名排好序的 PNG、已发位置、起始墙钟、加速倍率、每帧代表的课堂毫秒
    Fixture { files: Vec<PathBuf>, pos: usize, t0: Instant, speed: f64, step_ms: u64 },
    /// 起不动（没有桌面、fixture 目录空或读不了）：只保持心跳，不产出。
    Dead,
}

impl Source {
    fn next(&mut self) -> Next {
        match self {
            Source::Device { cap, t0 } => match cap.grab() {
                Ok(Some(f)) => Next::Got(Sample { at_ms: t0.elapsed().as_millis() as u64, frame: f }),
                Ok(None) => Next::Quiet,
                Err(e) => {
                    // 后端坏了不退出进程：报一次 stderr，下一轮继续问。
                    // 真正该被看见的是 session.close 里的 stream_errors 次数。
                    eprintln!("[{ID}] 抓屏失败：{e}");
                    Next::Quiet
                }
            },
            Source::Fixture { files, pos, t0, speed, step_ms } => {
                if *pos >= files.len() {
                    return Next::Ended;
                }
                // 第 k 张图代表第 k 个 poll 时刻；不限速的话一节课在两秒内灌完，
                // 节流与预算这些行为就全都测不出来了（与 a-audio 同一条理由）。
                let at_ms = (*pos as u64) * (*step_ms);
                let due = *t0 + Duration::from_millis((at_ms as f64 / speed.max(1.0)) as u64);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep(due - now);
                }
                let path = files[*pos].clone();
                *pos += 1;
                match pngio::read(&path) {
                    Ok(px) => Next::Got(Sample { at_ms, frame: Frame { width: px.width, height: px.height, rgb: px.rgb } }),
                    Err(e) => {
                        eprintln!("[{ID}] 读不到 {}：{e}", path.display());
                        Next::Bad
                    }
                }
            }
            Source::Dead => {
                std::thread::sleep(Duration::from_millis(200));
                Next::Quiet
            }
        }
    }

    fn errors(&self) -> usize {
        match self {
            Source::Device { cap, .. } => cap.errors(),
            _ => 0,
        }
    }

    /// 回放路径自己就是时钟（`speed` 控制每帧间隔）。再让会话按 `poll_ms` 墙钟追一遍，
    /// 两个时钟会叠在一起，`speed` 就成了摆设。
    fn paced_internally(&self) -> bool {
        matches!(self, Source::Fixture { .. })
    }
}

/// 一节课的采集会话。
struct Session {
    lesson_id: String,
    blob_dir: PathBuf,
    cfg: Cfg,
    src: Source,
    started: Instant,
    /// 上一帧**已落盘**的网格与哈希。判定拿它当参照，而且被节流/被门限拦下的帧
    /// 不许更新参照：否则一次缓慢滚动会"每次都只变一点点"，一整页板书就此安静消失。
    prev_grid: Option<Vec<u8>>,
    prev_bbox: Option<Vec<u8>>,
    prev_hash: u64,
    last_emit_ms: i64,
    frames: u64,
    bytes: u64,
    polls: u64,
    unchanged: u64,
    throttled: u64,
    capped: u64,
    skipped: u64,
    failed: Option<String>,
    request_stop: bool,
    done: bool,
    closed: bool,
    opened: bool,
}

impl Session {
    fn start(out: &Out, seq: &AtomicU64, params: Value, l: LessonInfo) -> Session {
        let cfg = Cfg::from_params(&params);
        let fixture_dir = params
            .get("fixture_dir")
            .or_else(|| params.get("fixture"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let use_fixture = match params.get("source").and_then(|v| v.as_str()) {
            Some("device") => false,
            Some("fixture") => true,
            _ => fixture_dir.is_some(),
        };
        let speed = params.get("speed").and_then(|v| v.as_f64()).unwrap_or(200.0);
        let (src, mut note, failed) =
            if use_fixture { open_fixture(&cfg, fixture_dir, speed) } else { open_device(&cfg) };

        let mut s = Session {
            lesson_id: l.lesson_id.clone(),
            blob_dir: PathBuf::from(&l.blob_dir),
            cfg,
            src,
            started: Instant::now(),
            prev_grid: None,
            prev_bbox: None,
            prev_hash: 0,
            last_emit_ms: -1,
            frames: 0,
            bytes: 0,
            polls: 0,
            unchanged: 0,
            throttled: 0,
            capped: 0,
            skipped: 0,
            failed,
            request_stop: false,
            done: false,
            closed: false,
            opened: false,
        };
        if s.failed.is_some() {
            // 刻意不发 session.open：健康表按"有事件的源"建键，全程没发事件的源会直接从
            // 表里缺席。"这台机器没有可采的桌面"必须显形，不能被一条开场记录盖过去。
            eprintln!("[{ID}] 无法开始采集：{}", s.failed.as_ref().unwrap());
            s.done = true;
            return s;
        }
        if let Some(o) = note.as_object_mut() {
            o.insert("source".into(), json!(ID));
            o.insert("lesson_id".into(), json!(s.lesson_id));
            o.insert("screen".into(), s.cfg.json());
        }
        s.opened = true;
        push(out, seq, kinds::SESSION_OPEN, 0, note);
        s
    }

    fn request_stop(&mut self) {
        self.request_stop = true;
    }

    fn pump(&mut self, out: &Out, seq: &AtomicU64) {
        if self.done {
            return;
        }
        // 收课：下课那一刻屏幕上是什么，也是这节课的事实。最后再问一次，
        // 过门限就落一帧（跳过节流：这一帧没有"下一帧"会跟它撞车）。
        if self.request_stop {
            self.sample(out, seq, true);
            self.done = true;
            return;
        }
        if (self.cfg.max_frames > 0 && self.frames >= self.cfg.max_frames)
            || (self.cfg.max_bytes > 0 && self.bytes >= self.cfg.max_bytes)
        {
            // 触顶之后不再问第二次：继续抓屏只会继续拷全屏，而一字节也不会再落盘。
            // 体积上限在这里就能住手（bytes 是写完一张累加的），帧数上限也是。
            self.done = true;
            return;
        }
        let due = self.started + Duration::from_millis(self.polls.saturating_mul(self.cfg.poll_ms));
        let now = Instant::now();
        if !self.src.paced_internally() && due > now {
            // 还没到下一个 poll：睡过去。空转会把一体机唯一值钱的那点 CPU 吃光。
            std::thread::sleep(due - now);
        }
        if !self.sample(out, seq, false) {
            self.done = true;
        }
    }

    /// 问一次来源、判定、必要时落盘。返回来源是否还在（false = 回放跑完了）。
    fn sample(&mut self, out: &Out, seq: &AtomicU64, final_tick: bool) -> bool {
        let n = self.src.next();
        self.polls += 1;
        // 先问完再拆：`match n` 会把 Got 里的样本搬走，之后再读 n 就是使用已移走的值。
        let ended = matches!(n, Next::Ended);
        match n {
            Next::Got(s) => self.consider(out, seq, s, final_tick),
            // 读坏一张跳过去：一张解不开的图不该换掉一整节课的关键帧。
            Next::Bad => self.skipped += 1,
            Next::Quiet => {}
        }
        !ended
    }

    /// 核心判定：这一帧值不值得成为一节课的第 N 张关键帧。
    fn consider(&mut self, out: &Out, seq: &AtomicU64, s: Sample, final_tick: bool) {
        let (plane, pw, ph) = gray::gray_half(&s.frame.rgb, s.frame.width, s.frame.height);
        if pw == 0 || ph == 0 {
            self.skipped += 1;
            return;
        }
        let grid = gray::grid_from_plane(&plane, pw, ph, phash::GRID_COLS, phash::GRID_ROWS);
        let bbox_grid = gray::grid_from_plane(&plane, pw, ph, BBOX_COLS, BBOX_ROWS);
        let hash = phash::dhash(&grid);
        let first = self.prev_grid.is_none();
        // 差值是“与上一落盘帧比”出来的量。没有上一帧就没有这个量：拿 MAX_DIST 当占位
        // 会把开场那一帧写成“画面大改了 64 位”，而下游与回看的人都会把它当成一次真变化读。
        let (dist, mad) = match &self.prev_grid {
            Some(prev) => (
                Some(phash::hamming(self.prev_hash, hash)),
                Some(phash::mean_abs_diff(prev, &grid)),
            ),
            None => (None, None),
        };
        if !first && dist.unwrap_or_default() < self.cfg.min_dist && mad.unwrap_or_default() < self.cfg.min_mad {
            self.unchanged += 1;
            return;
        }
        if !first && !final_tick && (s.at_ms as i64) - self.last_emit_ms < self.cfg.min_interval_ms as i64 {
            self.throttled += 1;
            return;
        }
        if (self.cfg.max_frames > 0 && self.frames >= self.cfg.max_frames)
            || (self.cfg.max_bytes > 0 && self.bytes >= self.cfg.max_bytes)
        {
            // 触顶只记账不再落盘：体积闸门是给"整整一节课都在动"那种教室准备的，
            // 没有它，采集会把自己写的文件堆满那台机器。
            self.capped += 1;
            return;
        }
        let dirty = if self.cfg.emit_dirty {
            self.prev_bbox
                .as_ref()
                .and_then(|p| phash::changed_bbox(p, &bbox_grid, BBOX_COLS, BBOX_ROWS, BBOX_TOL))
                .map(|b| phash::bbox_to_px(b, BBOX_COLS, BBOX_ROWS, s.frame.width, s.frame.height))
        } else {
            None
        };
        let trigger = if first {
            "open"
        } else if final_tick {
            // 下课那一刻补采的一帧。它必须能被认出来：回看的人得知道这不是课上的一次翻页。
            "close"
        } else if dist.unwrap_or_default() >= self.cfg.min_dist {
            "phash"
        } else {
            "mass"
        };
        let Sample { at_ms, frame } = s;
        if self.emit(out, seq, at_ms, frame, trigger, dist, mad, dirty) {
            self.prev_grid = Some(grid);
            self.prev_bbox = Some(bbox_grid);
            self.prev_hash = hash;
        }
    }

    /// 一个切好的关键帧：写 blob + 报一条 screen.keyframe。返回是否真的落了盘。
    fn emit(
        &mut self,
        out: &Out,
        seq: &AtomicU64,
        at_ms: u64,
        frame: Frame,
        trigger: &str,
        dist: Option<u32>,
        mad: Option<u32>,
        dirty: Option<[u32; 4]>,
    ) -> bool {
        let (sw, sh) = (frame.width, frame.height);
        let (rgb, w, h) = gray::fit_width(frame.rgb, sw, sh, self.cfg.max_width);
        let name = format!("screen-{:012}ms-{:05}.png", at_ms, seq.load(Ordering::SeqCst) + 1);
        let len = match pngio::write(&self.blob_dir.join(&name), w, h, &rgb) {
            Ok(n) => n,
            Err(e) => {
                // 写不进去就别报引用：下游在"有这个文件"与"打不开"之间不该两难。
                eprintln!("[{ID}] 写 blob {name} 失败：{e}");
                self.failed = Some(format!("写 blob 失败：{e}"));
                self.done = true;
                return false;
            }
        };
        let mut payload = serde_json::to_value(Keyframe {
            blob: name.clone(),
            len,
            t_ms: at_ms,
            trigger: trigger.to_string(),
            matched_doc_id: None,
            matched_page_id: None,
            matched_confidence: None,
        })
        .unwrap_or_else(|_| json!({}));
        if let Some(o) = payload.as_object_mut() {
            // Keyframe 没有 deny_unknown_fields：多出来的是采集端自己算出来的证据，
            // 下游不认识也能原样存档，认识就能判"这次变化到底大不大"。
            // 开场帧没有上一帧可比，所以是 null 而不是 0——0 会被读成“完全没变”。
            o.insert("dist".into(), json!(dist));
            o.insert("mad".into(), json!(mad));
            o.insert("width".into(), json!(w));
            o.insert("height".into(), json!(h));
            o.insert("source_width".into(), json!(sw));
            o.insert("source_height".into(), json!(sh));
            o.insert("source".into(), json!(ID));
            if let Some(d) = dirty {
                o.insert("dirty".into(), json!(d));
            }
        }
        push(out, seq, kinds::SCREEN_KEYFRAME, at_ms, payload);
        self.frames += 1;
        self.bytes += len;
        self.last_emit_ms = at_ms as i64;
        let show = |v: Option<u32>| v.map(|x| x.to_string()).unwrap_or_else(|| "—".into());
        eprintln!(
            "[{ID}] 关键帧 {name}：{w}x{h}，{len} 字节（{trigger}，dist={}，mad={}）",
            show(dist),
            show(mad)
        );
        true
    }

    fn finish(&mut self, out: &Out, seq: &AtomicU64) {
        if self.closed {
            return;
        }
        self.closed = true;
        if !self.opened {
            // 没开过场的会话不收场，也不写任何事件：让"这台机器没有可采的桌面"
            // 以"这个源不在健康表里"的形式原样暴露给导出与看板。
            return;
        }
        let wall_ms = self.started.elapsed().as_millis() as u64;
        let errors = self.src.errors();
        push(
            out,
            seq,
            kinds::SESSION_CLOSE,
            wall_ms,
            json!({
                "source": ID, "lesson_id": self.lesson_id,
                // chunks / bytes / stream_errors / wall_ms / error 是收课统计认的通用字段，
                // 在这里 chunks 就是帧数。
                "chunks": self.frames,
                "bytes": self.bytes,
                "polls": self.polls,
                "unchanged": self.unchanged,
                "throttled": self.throttled,
                "capped": self.capped,
                "skipped": self.skipped,
                "stream_errors": errors,
                "wall_ms": wall_ms,
                "error": self.failed,
            }),
        );
        match &self.failed {
            Some(e) => eprintln!("[{ID}] 本节课未产出关键帧（{e}）"),
            None => eprintln!(
                "[{ID}] 收课 {}：{} 帧 / {} 字节；问 {} 次（无变化 {}、节流 {}、触顶 {}、跳过 {}、后端出错 {}）",
                self.lesson_id, self.frames, self.bytes, self.polls, self.unchanged, self.throttled,
                self.capped, self.skipped, errors
            ),
        }
    }
}

fn open_device(cfg: &Cfg) -> (Source, Value, Option<String>) {
    match capture::open(&cfg.capture, cfg.monitor) {
        Ok(o) => {
            let mut note = o.backend.describe();
            if let Some(m) = note.as_object_mut() {
                m.insert("input".into(), json!("device"));
                m.insert("wanted_capture".into(), json!(cfg.capture));
                if let Some(r) = &o.fallback {
                    m.insert("fallback".into(), json!(r));
                }
            }
            if let Some(r) = &o.fallback {
                eprintln!("[{ID}] {r}");
            }
            (Source::Device { cap: o.backend, t0: Instant::now() }, note, None)
        }
        Err(e) => (Source::Dead, json!({ "input": "device", "wanted_capture": cfg.capture }), Some(e)),
    }
}

fn open_fixture(cfg: &Cfg, dir: Option<PathBuf>, speed: f64) -> (Source, Value, Option<String>) {
    let dead = |why: String, path: String| {
        (Source::Dead, json!({ "input": "fixture", "dir": path }), Some(format!("读不到 fixture：{why}")))
    };
    let Some(dir) = dir else {
        return dead("source=fixture 但没给 params.fixture_dir".into(), String::new());
    };
    let d = dir.display().to_string();
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("png")).unwrap_or(false)
            })
            .collect(),
        Err(e) => return dead(format!("{d}：{e}"), d),
    };
    // 文件名次序就是课堂时间次序（0001、0002、…，补零宽度一致时字典序即数值序）。
    files.sort();
    if files.is_empty() {
        return dead(format!("{d} 里没有 .png"), d);
    }
    let n = files.len();
    eprintln!("[{ID}] 回放 {d}：{n} 张 PNG，poll_ms={}，{speed}x 速度", cfg.poll_ms);
    (
        Source::Fixture { files, pos: 0, t0: Instant::now(), speed: speed.max(1.0), step_ms: cfg.poll_ms },
        json!({ "input": "fixture", "dir": d, "frames": n, "speed": speed }),
        None,
    )
}
