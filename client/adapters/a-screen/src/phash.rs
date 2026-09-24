//! dHash（差值感知哈希）+ 汉明距离 + 变化区 bbox。
//!
//! 为什么是 dHash 而不是 aHash：aHash 只看每格的平均亮度，整页换一张同样明暗的
//! 课件会被判成"没变"；dHash 比的是同一行里相邻两格的相对明暗，翻页必然翻掉一批
//! 符号位。为什么不上 pHash（DCT）：那要引入浮点 DCT 和一张频域模板，而这里的判据
//! 只是"这一帧值不值得落盘"。**"这两帧是不是同一页课件"是另一件事**，它属于
//! `screen.keyframe` 的 `matched_doc_id` / `matched_page_id`，要拿课件缩略图来比，
//! 不在这一层（见 AGENTS.md 里 a-screen 的描述）。
//!
//! 8×8 = 64 bit（网格要 9×8 格，因为要比"相邻两格"）。网格来自半分辨率灰度平面，
//! 所以整幅画面的每个像素都参与了一次判定——但参与不等于能触发：一格均值的
//! 抬升跟变化占的面积成正比，一条 1px 的线只能抬起几十分之一，这是感知哈希的本职（忽略小变化），
//! 不是采样漏洞。真需要"一笔也要看见"就把 `screen.min_dist` 调低。

/// 哈希位宽：每行比 8 对相邻格。
pub const HASH_COLS: usize = 8;
/// 哈希的行数。
pub const HASH_ROWS: usize = 8;
/// 算哈希需要的灰度网格宽度（比 HASH_COLS 多一列，才有"相邻"可比）。
pub const GRID_COLS: usize = HASH_COLS + 1;
/// 算哈希需要的灰度网格高度。
pub const GRID_ROWS: usize = HASH_ROWS;
/// 汉明距离上限：两帧完全相反时。超过它的阈值永远不成立，必须在写盘前被拒。
pub const MAX_DIST: u32 = (HASH_COLS * HASH_ROWS) as u32;

/// 由 GRID_COLS×GRID_ROWS 的灰度网格算 64 位 dHash。网格长度不足按"少几格算几格"
/// 处理（低位补 0），不 panic：适配器宁可少一位信息也不该在课上消失。
pub fn dhash(grid: &[u8]) -> u64 {
    let mut bits = 0u64;
    let mut i = 0usize;
    for y in 0..GRID_ROWS {
        for x in 0..HASH_COLS {
            let l = idx(y, x);
            let r = idx(y, x + 1);
            let (a, b) = (grid.get(l).copied().unwrap_or(0), grid.get(r).copied().unwrap_or(0));
            if a < b {
                bits |= 1u64 << i;
            }
            i += 1;
        }
    }
    bits
}

fn idx(y: usize, x: usize) -> usize {
    y * GRID_COLS + x
}

/// 两个哈希之间有几个符号位不同。
#[inline]
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 两帧网格的平均绝对差（0..=255）。它是 `hamming` 的补位，不是冗余：
/// dHash 只看"左格是否比右格暗"，一整块从黑变白可以一个位都不翻（相邻关系没变）——
/// 而这恰恰是"老师把白板擦干净了"。反过来单拿均差做判据也不行：光标闪一下也
/// 会抬均值，而它不该占一帧。所以两道门限各拦一类，两个数都随事件上报。
pub fn mean_abs_diff(prev: &[u8], cur: &[u8]) -> u32 {
    let n = prev.len().min(cur.len());
    if n == 0 {
        return 0;
    }
    let mut sum = 0u64;
    for i in 0..n {
        sum += prev[i].abs_diff(cur[i]) as u64;
    }
    // 长度不一致时按长的那份补 0 差值会低估，所以除以两者长度的较大值。
    let den = prev.len().max(cur.len()) as u64;
    (sum / den.max(1)) as u32
}

/// 相邻两帧的网格逐格相减，取变化格的并集作为变化区（网格坐标，左闭右开）。
/// 全在容差内返回 `None`——那意味着"确实没有任何地方变了"，与"变了但没到门限"
/// 是两件事，后者由 `hamming` 管。
///
/// 这块 bbox 是"老师点了屏幕哪里"的唯一凭据：光有整帧哈希说不出变化在左边那道
/// 算式还是右边那张图上。
pub fn changed_bbox(
    prev: &[u8],
    cur: &[u8],
    cols: usize,
    rows: usize,
    tol: u8,
) -> Option<(usize, usize, usize, usize)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (cols, rows, 0usize, 0usize);
    for y in 0..rows {
        for x in 0..cols {
            let i = y * cols + x;
            let (a, b) = (prev.get(i).copied().unwrap_or(0), cur.get(i).copied().unwrap_or(0));
            if a.abs_diff(b) > tol {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x1 == 0 || y1 == 0 {
        return None;
    }
    Some((x0, y0, x1, y1))
}

/// 把网格坐标的变化区换算成像素矩形 [x, y, w, h]。
pub fn bbox_to_px(bbox: (usize, usize, usize, usize), cols: usize, rows: usize, w: usize, h: usize) -> [u32; 4] {
    let (x0, y0, x1, y1) = bbox;
    let sx = |v: usize| (v as u64 * w as u64 / cols.max(1) as u64).min(w as u64) as u32;
    let sy = |v: usize| (v as u64 * h as u64 / rows.max(1) as u64).min(h as u64) as u32;
    let (px0, py0, px1, py1) = (sx(x0), sy(y0), sx(x1), sy(y1));
    [px0, py0, px1.saturating_sub(px0), py1.saturating_sub(py0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(v: u8) -> Vec<u8> {
        vec![v; GRID_COLS * GRID_ROWS]
    }

    #[test]
    fn identical_frames_hash_equal() {
        let (a, b) = (flat(77), flat(77));
        assert_eq!(dhash(&a), dhash(&b));
        assert_eq!(hamming(dhash(&a), dhash(&b)), 0);
    }

    #[test]
    fn rising_gradient_sets_every_bit() {
        let mut g = flat(0);
        for y in 0..GRID_ROWS {
            for x in 0..GRID_COLS {
                g[idx(y, x)] = (x * 20) as u8;
            }
        }
        // 每格都比左边亮 → 所有符号位为 1。这条钉死"位序"，位序漂了距离就白算。
        assert_eq!(dhash(&g), u64::MAX);
    }

    #[test]
    fn falling_gradient_clears_every_bit() {
        let mut g = flat(200);
        for y in 0..GRID_ROWS {
            for x in 0..GRID_COLS {
                g[idx(y, x)] = (200u16 - x as u16 * 20) as u8;
            }
        }
        assert_eq!(dhash(&g), 0);
        assert_eq!(hamming(0, u64::MAX), MAX_DIST);
    }

    #[test]
    fn a_page_turn_moves_many_bits() {
        // 明暗条纹左右互换：典型的"换了页"（整页内容重排，不是局部动一笔）。
        let mut left = flat(0);
        let mut right = flat(0);
        for y in 0..GRID_ROWS {
            for x in 0..GRID_COLS {
                let bright = if x % 2 == 0 { 240 } else { 10 };
                left[idx(y, x)] = bright;
                right[idx(y, x)] = if bright == 240 { 10 } else { 240 };
            }
        }
        let d = hamming(dhash(&left), dhash(&right));
        assert!(d >= 24, "翻页的距离不该这么小：{d}");
        assert!(d <= MAX_DIST);
    }

    #[test]
    fn bbox_finds_where_the_change_is() {
        let cols = 10;
        let rows = 8;
        let prev = vec![50u8; cols * rows];
        let mut cur = prev.clone();
        for y in 4..6 {
            for x in 7..9 {
                cur[y * cols + x] = 220;
            }
        }
        let b = changed_bbox(&prev, &cur, cols, rows, 8).expect("右下方的变化必须被框出来");
        assert_eq!(b, (7, 4, 9, 6));
        let px = bbox_to_px(b, cols, rows, 1920, 1080);
        assert!(px[0] >= 1200 && px[1] >= 400 && px[0] + px[2] <= 1920 && px[1] + px[3] <= 1080, "{px:?}");
    }

    #[test]
    fn nothing_changed_yields_no_bbox_not_a_zero_rect() {
        let g = vec![3u8; 80];
        assert!(changed_bbox(&g, &g, 10, 8, 8).is_none());
        // 容差之内的小抖动（光标余晖、抗锯齿）也不该框出东西来。
        let mut h = g.clone();
        h[0] = 3 + 6;
        assert!(changed_bbox(&g, &h, 10, 8, 8).is_none());
    }

    #[test]
    fn a_bright_bar_invisible_to_dhash_alone_still_moves_the_mad() {
        // 这条钉的是"为什么要有第二道门限"：黑屏上凭空出现半幅宽的白条，
        // 左右相邻关系没变 → dHash 距离 0；但它肯定是一帧新画面。
        let prev = vec![0u8; GRID_COLS * GRID_ROWS];
        let mut cur = prev.clone();
        for y in 0..GRID_ROWS {
            for x in 0..GRID_COLS / 2 {
                cur[idx(y, x)] = 250;
            }
        }
        assert_eq!(hamming(dhash(&prev), dhash(&cur)), 0, "白条不该翻动横向 dHash：否则前面的解释就是错的");
        assert!(mean_abs_diff(&prev, &cur) > 60, "均差必须把这页看见");
        // 而光标级的局部变化，两个判据都该安静。
        let mut blink = prev.clone();
        blink[idx(3, 4)] = 30;
        assert!(mean_abs_diff(&prev, &blink) < 4, "一个点不该被当成换页");
    }

    #[test]
    fn short_grid_does_not_panic() {
        // 网格被截断时宁可少几位，也不许在采集途中炸掉整个源。
        assert_eq!(dhash(&[9u8; 3]), dhash(&[9u8; 3]));
        assert_eq!(hamming(dhash(&[]), dhash(&[])), 0);
    }
}
