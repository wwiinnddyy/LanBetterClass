//! NDJSON 帧。
//!
//! 选它而不是长度前缀二进制，是因为这条总线上的事件率很低（笔 100-250 点/秒、
//! 音频编码后约 3KB/秒），换协议省不下可测的开销，却能换来两件实打实的事：
//! 现场排障时能直接 `cat` 采集日志，以及任何语言只要能打印一行就能当适配器。

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{self, BufRead, Write};

/// 读一行并解析。返回 `Ok(None)` 表示 EOF。空行跳过。
///
/// 解析失败当作 `Err` 抛给调用方，由调用方决定保留原文还是丢弃——
/// 不能在这里静默跳过，那会把"适配器写错一个字段"变成无声的数据丢失。
pub fn read_line<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<Option<T>> {
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let s = line.trim_end_matches(['\n', '\r']);
        if s.trim().is_empty() {
            continue;
        }
        return match serde_json::from_str::<T>(s) {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{e}; line={}", shorten(s, 200)),
            )),
        };
    }
}

/// 写一行并立刻 flush。这里不缓冲：跨进程消息通道上憋 4KB 才发，
/// 换来的吞吐对这个事件率毫无意义，代价却是下课瞬间丢一批未刷出的事件。
pub fn write_line<W: Write, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *w, v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    w.write_all(b"\n")?;
    w.flush()
}

/// 按字符截断，避免在 UTF-8 边界上切片 panic。
pub fn shorten(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}
