//! RGB 帧 → 灰度。两件事共用这一层：把画面缩放到 blob 的目标尺寸，以及把画面
//! 采样成 dHash 用的灰度网格。
//!
//! 全整数，且**每个像素都只被读一次**。这一层每个 poll 都要跑（默认 5 次/秒），
//! 而一体机上采集本身不该变成干扰；但它也不靠跳过像素换速度：被跳过的像素
//! 不是"采得慢一点"，是永远不可见。所以先做 2×2 盒式降采样（每像素读一次），
//! 再在小图上按网格取平均。
//!
//! 要分清两件事：本层保证**没有像素被跳过**；"一小笔能不能触发一帧"那是
//! 门限的事（变化越小均稀释得越多），见 phash.rs。

/// BT.601 亮度，整数版。屏幕内容用 709 还是 601 的系数，差别小到不会改变
/// 汉明距离的判定；整数乘除则决定这一层能不能在弱机上跑得起。
#[inline]
pub fn luma(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000).min(255)
}

/// 2×2 盒式降采样成半分辨率灰度图。返回的平面按 `(w/2) × (h/2)` 行优先排列，
/// 尺寸用 `half_size` 给回（奇数边长会向上取整，不丢最后一行/列）。
pub fn gray_half(rgb: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let hw = w.div_ceil(2);
    let hh = h.div_ceil(2);
    let mut out = vec![0u8; hw * hh];
    for y in 0..hh {
        let y0 = y * 2;
        for x in 0..hw {
            let x0 = x * 2;
            let mut sum = 0u32;
            let mut n = 0u32;
            for dy in 0..2 {
                let yy = y0 + dy;
                if yy >= h {
                    continue;
                }
                for dx in 0..2 {
                    let xx = x0 + dx;
                    if xx >= w {
                        continue;
                    }
                    let i = (yy * w + xx) * 3;
                    sum += luma(rgb[i], rgb[i + 1], rgb[i + 2]);
                    n += 1;
                }
            }
            out[y * hw + x] = if n == 0 { 0 } else { (sum / n) as u8 };
        }
    }
    (out, hw, hh)
}

/// 在**已经降过采样**的灰度平面上取 cols×rows 网格平均。
/// 网格尺寸由调用方决定：dHash 用 10×8，变化区 bbox 用同一份平面的另一套网格。
pub fn grid_from_plane(plane: &[u8], pw: usize, ph: usize, cols: usize, rows: usize) -> Vec<u8> {
    let mut out = vec![0u8; cols * rows];
    for cy in 0..rows {
        let y0 = cy * ph / rows;
        let y1 = ((cy + 1) * ph / rows).max(y0 + 1);
        for cx in 0..cols {
            let x0 = cx * pw / cols;
            let x1 = ((cx + 1) * pw / cols).max(x0 + 1);
            let mut sum = 0u64;
            let mut n = 0u64;
            for y in y0..y1.min(ph) {
                for x in x0..x1.min(pw) {
                    sum += plane[y * pw + x] as u64;
                    n += 1;
                }
            }
            out[cy * cols + cx] = if n == 0 { 0 } else { (sum / n) as u8 };
        }
    }
    out
}

/// 一步到位：RGB 帧 → cols×rows 灰度网格。
pub fn gray_grid(rgb: &[u8], w: usize, h: usize, cols: usize, rows: usize) -> Vec<u8> {
    if w == 0 || h == 0 || cols == 0 || rows == 0 {
        return vec![0u8; cols * rows.max(1)];
    }
    let (plane, pw, ph) = gray_half(rgb, w, h);
    grid_from_plane(&plane, pw, ph, cols, rows)
}

/// 盒式重采样整幅 RGB 帧到 `tw × th`。只在**真要落盘**时调用（不是每个 poll），
/// 所以它按质量优先：对每个目标像素取源框内的平均，而不是取最近点。
/// 取最近点在 1.5 倍缩放时会把课件的小字撕成锯齿，那些字正是下游要 OCR 的东西。
pub fn box_resize_rgb(src: &[u8], w: usize, h: usize, tw: usize, th: usize) -> Vec<u8> {
    if tw == 0 || th == 0 || w == 0 || h == 0 || tw == w && th == h {
        return src.to_vec();
    }
    let mut out = vec![0u8; tw * th * 3];
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let (mut sr, mut sg, mut sb, mut n) = (0u64, 0u64, 0u64, 0u64);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 3;
                    sr += src[i] as u64;
                    sg += src[i + 1] as u64;
                    sb += src[i + 2] as u64;
                    n += 1;
                }
            }
            let o = (ty * tw + tx) * 3;
            if n > 0 {
                out[o] = (sr / n) as u8;
                out[o + 1] = (sg / n) as u8;
                out[o + 2] = (sb / n) as u8;
            }
        }
    }
    out
}

/// 按长边等比缩到 `max_width`（0 或比当前还宽 = 原样返回）。返回 (帧, 是否缩过)。
pub fn fit_width(rgb: Vec<u8>, w: usize, h: usize, max_width: usize) -> (Vec<u8>, usize, usize) {
    if max_width == 0 || max_width >= w || w == 0 {
        return (rgb, w, h);
    }
    let nh = ((h as u64 * max_width as u64) / w as u64).max(1) as usize;
    (box_resize_rgb(&rgb, w, h, max_width, nh), max_width, nh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, c: [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            v.extend_from_slice(&c);
        }
        v
    }

    #[test]
    fn luma_is_not_pure_average() {
        // 绿比红蓝亮得多：若按 (r+g+b)/3 算，一页白底红字的板书会被压暗一档。
        assert!(luma(0, 255, 0) > luma(255, 0, 0));
        assert_eq!(luma(255, 255, 255), 255);
        assert_eq!(luma(0, 0, 0), 0);
    }

    #[test]
    fn uniform_frame_grids_to_that_value() {
        let g = gray_grid(&solid(64, 40, [12, 34, 56]), 64, 40, 10, 8);
        assert_eq!(g.len(), 80);
        let want = luma(12, 34, 56) as u8;
        assert!(g.iter().all(|&x| x == want), "纯色帧的网格必须处处相等：{g:?}");
    }

    #[test]
    fn no_source_pixel_is_skipped() {
        // 这条钉的是"不许跳行/跳列采样"：点亮任意一个源像素，半分辨率平面上
        // 必须至少有一格跟着变亮。被跳过的像素不是采得慢，是永远看不见。
        let (w, h) = (23usize, 17usize);
        for y in 0..h {
            for x in 0..w {
                let mut px = solid(w, h, [0, 0, 0]);
                let i = (y * w + x) * 3;
                px[i] = 200;
                px[i + 1] = 200;
                px[i + 2] = 200;
                let (plane, pw, ph) = gray_half(&px, w, h);
                assert_eq!((pw, ph), (12, 9));
                assert!(plane.iter().any(|&v| v > 0), "像素 ({x},{y}) 谁也没碰到：{plane:?}");
            }
        }
    }

    #[test]
    fn grid_handles_odd_sizes_without_panicking() {
        // 1919×1079 这类非偶数尺寸来自真实显示模式，不能靠补齐糊过去。
        let g = gray_grid(&solid(7, 5, [200, 100, 50]), 7, 5, 10, 8);
        assert_eq!(g.len(), 80);
        let (p, pw, ph) = gray_half(&solid(7, 5, [200, 100, 50]), 7, 5);
        assert_eq!((pw, ph), (4, 3));
        assert_eq!(p.len(), 12);
    }

    #[test]
    fn resize_keeps_a_uniform_frame_uniform() {
        let r = box_resize_rgb(&solid(60, 40, [9, 9, 9]), 60, 40, 20, 13);
        assert_eq!(r.len(), 20 * 13 * 3);
        assert!(r.chunks(3).all(|p| p == [9, 9, 9]));
    }

    #[test]
    fn fit_width_scales_the_long_edge_and_keeps_aspect() {
        let (px, w, h) = fit_width(solid(1920, 1080, [1, 2, 3]), 1920, 1080, 1280);
        assert_eq!((w, h), (1280, 720));
        assert_eq!(px.len(), w * h * 3);
        // 0 = 原尺寸；比当前还宽也是原尺寸（不许把小图放大成糊图）。
        assert_eq!(fit_width(solid(8, 8, [1, 2, 3]), 8, 8, 0).1, 8);
        assert_eq!(fit_width(solid(8, 8, [1, 2, 3]), 8, 8, 64).1, 8);
    }
}
