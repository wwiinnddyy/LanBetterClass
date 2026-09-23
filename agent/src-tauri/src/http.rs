//! 观察端的最小 HTTP 客户端：只用 `std::net`，走本机 `http://`。
//!
//! 与客户端 `client/src/push.rs` 同源——不引 async 运行时 / HTTP 库 / TLS 栈。生产要出公网时，
//! TLS 交给反向代理终结；这里只面向本机/局域网的 client serve 与 server。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 发一个请求，返回 (状态码, 响应体文本)。`token` 非空时带 `X-ClassAgent-Token` 头。
pub fn request(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    token: Option<&str>,
) -> Result<(u16, String), String> {
    let (host, port, path) = split_url(url)?;
    let payload = body.unwrap_or(&[]);

    let mut sock = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("连接 {host}:{port} 失败：{e}"))?;
    let to = Duration::from_secs(15);
    sock.set_read_timeout(Some(to)).map_err(|e| e.to_string())?;
    sock.set_write_timeout(Some(to)).map_err(|e| e.to_string())?;

    let token_line = token.map(|t| format!("X-ClassAgent-Token: {t}\r\n")).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: agent-observer\r\n\
         Accept: application/json, text/plain\r\n{token_line}Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        payload.len()
    );
    sock.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    if !payload.is_empty() {
        sock.write_all(payload).map_err(|e| e.to_string())?;
    }
    sock.flush().map_err(|e| e.to_string())?;

    // 先逐字节读到头部结束（\r\n\r\n），再按 Content-Length 读体。
    let mut buf: Vec<u8> = Vec::new();
    let mut b = [0u8; 1];
    loop {
        let n = sock.read(&mut b).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.push(b[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 256 * 1024 {
            return Err("响应头过大".to_string());
        }
    }

    let headers = String::from_utf8_lossy(&buf).to_string();
    let code = headers
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
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
        let n = sock.read(&mut body_bytes[read..]).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read += n;
    }
    body_bytes.truncate(read);

    Ok((code, String::from_utf8_lossy(&body_bytes).to_string()))
}

fn split_url(url: &str) -> Result<(String, u16, String), String> {
    let s = url
        .strip_prefix("http://")
        .ok_or_else(|| "只支持 http://（本机/局域网）".to_string())?;
    let (authority, rest) = match s.find('/') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, "/"),
    };
    let path = if rest.is_empty() { "/".to_string() } else { rest.to_string() };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| "端口非法".to_string())?),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return Err("URL 缺主机".to_string());
    }
    Ok((host, port, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_port_path() {
        let (h, p, path) = split_url("http://127.0.0.1:8786/api/health").unwrap();
        assert_eq!((h.as_str(), p, path.as_str()), ("127.0.0.1", 8786, "/api/health"));
        let (h2, p2, path2) = split_url("http://localhost").unwrap();
        assert_eq!((h2.as_str(), p2, path2.as_str()), ("localhost", 80, "/"));
    }

    #[test]
    fn rejects_non_http() {
        assert!(split_url("https://x/").is_err());
    }
}
