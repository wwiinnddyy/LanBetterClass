//! PNG 读写。
//!
//! 写出去的一侧：blob 必须能被观察端 `<img src>` 直接显示，所以是标准 PNG，
//! 不是自定义封装。
//!
//! 读进来的一侧才是重点：fixture 回放的图、课件导出的图都是**第三方写出来的文件**
//! （与 ci-audio 用 Python 的 `wave` 而不是自研写头同一个道理）。所以每种颜色深度
//! 都要有明确的下场——要么被转成 RGB，要么带着"为什么不能是你"被拒；
//! "看起来能读"就当没事，等于把一节课的关键帧静默换成黑屏。

use png::{BitDepth, ColorType, Compression, Decoder, Filter, Encoder};
use std::io::BufReader;
use std::path::Path;

pub struct Pixmap {
    pub width: usize,
    pub height: usize,
    /// 行优先 RGB，长度必须是 `width * height * 3`。
    pub rgb: Vec<u8>,
}

/// 写一张 RGB PNG，返回落盘字节数。
///
/// `NoFilter` + `Fast`：屏幕内容大片同色，行内预测能省的没多少，代价却是每帧都在
/// 采集进程里多烧一遍 CPU——那台机器同时在跑希沃。宁可文件大一点。
pub fn write(path: &Path, width: usize, height: usize, rgb: &[u8]) -> std::io::Result<u64> {
    if width == 0 || height == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("空图像 {width}x{height}")));
    }
    if rgb.len() != width * height * 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("像素缓冲 {} 字节，不匹配 {width}x{height}", rgb.len()),
        ));
    }
    let file = std::fs::File::create(path)?;
    let mut enc = Encoder::new(file, width as u32, height as u32);
    enc.set_color(ColorType::Rgb);
    enc.set_depth(BitDepth::Eight);
    enc.set_compression(Compression::Fast);
    enc.set_filter(Filter::NoFilter);
    let mut w = enc.write_header().map_err(io_err)?;
    w.write_image_data(rgb).map_err(io_err)?;
    w.finish().map_err(io_err)?;
    let len = std::fs::metadata(path)?.len();
    Ok(len)
}

fn io_err(e: png::EncodingError) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// 读一张 PNG 并统一成 RGB。拒绝的理由必须是可读的：现场只会看"这一节没有关键帧"，
/// 不会去猜为什么。
pub fn read(path: &Path) -> Result<Pixmap, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("打不开：{e}"))?;
    let decoder = Decoder::new(BufReader::new(file));
    let (info, mut reader) = decoder.read_info().map_err(|e| format!("不是合法 PNG：{e}"))?;
    if info.width == 0 || info.height == 0 {
        return Err("PNG 声明的尺寸是 0".into());
    }
    if info.interlaced {
        // 逐行接口读不了隔行图（每行的行宽随 Adam7 的 pass 变）。截图工具不会产出
        // 隔行 PNG，遇到就直说，别用"尽力了"糊过去。
        return Err("不支持隔行（Adam7）PNG：请让导出方存非隔行".into());
    }
    if info.bit_depth != BitDepth::Eight {
        return Err(format!("只支持 8 bit/通道，这张是 {:?}", info.bit_depth));
    }
    let (w, h) = (info.width as usize, info.height as usize);
    let palette = info.palette.clone().map(|c| c.into_owned());
    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut rows: u32 = 0;
    while let Some(row) = reader.next_row().map_err(|e| format!("解码失败：{e}"))? {
        let data = row.data();
        if data.len() % w != 0 {
            return Err(format!("行长度 {} 不能被宽度 {w} 整除", data.len()));
        }
        let bpp = data.len() / w;
        expand_row(&mut rgb, data, bpp, info.color_type, palette.as_deref())?;
        rows += 1;
    }
    if rows as usize != h {
        return Err(format!("只解出 {rows} 行，声明有 {h} 行"));
    }
    if rgb.len() != w * h * 3 {
        return Err(format!("RGB 缓冲 {} 字节，应为 {}", rgb.len(), w * h * 3));
    }
    Ok(Pixmap { width: w, height: h, rgb })
}

fn expand_row(
    out: &mut Vec<u8>,
    row: &[u8],
    bpp: usize,
    ct: ColorType,
    palette: Option<&[u8]>,
) -> Result<(), String> {
    match (ct, bpp) {
        (ColorType::Rgb, 3) => out.extend_from_slice(row),
        (ColorType::Rgba, 4) => {
            for p in row.chunks_exact(4) {
                out.extend_from_slice(&p[..3]);
            }
        }
        (ColorType::Grayscale, 1) => {
            for &g in row {
                out.extend_from_slice(&[g, g, g]);
            }
        }
        (ColorType::GrayscaleAlpha, 2) => {
            for p in row.chunks_exact(2) {
                out.extend_from_slice(&[p[0], p[0], p[0]]);
            }
        }
        (ColorType::Indexed, 1) => {
            let pal = palette.ok_or("调色板 PNG 但没带 PLTE 块")?;
            for &i in row {
                let s = (i as usize) * 3;
                if s + 2 >= pal.len() {
                    return Err(format!("调色板索引 {i} 越界（PLTE 只有 {} 项）", pal.len() / 3));
                }
                out.extend_from_slice(&pal[s..s + 3]);
            }
        }
        (c, n) => return Err(format!("颜色类型 {c:?}（{n} 字节/像素）不支持")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("a-screen-{}-{name}.png", std::process::id()));
        p
    }

    fn ramp(w: usize, h: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                v.push(((x * 37) % 256) as u8);
                v.push(((y * 53) % 256) as u8);
                v.push(((x + y) as u8).wrapping_mul(11));
            }
        }
        v
    }

    #[test]
    fn round_trip_keeps_every_byte() {
        // 自己写、自己读：至少证明这条链两端对得上（第三方 PNG 由 CI 那头负责）。
        let (w, h) = (17usize, 9usize);
        let rgb = ramp(w, h);
        let p = tmp("roundtrip");
        let n = write(&p, w, h, &rgb).expect("写 PNG 成功");
        assert!(n > 100, "这么小的图也该有几十字节头：{n}");
        let got = read(&p).expect("读回自己写的 PNG");
        assert_eq!((got.width, got.height, got.rgb.as_slice()), (w, h, rgb.as_slice()));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_file_is_actually_a_png() {
        // 万一编码器被换成别的容器，下游的 <img> 只会安静地不显示。
        let p = tmp("sig");
        write(&p, 2, 2, &ramp(2, 2)).unwrap();
        let head = std::fs::read(&p).unwrap();
        assert_eq!(&head[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn mismatched_pixel_buffer_is_refused_not_truncated() {
        // 报了引用却没文件、或者文件只有一半，比不写更难查。
        let p = tmp("bad");
        let e = write(&p, 4, 4, &ramp(3, 4)).expect_err("尺寸不符必须被拒");
        assert!(e.to_string().contains("不匹配"), "{e}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rgba_grayscale_and_indexed_all_expand_to_rgb() {
        // 逐行展开是纯函数，三种常见导出都要能变成同一份 RGB。
        let mut out = Vec::new();
        expand_row(&mut out, &[1, 2, 3, 9, 4, 5, 6, 0], 4, ColorType::Rgba, None).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6], "alpha 要被丢掉，但不能碰通道顺序");
        let mut out = Vec::new();
        expand_row(&mut out, &[7, 8], 1, ColorType::Grayscale, None).unwrap();
        assert_eq!(out, vec![7, 7, 7, 8, 8, 8]);
        let mut out = Vec::new();
        expand_row(&mut out, &[0, 1], 1, ColorType::Indexed, Some(&[255, 0, 0, 0, 255, 0])).unwrap();
        assert_eq!(out, vec![255, 0, 0, 0, 255, 0]);
        let mut out = Vec::new();
        let e = expand_row(&mut out, &[9], 1, ColorType::Indexed, Some(&[1, 2, 3])).unwrap_err();
        assert!(e.contains("越界"), "{e}");
        // 16 bit/通道在上面就被拒了，不该走到这里。
        let mut out = Vec::new();
        assert!(expand_row(&mut out, &[1, 2, 3, 4], 4, ColorType::Rgb, None).is_err());
    }
}
