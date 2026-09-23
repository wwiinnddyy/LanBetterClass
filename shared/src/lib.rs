//! 采集核心与数据源适配器之间的唯一契约。
//!
//! 最重要的一条设计决定：`Envelope::kind` 是**开放字符串**，不是枚举。
//! 一旦核心枚举了数据源类型，新增一个学校环境就得重编译核心，
//! "随时添加、随时关闭适配器"就不再成立。这里只提供 `kinds` 常量作为命名约定。
//!
//! 线协议：NDJSON over stdio。一行一条 JSON，`stdin` 收 `Command`，`stdout` 吐
//! `Admit`（第一行）与 `Envelope`。大块二进制不进消息通道，由适配器直接写进
//! `LessonInfo::blob_dir`，事件里只放文件名引用。

use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 协议版本。适配器在 `Admit::proto` 里回报它实现的版本，核心据此拒绝。
pub const PROTO: u8 = 1;

pub mod kinds {
    /// 核心自己写进日志的适配器自述记录，用于事后知道"这节课到底装了哪些源"。
    pub const CORE_ADMIT: &str = "core.admit";
    /// 适配器崩溃后被重新拉起。跨过这条记录的 seq 空间是新的，不能当成重复事件。
    pub const CORE_RESPAWN: &str = "core.respawn";
    pub const SESSION_OPEN: &str = "session.open";
    pub const SESSION_CLOSE: &str = "session.close";
    pub const INK_PAGE_ACTIVATE: &str = "ink.page_activate";
    pub const INK_STROKE_COMMIT: &str = "ink.stroke_commit";
    pub const INK_STROKE_DELETE: &str = "ink.stroke_delete";
    pub const AUDIO_CHUNK: &str = "audio.chunk";
    pub const ASR_UTTERANCE: &str = "asr.utterance";
    pub const SCREEN_KEYFRAME: &str = "screen.keyframe";
    pub const COURSEWARE_PAGE: &str = "courseware.page";
    pub const EVAL_RECORD: &str = "eval.record";
}

/// 进程启动至今的微秒数，用作单调时钟。
/// 与墙钟分开记：系统时钟一次跳校就能把整节课的时间轴搅乱。
pub fn mono_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let s = START.get_or_init(Instant::now);
    s.elapsed().as_micros() as u64
}

pub fn utc_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 适配器 -> 核心的一条事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub proto: u8,
    /// 该适配器内从 1 开始单调递增。核心用它检测丢失，也用它做重投递幂等。
    pub seq: u64,
    pub kind: String,
    /// 适配器自己的单调时钟。同源事件之间的相对顺序以它为准。
    pub t_mono_us: u64,
    /// 该事件在课堂本地时间轴上的起点（毫秒，0 = 上课铃）。
    /// 可选，但缺了就没有跨源对齐：笔迹和录音只能各排各的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_event_ms: Option<u64>,
    /// 可选：适配器自认的墙钟。核心不信任它，只原样存档。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_utc_ms: Option<u64>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl Envelope {
    pub fn new(seq: u64, kind: impl Into<String>, t_event_ms: Option<u64>, payload: serde_json::Value) -> Self {
        Envelope {
            proto: PROTO,
            seq,
            kind: kind.into(),
            t_mono_us: mono_us(),
            t_event_ms,
            t_utc_ms: Some(utc_ms()),
            payload,
        }
    }
}

/// 适配器进程起来后必须先在 stdout 写的第一个对象。核心据此完成能力协商。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Admit {
    pub proto: u8,
    pub adapter_id: String,
    pub version: String,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    /// 本适配器会产出哪些 kind。核心不校验封闭性，只用它做健康检查：
    /// 一节课结束后，声明了却没产出任何事件的源会在 `sources` 里标成 silent。
    #[serde(default)]
    pub produces: Vec<String>,
    /// 允许运行的平台，取值为 `std::env::consts::OS`（"windows" / "linux"）。
    #[serde(default)]
    pub platforms: Vec<String>,
    /// false 表示该源不需要"上课/下课"信号，进程起来就持续采（如系统清单）。
    #[serde(default)]
    pub needs_lesson: bool,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// 人类可读的前置条件，例如"需要教师领夹麦"。核心只在 status 里回显，不做逻辑。
    #[serde(default)]
    pub notes: Vec<String>,
}

/// 超预算的处理不是"为了安全好看"，是因为采集端和白板抢同一台弱机器：
/// 没有强制上限，一个写崩了的适配器就能把整节课变成白板的掉帧。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Exceed {
    /// 只记账，不动作。
    Log,
    /// 记录并标记该条事件为 over_budget，但不丢数据。
    Degrade,
    /// 直接杀掉该适配器并停止重启（防止反复拉起把 CPU 打满）。
    Kill,
}

fn default_exceed() -> Exceed {
    Exceed::Degrade
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub max_events_per_s: u32,
    pub max_bytes_per_s: u64,
    #[serde(default = "default_exceed")]
    pub on_exceed: Exceed,
}

impl Default for Budget {
    fn default() -> Self {
        Budget { max_events_per_s: 2_000, max_bytes_per_s: 8_000_000, on_exceed: Exceed::Degrade }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPolicy {
    pub max_retries: u32,
    pub backoff_ms: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        RestartPolicy { max_retries: 3, backoff_ms: 1_000 }
    }
}

/// 核心 -> 适配器。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// 进程一起来就发。`data_dir` 是采集根的绝对路径。
    Configure { data_dir: String, params: serde_json::Value },
    StartLesson { lesson: LessonInfo },
    /// 课结束了。收尾算在这条上：适配器要把攒着的尾段（未闭合的话轮、还没落的 blob）
    /// 连同 `session.close` 发完——核心会等一段静默再接关课，所以“最后一句”来得及落盘。
    /// 不要把收尾推到 `Stop` 上：那时课已经关了，事件只能掉进 misc.ndjson，等于记在课外面。
    StopLesson { lesson_id: String, reason: String },
    /// 要求适配器在 2 秒内自己退出；超时由核心 kill。
    Stop { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonInfo {
    pub lesson_id: String,
    /// 上一节课的 id。"衔接上节课笔记"靠这条显式链，不靠语义相似度检索。
    pub prev_lesson_id: Option<String>,
    pub subject: Option<String>,
    pub class: Option<String>,
    pub teacher: Option<String>,
    pub started_at_utc_ms: u64,
    /// 课件原文件的结构化描述。拿到课件就不需要对屏幕做 OCR 取主干。
    pub courseware: Vec<CoursewareRef>,
    /// 适配器把音频分段、关键帧等大块写进这里，事件里只报文件名。
    pub blob_dir: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoursewareRef {
    pub doc_id: String,
    pub path: String,
    /// "pptx" / "pdf" / "enbx" / ...
    pub format: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

// ---------------------------------------------------------------------------
// 各 kind 的 payload。类型是建议性的：核心一律按 Value 存，
// 因此不认识的新 kind 也能原样落盘，等下游分析侧再解释。
// ---------------------------------------------------------------------------

/// 一笔写完的墨迹。`points` 里第三个分量是压感，第四个是相对落笔的毫秒。
/// 没有第四个分量，"老师说这句话时黑板上是什么"就永远做不出来。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrokeCommit {
    pub stroke_id: String,
    /// 跨节课稳定的页 id。数组下标不合格：插删一页就会全线错位。
    pub page_id: String,
    pub tool: String,
    pub color: String,
    pub width: f64,
    #[serde(default)]
    pub layer: Option<String>,
    pub duration_ms: u64,
    /// [x, y, pressure, dt_ms]；x/y 是画布逻辑坐标，不含视口变换。
    pub points: Vec<[f64; 4]>,
    #[serde(default)]
    pub bbox: Option<[f64; 4]>,
    /// 白板侧的抽稀策略自述。不知道它丢了多少点，就没法解读书写流畅性。
    #[serde(default)]
    pub decimation: Option<String>,
    /// 该页此刻的视口变换 [scale, tx, ty]，用于和屏幕关键帧对齐。
    #[serde(default)]
    pub viewport: Option<[f64; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrokeDelete {
    pub stroke_id: String,
    pub page_id: String,
    /// write / erase / undo / redo。只给最终态会把学生从没见过的内容写进笔记。
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageActivate {
    pub page_id: String,
    pub index: u32,
    #[serde(default)]
    pub doc_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Utterance {
    pub t0_ms: u64,
    pub t1_ms: u64,
    /// "teacher" / "student" / "unknown" / diarization 给出的簇标签。
    pub speaker: String,
    pub text: String,
    #[serde(default)]
    pub confidence: Option<f64>,
    /// 词级时间戳 [[word, t0_ms, t1_ms], ...]。FIAC 的 3 秒采样要靠它。
    #[serde(default)]
    pub words: Vec<[String; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioChunk {
    pub blob: String,
    pub len: u64,
    /// "opus" / "pcm_s16le" / "wav"
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// 该分段在课堂时间轴上的起点。
    pub t0_ms: u64,
    pub dur_ms: u64,
    #[serde(default)]
    pub silent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframe {
    pub blob: String,
    pub len: u64,
    pub t_ms: u64,
    /// "ink_start" / "page_change" / "dirty_rect" / "timer"。
    /// 只接受事件触发的抓帧；定时抓帧会把希沃那一路变成 GPU 负担。
    pub trigger: String,
    /// 与课件缩略图 pHash 匹配出来的页。匹配上就不需要 OCR。
    #[serde(default)]
    pub matched_doc_id: Option<String>,
    #[serde(default)]
    pub matched_page_id: Option<String>,
    #[serde(default)]
    pub matched_confidence: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRecord {
    pub instrument: String,
    pub t_ms: u64,
    pub answers: serde_json::Value,
}

// ---------------------------------------------------------------------------
// 落盘格式
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Accepted,
    /// 与同一适配器上一条之间存在 seq 空洞，`gap` 给出丢了多少条。
    GapBefore,
    OverBudget,
    /// seq 不比上一条大：同一事件被重复投递。保留下来但不参与对齐。
    Duplicate,
    /// 协议版本不支持或载荷解析失败，原样保存在 `raw` 里。
    Rejected,
}

/// `lessons/<id>/events.ndjson` 的一行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRecord {
    /// 全局序号，跨适配器唯一，用于外部消费者断点续读。
    pub global_seq: u64,
    pub adapter_id: String,
    pub t_core_utc_ms: u64,
    pub t_core_mono_us: u64,
    pub status: RecordStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<u64>,
    pub envelope: Envelope,
    /// 解析失败时保留原始行，避免"适配器写错一个字段"变成静默丢数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonMeta {
    pub info: LessonInfo,
    /// 上课那一刻核心的单调时钟，用来把 `t_core_mono_us` 换算成课堂相对毫秒。
    pub started_core_mono_us: u64,
    #[serde(default)]
    pub ended_core_mono_us: Option<u64>,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

/// `adapters.d/*.adapter.json`：装载声明，与 `Manifest`（适配器自述）是两件事。
/// 前者是"这台环境装了什么"，后者是"这个程序会说些什么"。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterDecl {
    pub id: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 额外限制，与 Manifest::platforms 取交集。
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub params: serde_json::Value,
}

fn default_true() -> bool {
    true
}

/// 把声明里的 `$TARGET_DIR` 展开，并在 Windows 上补齐 `.exe`。
/// 不这么做的代价是每个新环境都要现场改配置文件。
pub fn resolve_argv(argv: &[String]) -> Vec<String> {
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target/debug".to_string());
    argv.iter()
        .enumerate()
        .map(|(i, a)| {
            let mut s = a.replace("$TARGET_DIR", &target);
            if i == 0 && cfg!(windows) && !s.ends_with(".exe") && std::path::Path::new(&s).extension().is_none() {
                s.push_str(".exe");
            }
            s
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 客户端 → 服务端：一节课的上传契约
//
// 与采集侧的 Envelope 分开，互不枚举——服务端只认"能喂 AI 的载荷"，
// 原始 events/blobs 仍留在采集端本机。这条边界就是"客户端/服务端分离"的接缝。
// ---------------------------------------------------------------------------

/// 客户端把一节课折叠后的 AI 载荷（`timeline::build` 的产物）推给远程服务端。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LessonUpload {
    pub proto: u8,
    pub lesson_id: String,
    pub uploaded_at_utc_ms: u64,
    /// 采集端自述，例如 "classagent-client 0.1.0"。
    pub source: String,
    /// `AiPayload` 的 JSON 形态。服务端不重解析其内部结构，只透传与落盘。
    pub ai_payload: serde_json::Value,
}

/// 服务端对一次上传的确认。`deduped` 让客户端知道这是重投（幂等，不重复落盘）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestAck {
    pub ok: bool,
    pub proto: u8,
    pub lesson_id: String,
    /// 本次是否真的写了盘（新内容或有变化）。
    pub stored: bool,
    /// 内容与已存副本逐字节相同 → 幂等重投。
    pub deduped: bool,
    pub received_at_utc_ms: u64,
    /// 生成的"交给 AI"请求单路径（服务端到此为止，真正的模型调用在下游）。
    pub ai_request: String,
}

/// 路径片段白名单：lesson id / 文件名过它，禁 `/` `\` `..` NUL 与绝对路径。
/// 采集端本地看板与远程服务端共用同一把尺子。
pub fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 128
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}
