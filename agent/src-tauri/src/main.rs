#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! agent · 课堂观察端。把原先藏在客户端 `classagent-client serve` 里的本地看板抽成独立桌面 App：
//! 连接采集客户端（client）与远程服务端（server），做可视化配置与查看。
//!
//! 所有 HTTP 走这里的 Rust 命令（`http_get` / `http_post`），前端 `invoke` 调用：
//! 原生 socket 请求不受 webview 同源/CORS 限制，也不必给 serve 加 CORS 头。

mod http;

#[tauri::command]
fn http_get(url: String) -> Result<String, String> {
    let (code, body) = http::request("GET", &url, None, None)?;
    if (200..=299).contains(&code) {
        Ok(body)
    } else {
        Err(format!("HTTP {code}: {body}"))
    }
}

#[tauri::command]
fn http_post(url: String, body: String, token: Option<String>) -> Result<String, String> {
    let (code, resp) = http::request("POST", &url, Some(body.as_bytes()), token.as_deref())?;
    if (200..=299).contains(&code) {
        Ok(resp)
    } else {
        Err(format!("HTTP {code}: {resp}"))
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![http_get, http_post])
        .run(tauri::generate_context!())
        .expect("运行 agent 观察端失败");
}
