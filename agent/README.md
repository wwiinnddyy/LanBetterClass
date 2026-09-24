# agent · 课堂观察端

第三个组件：一个 **Tauri 桌面 App**，把原先藏在采集客户端 `classagent-client serve` 里的
本地看板**抽出来、升级成独立观察端**。它同时连**客户端**与**服务端**，做可视化配置与查看。

## 它连什么

| 目标 | 默认地址 | 用的接口 |
| --- | --- | --- |
| 采集客户端 `classagent-client serve` | `http://127.0.0.1:8786` | `/api/health`、`/api/lessons`、`/api/lesson/<id>`、`/api/lesson/<id>/digest`、`/api/lesson/<id>/stats`、`/api/lesson/<id>/blob/<name>`、`/api/adapters`、`POST /api/adapter/<file>` |
| 远程服务端 `classagent-server` | `http://127.0.0.1:8790` | `/health`、`/api/lessons`、`/api/lesson/<id>/ai-request` |

五个标签页：**课程摘要**（读客户端 digest + stats）、**录音**（话轮表 + 电平横条 + 回听）、
**关键帧**（时间轴刻度 + 帧表 + 看原图）、**数据源配置**（列 adapters.d、开关与改采集参数，
需客户端以 `serve --allow-write` 启动）、**服务端**（看已收课程与服务端生成的 AI 请求单）。

回听与看原图都不经 Rust 中转：`<audio src>` / `<img src>` 直连 `/api/lesson/<id>/blob/<name>`。
`http_get` 把响应体按文本读（`String::from_utf8_lossy`），WAV 与 PNG 字节过它一次就不是原样了；
媒体元素发的是 no-cors 请求，`csp: null` 下 WebView2 不拦。副作用：serve 不支持 Range，
拖进度条会重取整段（单段由 `max_segment_ms` 封顶，一次点一段可控）。关键帧表不预取缩略图，
就是这个道理：一节课可能几千张，看哪张取哪张。

## 设计约束

- **独立 workspace**：`src-tauri/Cargo.toml` 自带 `[workspace]`，不并入根 workspace，
  所以 `ci.yml` 的 `cargo build --workspace` 不会被 webview 重依赖拖慢。
- **零构建前端**：`ui/` 是纯静态 HTML/CSS/JS，无 Node/Vite —— 延续仓库"不装 node"的取向。
- **HTTP 走 Rust 命令**：`http_get` / `http_post` 用 `std::net` 手搓（同 `client/src/push.rs`），
  原生 socket 不受 webview CORS 限制，也不必给 serve 加 CORS 头。仅面向本机/局域网 `http://`。
  注意这两个命令只回**文本**：二进制（回听的 WAV、看原图的 PNG）必须走上面的直连路径，不能过它们。
- **改参数是“下一节课生效”**：`serve` 与 `run` 是两个进程、中间没有 IPC，写接口只能改文件；
  客户端在 `start_lesson` 重读声明并重发 `Configure`。页面上“磁盘声明”与“本节课实际生效”
  并排显示，不一致就是“改好了，等下节课”的凭据。

## 本地跑（例外：GUI 只能本机运行）

构建/校验一律走 CI（见 `.github/workflows/agent.yml`，Win+Linux 装 webview 依赖后 `cargo build`+`test`）。
产物由该工作流的 `agent-<os>` artifact 交付，**不在本地构建**：

```bash
# 列出最近一次 agent 工作流的产物名
gh run list --workflow agent --limit 1
# 取回观察端可执行文件（RUN_ID 换成上面的编号）
gh run download <RUN_ID> --name agent-windows-latest --dir .ci-artifacts
```

要在本机看界面：

```bash
# 先起两端
classagent-client serve --data try/data --adapters try/adapters.d --allow-write
classagent-server --listen 127.0.0.1:8790 --data server-data

# 再起观察端（需 tauri-cli）
cargo install tauri-cli --version "^2"
cd agent/src-tauri && cargo tauri dev
```

两端二进制取自 CI 产物（不在本地构建）：`gh run download <RUN_ID> --name bins-windows-latest --dir try`。
不想手敲：`try\体验-三端联动.bat` 一把跑完 ①→② 链路 —— 起服务端 → 采集一节课 → push →
再 push 验去重 → 起客户端 serve，两端就位后再开观察端即可。

## 只能在真机上验的几项（CI 做不到，逐项目视）

CI 能断接口、字段与字节，断不了“耳朵听得到”、“眼睛里看得见”与“拔了设备会怎样”。
以下几项改动了 `agent/ui/` 或采集链后必须跑一遍，并把结果写进 PR/提交说明：

1. **回听**：取 `agent-windows-latest` 产物→ 起 `classagent-client serve --data … --adapters … --allow-write`
   → 测试连接 → 课程摘要里选一节课 → 录音标签 → 点任意一段“回听”→ **听得到人声**，
   `#playerName` 显示的段名与表里那行一致。听不到先看 Console 里的 404/403（没开 `--allow-write` 不影响读）。
2. **看原图**：关键帧标签 → 点表里任意一行或时间轴刻度上任意一格 → `#frameView` 里出现的是
   **那一时刻的屏幕**（不是破图）。这一步 CI 只能断到“content_type 与字节一致”，真渲染只能看。
   同时确认自述那一行“后端 gdi|dxgi·显示器 N·宽×高”与这台机器的事实相符。
3. **两条抓屏后端各跑一节课**：把 `capture` 依次改成 `gdi` / `dxgi` 并各开一节课。DXGI 在锁屏 /
   高刷 / HDR 下会报 `ACCESS_LOST`，期望是**重建并继续**而不是进程挂掉，且“后端出错”格计数 > 0
   并标红。`auto` 则应在 DXGI 起不来时退到 GDI，并在自述里写明 fallback。
4. **静止不能变成一堆图**：屏幕不动跑 5 分钟 → 关键帧表应只有开场那一帧，而“判为无变化”在涨。
   这是“尽量密”的默认参数最便宜的反向护栏。
5. **调参真的下一节课生效**：把 `rms_open` 从 500 改成 45 → 保存（状态栏应写“下一节课生效”）
   → 再开一节课 → 录音标签的段数与“短促丢弃”应明显随门限变化（真麦克风实测：默认 500 在
   安静房间里切不出段，45 才能采到话轮）。“本节课实际生效”那一行应与新写的值一致。
   抓屏那一栏同理：把 `min_dist` 改到 64，下一节课应该只剩开场一帧。
6. **掉帧与重建要能被看见，且按源指名**：采集中拔麦/断设备 30 秒再插回 → 下课导出 → 摘要第四段
   出现“流错误 N 次”，并且算在**出错那个源**头上。`stream_errors` 现在是录音与抓屏共用的一个
   字段，把抓屏的后端重建写成“录音掉帧”会把排障引向完全错的一头。
