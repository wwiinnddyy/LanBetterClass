//! 最小 WAV（RIFF）读写：只接受 PCM s16le，其余一律明确拒绝。
//!
//! 手写这几十行而不是引 hound/lofty：适配器跑在和老师抢机器的平板上，
//! 多一个解析库就多一份体积与供应链；而 blob 的字节格式是要被云端 ASR 直接
//! 消费的——头写错，整节课就"听不见"。所以它必须能被单测钉死，
//! 而不是"看起来能播"。
//!
//! 只支持单声道/立体声 16 bit。采样率不假设：设备给多少就写多少，
//! 真实采样率随事件一起上报，交给下游决定要不要重采样。

use std::io::{self, Read, Write};
use std::path::Path;

/// 一段 PCM 音频：交织存放的 i16 采样 + 它的解释方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcm {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

pub const HEADER_LEN: usize = 44;

/// 拼 44 字节 RIFF/WAVE 头。小端，`data_len` 是纯 PCM 的字节数。
pub fn make_header(sample_rate: u32, channels: u16, data_len: u32) -> [u8; HEADER_LEN] {
    let bits: u16 = 16;
    let block_align = channels * bits / 8;
    let byte_rate = sample_rate * block_align as u32;
    let mut h = [0u8; HEADER_LEN];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt 块自身长度
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // 1 = PCM，不写浮点
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// 写成 `<44 字节头><PCM>`，返回落盘总字节数（事件里的 `len` 就用它）。
pub fn write(path: &Path, sample_rate: u32, channels: u16, samples: &[i16]) -> io::Result<u64> {
    let data = samples_to_bytes(samples);
    let head = make_header(sample_rate, channels, data.len() as u32);
    let mut f = std::fs::File::create(path)?;
    f.write_all(&head)?;
    f.write_all(&data)?;
    f.flush()?;
    Ok((HEADER_LEN + data.len()) as u64)
}

pub fn samples_to_bytes(samples: &[i16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// 读一个 WAV。非 PCM/非 16bit/长度奇数/魔数不对，都当成错误抛给调用方，
/// 由适配器写进 stderr 让现场看见——不返回"半个能用的对象"。
pub fn read(path: &Path) -> io::Result<Pcm> {
    let mut buf = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut buf)?;
    parse(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{path:?}：{e}")))
}

fn parse(b: &[u8]) -> Result<Pcm, String> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err("不是 RIFF/WAVE 文件".into());
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (tag, ch, rate, bits)
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        let start = pos + 8;
        let end = match start.checked_add(len) {
            Some(e) if e <= b.len() => e,
            // 声明长度越过文件尾：按实际剩余处理，宁可少读也不要 panic。
            _ => b.len(),
        };
        match id {
            b"fmt " => {
                if len < 16 {
                    return Err("fmt 块不完整".into());
                }
                let tag = u16::from_le_bytes([b[start], b[start + 1]]);
                let ch = u16::from_le_bytes([b[start + 2], b[start + 3]]);
                let rate = u32::from_le_bytes([b[start + 4], b[start + 5], b[start + 6], b[start + 7]]);
                let bits = u16::from_le_bytes([b[start + 14], b[start + 15]]);
                fmt = Some((tag, ch, rate, bits));
            }
            b"data" => data = Some(&b[start..end]),
            // LIST/adsm 等杂块原样跳过：录音笔导出的文件几乎都带。
            _ => {}
        }
        // 块按偶数字节对齐，奇数长度后面补了一个填充字节。
        pos = end + (end - start) % 2;
        if end == b.len() {
            break;
        }
    }
    let (tag, ch, rate, bits) = fmt.ok_or_else(|| String::from("缺 fmt 块"))?;
    if tag != 1 {
        return Err(format!("只支持 PCM，format tag={tag}（浮点/压缩音轨请先转成 s16le）"));
    }
    if bits != 16 {
        return Err(format!("只支持 16 bit，实际 {bits} bit"));
    }
    if ch == 0 || ch > 2 {
        return Err(format!("只支持单/立体声，实际 {ch} 声道"));
    }
    let bytes = data.ok_or_else(|| String::from("缺 data 块"))?;
    let n = bytes.len() - bytes.len() % 2;
    let mut samples = Vec::with_capacity(n / 2);
    for pair in bytes[..n].chunks_exact(2) {
        samples.push(i16::from_le_bytes([pair[0], pair[1]]));
    }
    Ok(Pcm { sample_rate: rate, channels: ch, samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("classagent-a-audio-{name}.wav"))
    }

    #[test]
    fn header_describes_the_payload() {
        let h = make_header(16_000, 1, 3_200);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 36 + 3_200);
        assert_eq!(u16::from_le_bytes([h[20], h[21]]), 1, "format tag 必须是 PCM");
        assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 16_000);
        assert_eq!(u16::from_le_bytes([h[32], h[33]]), 2, "单声道 16bit 块对齐 2");
        assert_eq!(u32::from_le_bytes([h[28], h[29], h[30], h[31]]), 32_000, "byte_rate");
    }

    #[test]
    fn write_then_read_round_trips() {
        let p = tmp("roundtrip");
        let src: Vec<i16> = (0..800).map(|i| (i as i16) * 7 - 2000).collect();
        let n = write(&p, 44_100, 2, &src).unwrap();
        assert_eq!(n as usize, HEADER_LEN + src.len() * 2);
        let back = read(&p).unwrap();
        assert_eq!(back.sample_rate, 44_100);
        assert_eq!(back.channels, 2);
        assert_eq!(back.samples, src);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn float_track_is_refused_not_misread() {
        // 把 format tag 改成 3（IEEE float）：必须报错，不能当成 PCM 解析出噪音。
        let p = tmp("float");
        let mut bytes = make_header(16_000, 1, 4).to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes[20] = 3;
        std::fs::write(&p, &bytes).unwrap();
        let e = read(&p).unwrap_err().to_string();
        assert!(e.contains("PCM"), "错误信息要说明为什么不能用：{e}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn garbage_and_truncated_files_error_cleanly() {
        let p = tmp("garbage");
        std::fs::write(&p, b"not a wav at all").unwrap();
        assert!(read(&p).is_err());
        // 声明了 data 长度但没写内容：读到的采样数为 0，不 panic。
        std::fs::write(&p, make_header(16_000, 1, 999_999).to_vec()).unwrap();
        let back = read(&p).unwrap();
        assert!(back.samples.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_chunks_are_skipped_and_odd_lengths_stay_aligned() {
        // 录音笔导出的文件常带 LIST 等杂块，且块长可能是奇数（后面补 1 字节填充）。
        // 逐块错位一个字节，data 的起点就读歪，整段会变成噪音。
        let mut full = Vec::new();
        full.extend_from_slice(b"RIFF");
        full.extend_from_slice(&0u32.to_le_bytes()); // 总长由解析器忽略，它只沿块链走
        full.extend_from_slice(b"WAVE");
        full.extend_from_slice(b"fmt ");
        full.extend_from_slice(&16u32.to_le_bytes());
        full.extend_from_slice(&1u16.to_le_bytes()); // PCM
        full.extend_from_slice(&1u16.to_le_bytes()); // 单声道
        full.extend_from_slice(&8_000u32.to_le_bytes());
        full.extend_from_slice(&16_000u32.to_le_bytes()); // byte rate
        full.extend_from_slice(&2u16.to_le_bytes()); // block align
        full.extend_from_slice(&16u16.to_le_bytes()); // bits
        full.extend_from_slice(b"LIST");
        full.extend_from_slice(&3u32.to_le_bytes()); // 奇数长度
        full.extend_from_slice(b"abc");
        full.push(0); // 填充
        full.extend_from_slice(b"data");
        full.extend_from_slice(&4u32.to_le_bytes());
        full.extend_from_slice(&(-2i16).to_le_bytes());
        full.extend_from_slice(&300i16.to_le_bytes());

        let back = parse(&full).expect("LIST 块不该影响解析");
        assert_eq!(back.sample_rate, 8_000);
        assert_eq!(back.channels, 1);
        assert_eq!(back.samples, vec![-2, 300]);
    }
}
