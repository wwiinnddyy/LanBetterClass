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
                h.note = Some(format!("{e}"));
                self.handles.push(h);
                Err(h.note.unwrap_or_default())
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
