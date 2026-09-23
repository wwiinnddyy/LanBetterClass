//! 适配器子进程监督。
//!
//! 进程外是这套架构的代价所在：多了一套 spawn / 心跳缺位 / 退避重启 / 配额强制。
//! 买到的是崩溃隔离、语言异构和"放进目录就启用"。这里就是那套代价的具体形状。

use crate::protocol::{read_line, write_line};
use classagent_schema::{resolve_argv, Admit, Command as WireCommand, Manifest, PROTO};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as OsCommand, ExitStatus, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Inbound {
    /// 适配器 stdout 上的一行。第一行必须是 Admit，其余按 Envelope 解析。
    Line { adapter_id: String, value: serde_json::Value },
    Eof { adapter_id: String },
    Err { adapter_id: String, msg: String },
    /// 控制台输入，用于现场人工驱动：start / stop / status / quit。
    Console { line: String },
}

#[derive(Debug, Clone)]
pub struct AdapterSpec {
    pub decl: classagent_schema::AdapterDecl,
    pub decl_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 已 spawn，还没收到 Admit。
    Pending,
    Ready,
    Backoff,
    Dead,
    /// 超预算被杀，不再拉起。
    Killed,
    /// 声明里 enabled=false 或平台不匹配。
    Disabled,
    /// 我们主动叫停的，正常退出。
    Stopped,
}

pub struct AdapterHandle {
    pub id: String,
    pub spec: AdapterSpec,
    pub status: Status,
    pub manifest: Option<Manifest>,
    pub restarts: u32,
    pub note: Option<String>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    not_before: Option<Instant>,
    asked_to_stop: bool,
    pub last_line_at: Option<Instant>,
}

impl AdapterHandle {
    pub fn admit(&mut self, a: &Admit) -> Result<(), String> {
        if a.proto != PROTO {
            self.status = Status::Dead;
            self.note = Some(format!("协议版本不符：适配器报 v{}，核心是 v{PROTO}", a.proto));
            return Err(self.note.clone().unwrap_or_default());
        }
        if a.adapter_id != self.id {
            self.note = Some(format!("适配器自报 id={} 与声明 id={} 不一致", a.adapter_id, self.id));
        }
        self.manifest = Some(a.manifest.clone());
        self.status = Status::Ready;
        self.note = None;
        Ok(())
    }

    fn restart_policy(&self) -> classagent_schema::RestartPolicy {
        self.manifest.as_ref().map(|m| m.restart.clone()).unwrap_or_default()
    }
}

pub struct Supervisor {
    tx: Sender<Inbound>,
    pub handles: Vec<AdapterHandle>,
    data_dir: PathBuf,
}

impl Supervisor {
    pub fn new(tx: Sender<Inbound>, data_dir: PathBuf) -> Self {
        Supervisor { tx, handles: Vec::new(), data_dir }
    }

    pub fn launch(&mut self, spec: AdapterSpec) -> Result<(), String> {
        let tx = self.tx.clone();
        let data_dir = self.data_dir.clone();
        match spawn_handle(&spec, &tx, &data_dir) {
            Ok(h) => {
                self.handles.push(h);
                Ok(())
            }
            Err(e) => {
                let mut h = placeholder(spec.clone());
                h.status = Status::Dead;
                let msg = format!("{e}");
                h.note = Some(msg.clone());
                self.handles.push(h);
                Err(msg)
            }
        }
    }

    pub fn handle(&mut self, id: &str) -> Option<&mut AdapterHandle> {
        self.handles.iter_mut().find(|h| h.id == id)
    }

    pub fn find(&self, id: &str) -> Option<&AdapterHandle> {
        self.handles.iter().find(|h| h.id == id)
    }

    pub fn ids(&self) -> Vec<String> {
        self.handles.iter().map(|h| h.id.clone()).collect()
    }

    pub fn send(&mut self, id: &str, cmd: WireCommand) -> io::Result<()> {
        let h = self.handle(id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("无适配器 {id}")))?;
        if h.asked_to_stop {
            return Ok(());
        }
        if let Some(stdin) = h.stdin.as_mut() {
            write_line(stdin, &cmd)?;
        }
        if matches!(&cmd, WireCommand::Stop { .. }) {
            h.asked_to_stop = true;
            // 必须关掉 stdin：只会阻塞在读输入上的适配器否则永远不会退出。
            if let Some(mut s) = h.stdin.take() {
                let _ = s.flush();
            }
        }
        Ok(())
    }

    pub fn manifest_of(&self, id: &str) -> Option<Manifest> {
        self.find(id).and_then(|h| h.manifest.clone())
    }

    /// 收割已退出的进程，返回本轮退出的 id。
    pub fn reap(&mut self) -> Vec<(String, Option<ExitStatus>)> {
        let now = Instant::now();
        let mut out = Vec::new();
        for h in self.handles.iter_mut() {
            let exited = match h.child.as_mut() {
                Some(c) => c.try_wait().ok().flatten(),
                None => continue,
            };
            let Some(code) = exited else { continue };
            h.child = None;
            h.stdin = None;
            let prev = h.status;
            let (next, delayed) = if h.asked_to_stop || prev == Status::Killed || prev == Status::Disabled {
                (Status::Stopped, None)
            } else {
                let policy = h.restart_policy();
                if h.restarts < policy.max_retries {
                    let delay = policy.backoff_ms.saturating_mul(1u64 << h.restarts.min(5));
                    h.not_before = Some(now + Duration::from_millis(delay));
                    (Status::Backoff, Some(delay))
                } else {
                    (Status::Dead, None)
                }
            };
            h.status = next;
            let suffix = delayed.map(|d| format!("，{d}ms 后重启")).unwrap_or_default();
            h.note = Some(format!("进程退出 {code}{suffix}"));
            out.push((h.id.clone(), Some(code)));
        }
        out
    }

    pub fn respawn_due(&mut self) -> Vec<String> {
        let now = Instant::now();
        let mut due = Vec::new();
        for (i, h) in self.handles.iter().enumerate() {
            if h.status == Status::Backoff {
                if let Some(at) = h.not_before {
                    if now >= at {
                        due.push(i);
                    }
                }
            }
        }
        let mut respawned = Vec::new();
        for i in due {
            // 崩溃重启拿的必须是磁盘上的最新声明：拿内存里那份旧 spec 的话，
            // "改完参数后源挂了再起来"会静默回到旧门限，而这正是调参时最常见的情境。
            refresh_params(&mut self.handles[i]);
            let spec = self.handles[i].spec.clone();
            let prev_restarts = self.handles[i].restarts;
            let tx = self.tx.clone();
            let data_dir = self.data_dir.clone();
            match spawn_handle(&spec, &tx, &data_dir) {
                Ok(mut h) => {
                    h.restarts = prev_restarts + 1;
                    self.handles[i] = h;
                    respawned.push(spec.decl.id.clone());
                }
                Err(e) => {
                    self.handles[i].status = Status::Dead;
                    self.handles[i].note = Some(format!("重启失败：{e}"));
                }
            }
        }
        respawned
    }

    /// 开课前把磁盘上的声明重新读进来，并下发 `Configure`。
    ///
    /// 这是"改完参数什么时候生效"的答案：下一节课。为什么不是当场：
    /// 1. 看板的 `serve` 与采集的 `run` 是两个进程，中间没有 IPC，写接口只能改文件；
    /// 2. 想立刻生效就得 kill + respawn 该源，而 `send()` 在 `asked_to_stop` 之后是 no-op、
    ///    `reap()` 对主动停的源不再重启——那条路会把正在说的那一句切成两段，
    ///    还会造出"一课多代"的 seq 空间。代价不该由一个门限数字来付。
    ///
    /// 只重读 `params`：`argv`/`cwd` 写接口本来就拒改，人工改了也应该只在下次启动客户端
    /// 时生效——开课开到一半换掉可执行文件，没人能从导出里看出采到的东西换了来路。
    /// 任何读不到的情况都保留旧值并继续：声明文件被临时锁住不该让这节课开不了。
    pub fn reload_params(&mut self) -> Vec<String> {
        let mut done = Vec::new();
        for h in self.handles.iter_mut() {
            if !refresh_params(h) {
                continue;
            }
            if h.stdin.is_none() {
                // 没管道可写（未启动/正在退出）：它的下一代在 spawn 时自己会读到新值。
                continue;
            }
            let cmd = WireCommand::Configure {
                data_dir: self.data_dir.to_string_lossy().into_owned(),
                params: h.spec.decl.params.clone(),
            };
            match write_line(h.stdin.as_mut().expect("checked above"), &cmd) {
                Ok(()) => done.push(h.id.clone()),
                Err(e) => eprintln!("[client] {e}：{} 沿用上次下发的参数", h.id),
            }
        }
        done
    }

    pub fn kill(&mut self, id: &str, why: &str) {
        if let Some(h) = self.handle(id) {
            h.status = Status::Killed;
            h.note = Some(why.to_string());
            if let Some(c) = h.child.as_mut() {
                let _ = c.kill();
            }
            h.stdin = None;
        }
    }

    /// 有序退出：先让适配器自己收尾（写最后一批分段），2 秒后强制。
    pub fn shutdown(&mut self, reason: &str) {
        let ids: Vec<String> = self.handles.iter().map(|h| h.id.clone()).collect();
        for id in &ids {
            let _ = self.send(id, WireCommand::Stop { reason: reason.to_string() });
        }
        let deadline = Instant::now() + Duration::from_millis(2_000);
        while Instant::now() < deadline {
            let alive = self.handles.iter_mut().any(|h| match h.child.as_mut() {
                Some(c) => !matches!(c.try_wait(), Ok(Some(_))),
                None => false,
            });
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for h in self.handles.iter_mut() {
            if let Some(c) = h.child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            h.child = None;
        }
    }

    pub fn status_table(&self) -> Vec<(String, Status, Option<String>, Option<Manifest>, u32)> {
        self.handles
            .iter()
            .map(|h| (h.id.clone(), h.status, h.note.clone(), h.manifest.clone(), h.restarts))
            .collect()
    }
}

/// 重读 `spec.decl_path` 里的 `params` 并写回内存。成功（不论值有没有变）返回 true。
///
/// 与 `reload_params` 共用：前者负责下发给活着的源，后者负责下一代 spawn 时的取值。
/// 读不到就保留旧值——开课/重启不该被一个临时锁住的声明文件挡住。
fn refresh_params(h: &mut AdapterHandle) -> bool {
    let path = h.spec.decl_path.clone();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[client] 重读 {} 失败（{e}），沿用内存里的参数", h.id);
            return false;
        }
    };
    let decl: classagent_schema::AdapterDecl = match serde_json::from_str(&text) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[client] {} 重新解析失败（{e}），沿用内存里的参数", h.id);
            return false;
        }
    };
    h.spec.decl.params = decl.params;
    true
}

fn placeholder(spec: AdapterSpec) -> AdapterHandle {
    AdapterHandle {
        id: spec.decl.id.clone(),
        spec,
        status: Status::Dead,
        manifest: None,
        restarts: 0,
        note: None,
        child: None,
        stdin: None,
        not_before: None,
        asked_to_stop: false,
        last_line_at: None,
    }
}

fn spawn_handle(spec: &AdapterSpec, tx: &Sender<Inbound>, data_dir: &PathBuf) -> io::Result<AdapterHandle> {
    let argv = resolve_argv(&spec.decl.argv);
    if argv.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "argv 为空"));
    }
    let mut cmd = OsCommand::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(cwd) = spec.decl.cwd.as_ref().filter(|c| !c.is_empty()) {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("启动 {} 失败（{}）：{e}", spec.decl.id, argv.join(" ")),
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("stdout 未接管道"))?;
    let stdin = child.stdin.take().ok_or_else(|| io::Error::other("stdin 未接管道"))?;

    let mut h = AdapterHandle {
        id: spec.decl.id.clone(),
        spec: spec.clone(),
        status: Status::Pending,
        manifest: None,
        restarts: 0,
        note: None,
        child: Some(child),
        stdin: Some(stdin),
        not_before: None,
        asked_to_stop: false,
        last_line_at: Some(Instant::now()),
    };
    if let Some(s) = h.stdin.as_mut() {
        write_line(s, &WireCommand::Configure { data_dir: data_dir.to_string_lossy().into_owned(), params: spec.decl.params.clone() })?;
    }

    let id = h.id.clone();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        // 连续失败上限：防止适配器狂吐坏行把核心打成热循环。
        let mut streak = 0usize;
        loop {
            match read_line::<serde_json::Value>(&mut reader) {
                Ok(Some(v)) => {
                    streak = 0;
                    if tx2.send(Inbound::Line { adapter_id: id.clone(), value: v }).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx2.send(Inbound::Eof { adapter_id: id });
                    break;
                }
                Err(e) => {
                    streak += 1;
                    if tx2.send(Inbound::Err { adapter_id: id.clone(), msg: e.to_string() }).is_err() || streak > 50 {
                        break;
                    }
                }
            }
        }
    });

    Ok(h)
}

/// 读 `adapters.d/*.adapter.json`。放进来就是启用，改名就是停用——
/// 现场运维不需要会编译任何东西。
pub fn discover(dir: &Path) -> io::Result<(Vec<AdapterSpec>, Vec<String>)> {
    let mut specs = Vec::new();
    let mut skipped = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((specs, skipped)),
        Err(e) => return Err(e),
    };
    let os = std::env::consts::OS;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if !name.ends_with(".adapter.json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                skipped.push(format!("{name}: 读不到（{e}）"));
                continue;
            }
        };
        let decl: classagent_schema::AdapterDecl = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(e) => {
                skipped.push(format!("{name}: 声明解析失败（{e}）"));
                continue;
            }
        };
        if !decl.enabled {
            skipped.push(format!("{}: enabled=false", decl.id));
            continue;
        }
        if !decl.platforms.is_empty() && !decl.platforms.iter().any(|p| p == os) {
            skipped.push(format!("{}: 本平台 {os} 未启用", decl.id));
            continue;
        }
        specs.push(AdapterSpec { decl, decl_path: path });
    }
    specs.sort_by(|a, b| a.decl.id.cmp(&b.decl.id));
    Ok((specs, skipped))
}

#[cfg(test)]
mod tests {
    use super::{placeholder, refresh_params, AdapterSpec};
    use classagent_schema::AdapterDecl;

    fn decl(text: &str) -> AdapterDecl {
        serde_json::from_str(text).expect("测试用的声明本身要是合法的")
    }

    /// “改门限 → 下节课生效”整条链的第一步：重读必须只拿 params。
    #[test]
    fn reload_picks_up_params_but_never_argv() {
        let dir = std::env::temp_dir().join(format!("ca-sup-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a-x.adapter.json");
        std::fs::write(
            &path,
        r#"{"id":"a-x","argv":["true"],"params":{"vad":{"rms_open":500}}}"#,
        )
        .unwrap();
        let mut h = placeholder(AdapterSpec { decl: decl(&std::fs::read_to_string(&path).unwrap()), decl_path: path.clone() });

        // 看板的写接口把磁盘改了，内存里还是旧值：重读要把它拿回来。
        std::fs::write(
            &path,
        r#"{"id":"a-x","argv":["true"],"params":{"vad":{"rms_open":45}}}"#,
        )
        .unwrap();
        assert!(refresh_params(&mut h), "声明可读时重读必须成功");
        assert_eq!(h.spec.decl.params["vad"]["rms_open"], serde_json::json!(45));

        // 只拿 params：argv 被人工改成本机另一个程序，不能从这条路径静默生效。
        std::fs::write(
            &path,
        r#"{"id":"a-x","argv":["calc.exe"],"params":{"vad":{"rms_open":45}}}"#,
        )
        .unwrap();
        assert!(refresh_params(&mut h));
        assert_eq!(h.spec.decl.argv, vec!["true"], "重读不能换可执行文件");

        // 声明文件坏了（改到一半 / 手工写崩）：保留旧值并告知，绝不让开课失败。
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(!refresh_params(&mut h), "读不合法 JSON 要返回 false");
        assert_eq!(h.spec.decl.params["vad"]["rms_open"], serde_json::json!(45), "坏声明不能把内存里的参数抹掉");

        std::fs::remove_dir_all(&dir).ok();
    }
}
