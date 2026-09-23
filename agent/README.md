# agent · 课堂观察端

第三个组件：一个 **Tauri 桌面 App**，把原先藏在采集客户端 `classagent-core serve` 里的
本地看板**抽出来、升级成独立观察端**。它同时连**客户端**与**服务端**，做可视化配置与查看。

## 它连什么

| 目标 | 默认地址 | 用的接口 |
| --- | --- | --- |
| 采集客户端 `classagent-core serve` | `http://127.0.0.1:8786` | `/api/health`、`/api/lessons`、`/api/lesson/<id>/digest`、`/api/lesson/<id>/stats`、`/api/adapters`、`POST /api/adapter/<file>` |
| 远程服务端 `classagent-server` | `http://127.0.0.1:8790` | `/health`、`/api/lessons`、`/api/lesson/<id>/ai-request` |

三个标签页：**课程摘要**（读客户端 digest + stats）、**数据源配置**（列 adapters.d 并开关，
需客户端以 `serve --allow-write` 启动）、**服务端**（看已收课程与服务端生成的 AI 请求单）。

## 设计约束

- **独立 workspace**：`src-tauri/Cargo.toml` 自带 `[workspace]`，不并入根 workspace，
  所以 `ci.yml` 的 `cargo build --workspace` 不会被 webview 重依赖拖慢。
- **零构建前端**：`ui/` 是纯静态 HTML/CSS/JS，无 Node/Vite —— 延续仓库"不装 node"的取向。
- **HTTP 走 Rust 命令**：`http_get` / `http_post` 用 `std::net` 手搓（同 `core/push.rs`），
  原生 socket 不受 webview CORS 限制，也不必给 serve 加 CORS 头。仅面向本机/局域网 `http://`。

## 本地跑（例外：GUI 只能本机运行）

构建/校验一律走 CI（见 `.github/workflows/agent.yml`，Win+Linux 装 webview 依赖后 `cargo build`+`test`）。
要在本机看界面：

```bash
# 先起两端
classagent-core serve --data try/data --adapters try/adapters.d --allow-write
classagent-server --listen 127.0.0.1:8790 --data server-data

# 再起观察端（需 tauri-cli）
cargo install tauri-cli --version "^2"
cd agent/src-tauri && cargo tauri dev
```
