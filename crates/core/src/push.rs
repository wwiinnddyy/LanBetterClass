//! 客户端 → 服务端：把一节课的 `ai_payload` 通过 HTTP POST 推给远程服务端。
//!
//! 手搓最小 HTTP/1.1 客户端（只用 `std::net::TcpStream`），**不引入 async 运行时、
//! HTTP 库或 TLS 栈**：tiny_http 0.12 只有服务端没有客户端，而这台机器的依赖树越小
//! 越好。生产环境的 TLS 交给反向代理终结——与选 tiny_http 的取舍同源。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 向 `addr`（HOST:PORT）的 `path` 发一个 JSON POST，返回 (状态码, 响应体文本)。
/// `token` 非空时带 `X-ClassAgent-Token` 头。
pub fn post_json(addr: &str, path: &str, body: &[u8], token: Option<&str>) -> std::io::Result<(u16, String)> {
    let mut sock = TcpStream::connect(addr)?;
    sock.set_read_timeout(Some(Duration::from_secs(15)))?;
    sock.set_write_timeout(Some(Duration::from_secs(15)))?;

    let token_line = match token {
        Some(t) => format!("X-ClassAgent-Token: {t}\r\n"),
        None => String::new(),
    };
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: classagent-core\r\n\
         Content-Type: application/json\r\n{token_line}Content-Length: {}\r\n\
         Connection: close\r\nAccept: application/json\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes())?;
    sock.write_all(body)?;
    sock.flush()?;

    // 先逐字节读到头部结束（\r\n\r\n），响应头很小，够简单也够稳。
    let mut buf: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = sock.read(&mut byte)?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::other("响应头过大"));
        }
    }

    let headers = String::from_utf8_lossy(&buf).to_string();
    let status_line = headers.lines().next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    let content_len = headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    let mut body_bytes = vec![0u8; content_len];
    let mut read = 0usize;
    while read < content_len {
        let n = sock.read(&mut body_bytes[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    body_bytes.truncate(read);

    Ok((code, String::from_utf8_lossy(&body_bytes).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 离线可断言：连一个不存在的端口应返回连接错误而不是 panic。
    #[test]
    fn connect_to_closed_port_errors() {
        let r = post_json("127.0.0.1:1", "/api/ingest", b"{}", None);
        assert!(r.is_err(), "连不存在的端口应报错");
    }
}
