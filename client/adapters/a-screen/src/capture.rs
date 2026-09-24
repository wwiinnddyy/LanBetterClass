//! 抓屏后端。Windows 上两条路，用 `params.capture` 选：
//!
//! - `gdi`  —— `GetDC` + `BitBlt` + `GetDIBits`。哪儿都能跑：RDP 远程协助、虚机、
//!   基础显示驱动下都有画面，代价是只能轮询（每 poll 拷一次全屏，没人告诉你"变了"）。
//! - `dxgi` —— `IDXGIOutputDuplication` 桌面复制。合成器直接交出帧，没有新帧时
//!   `AcquireNextFrame` 立刻超时返回，几乎不花钱；但 RDP 会话与一些虚机里拿不到
//!   复制接口，分辨率或模式一切换还会 `ACCESS_LOST`，必须重建。
//! - `auto` —— 先试 DXGI，拿不到就退回 GDI，**并把"用的哪条 + 为什么退"写进 session.open**。
//!   课后看回放时"这一节的帧是怎么来的"必须是可见的事实，不是猜测。
//!
//! 两条路的产物是同一种 RGB 帧，所以上层（变化检测、落盘、事件）完全不区分来源——
//! 与 a-audio 的设备/回放路径同一个道理。
//!
//! 非 Windows 一律明确拒绝：那个源在这台机器上就是不存在，让它在健康表里缺席，
//! 而不是假装采到一张黑屏。X11 / PipeWire 是另一次改动。

pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// 行优先 RGB，长度 `width * height * 3`。
    pub rgb: Vec<u8>,
}

pub trait Backend {
    /// 抓一帧。`Ok(None)` 只表达一件事：这一刻没有新画面（只有 DXGI 会说这句话）。
    fn grab(&mut self) -> Result<Option<Frame>, String>;
    /// 进 `session.open` 的自述：用了哪条路、哪台显示器、多大、重建过几次。
    fn describe(&self) -> serde_json::Value;
    /// 后端自己出过几次事（GDI 调用失败、DXGI ACCESS_LOST 重建）。
    /// 它必须进数据而不是只进 stderr：如果一节课的帧全是在重建途中抢来的，
    /// "老师什么时候翻的页"就不可信，课后复盘得能看到这个前提。
    fn errors(&self) -> usize;
}

pub struct Opened {
    pub backend: Box<dyn Backend>,
    /// `auto` 退到 GDI 时，DXGI 那句失败原因。丢掉它就等于把兼容性妥协藏起来。
    pub fallback: Option<String>,
}

/// 观察端那个下拉框必须和这份清单一致（`.github/workflows/agent.yml` 有文本守卫钉它）。
pub const BACKENDS: [&str; 3] = ["auto", "gdi", "dxgi"];

#[cfg(not(windows))]
pub fn open(_want: &str, _monitor: usize) -> Result<Opened, String> {
    Err("本机不是 Windows：抓屏后端只有 gdi / dxgi 两条，X11 / PipeWire 尚未实现".into())
}

#[cfg(windows)]
pub fn open(want: &str, monitor: usize) -> Result<Opened, String> {
    match want {
        "gdi" => return gdi::opened(monitor).map(|b| Opened { backend: b, fallback: None }),
        "dxgi" => return dxgi::opened(monitor).map(|b| Opened { backend: b, fallback: None }),
        "auto" => {}
        other => {
            // 未知取值不猜：写错的人要立刻知道，而不是"用另一条路安安静静采了一整节课"。
            return Err(format!("params.capture 只认 auto / gdi / dxgi，收到 {other:?}"));
        }
    }
    match dxgi::opened(monitor) {
        Ok(b) => Ok(Opened { backend: b, fallback: None }),
        Err(e) => match gdi::opened(monitor) {
            Ok(b) => Ok(Opened { backend: b, fallback: Some(format!("DXGI 不可用，退回 GDI：{e}")) }),
            Err(e2) => Err(format!("两条路都不通：dxgi={e}；gdi={e2}")),
        },
    }
}

/// DIB / 纹理的行都是 BGRA（小端 + BI_RGB 32bit 或 B8G8R8A8），转成 RGB 顺手丢掉 alpha。
/// 通道顺序反了不会报错，只会让一整节课的红蓝互换——而没人回看时这件事完全无声。
pub fn bgra_to_rgb(src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; src.len() / 4 * 3];
    let mut j = 0usize;
    for p in src.chunks_exact(4) {
        out[j] = p[2];
        out[j + 1] = p[1];
        out[j + 2] = p[0];
        j += 3;
    }
    out
}

#[cfg(windows)]
mod gdi {
    use super::{bgra_to_rgb, Backend, Frame};
    use serde_json::json;
    use std::ffi::c_void;
    use windows::core::BOOL;
    // LPARAM 不在 windows::core 里：GDI 那边的回调签名把它放在 Win32::Foundation。
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
        GetDIBits, GetMonitorInfoW, HGDIOBJ, HDC, HMONITOR, MONITORENUMPROC, MONITORINFO, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

    #[derive(Clone, Copy)]
    struct Rect {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    }

    pub struct Gdi {
        r: Rect,
        monitor: usize,
        primary: bool,
        dpi_aware: bool,
        errs: usize,
    }

    pub fn opened(monitor: usize) -> Result<Box<dyn Backend>, String> {
        Gdi::open(monitor).map(|g| Box::new(g) as Box<dyn Backend>)
    }

    unsafe extern "system" fn collect(
        hm: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<(Rect, bool)>);
        let mut mi = MONITORINFO::default();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hm, &mut mi).0 != 0 {
            let rc = mi.rcMonitor;
            // dwFlags 的第 1 位 = MONITORINFOF_PRIMARY。不引那个常量，避免为了一次
            // 位运算再拖一个 feature 进来。
            out.push((Rect { x: rc.left, y: rc.top, w: rc.right - rc.left, h: rc.bottom - rc.top }, mi.dwFlags & 1 != 0));
        }
        BOOL(1)
    }

    /// 主屏排第一，其余按系统枚举次序。这样 `monitor: 0` 在任何接法下都是"老师看的那块"，
    /// 不会随着插拔副屏悄悄把采集目标换掉——那是会毁掉一整节课的事。
    fn monitors() -> Vec<(Rect, bool)> {
        unsafe {
            let mut v: Vec<(Rect, bool)> = Vec::new();
            let cb: MONITORENUMPROC = Some(collect);
            EnumDisplayMonitors(None, None, cb, LPARAM(&mut v as *mut _ as isize));
            let (prim, rest): (Vec<_>, Vec<_>) = v.into_iter().partition(|(_, p)| *p);
            prim.into_iter().chain(rest).collect()
        }
    }

    impl Gdi {
        fn open(monitor: usize) -> Result<Self, String> {
            // 不设这个，150% 缩放的 4K 一体机上拿到的是系统拉伸过的图，
            // 课件上的小字在 blob 里永远读不出来。
            let dpi_aware = unsafe { SetProcessDPIAware() }.0 != 0;
            let ms = monitors();
            if ms.is_empty() {
                return Err("EnumDisplayMonitors 一个显示器都没枚举到（没有桌面？）。".into());
            }
            let idx = monitor.min(ms.len() - 1);
            let (r, primary) = ms[idx];
            if r.w <= 0 || r.h <= 0 {
                return Err(format!("显示器 {idx} 的矩形是 {}x{}，不可用", r.w, r.h));
            }
            Ok(Gdi { r, monitor: idx, primary, dpi_aware, errs: 0 })
        }
    }

    impl Backend for Gdi {
        fn grab(&mut self) -> Result<Option<Frame>, String> {
            let (w, h) = (self.r.w, self.r.h);
            let (pw, ph) = (w as usize, h as usize);
            unsafe {
                let screen = GetDC(None);
                if screen.is_invalid() {
                    self.errs += 1;
                    return Err("GetDC(None) 拿不到屏幕 DC".into());
                }
                let mem = CreateCompatibleDC(Some(screen));
                let bmp = CreateCompatibleBitmap(screen, w, h);
                let mut px = vec![0u8; pw * ph * 4];
                // BITMAPINFO 只 derive 了 Clone/Copy/Debug/PartialEq，没有 Default，
                // 所以整块清零再填头。它本就是 POD，zeroed 就是它的默认值。
                let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
                bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = w;
                // 负高度 = 自上而下的 DIB，省掉一次整帧翻行。
                bmi.bmiHeader.biHeight = -h;
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = BI_RGB.0;
                let mut failure: Option<String> = None;
                let old = if mem.is_invalid() {
                    failure = Some("CreateCompatibleDC 失败".to_string());
                    HGDIOBJ(std::ptr::null_mut())
                } else if bmp.is_invalid() {
                    failure = Some("CreateCompatibleBitmap 失败".to_string());
                    HGDIOBJ(std::ptr::null_mut())
                } else {
                    SelectObject(mem, bmp.into())
                };
                if failure.is_none() {
                    if let Err(e) = BitBlt(mem, 0, 0, w, h, Some(screen), self.r.x, self.r.y, SRCCOPY) {
                        failure = Some(format!("BitBlt：{e}"));
                    }
                }
                if failure.is_none() {
                    // 返回的是"写了几行"，0 才是失败。
                    let got = GetDIBits(
                        mem,
                        bmp,
                        0,
                        h as u32,
                        Some(px.as_mut_ptr() as *mut c_void),
                        &mut bmi,
                        DIB_RGB_COLORS,
                    );
                    if got == 0 {
                        failure = Some("GetDIBits 一行都没写出来".to_string());
                    }
                }
                if !old.is_invalid() {
                    SelectObject(mem, old);
                }
                if !mem.is_invalid() {
                    let _ = DeleteDC(mem);
                }
                if !bmp.is_invalid() {
                    let _ = DeleteObject(bmp.into());
                }
                let _ = ReleaseDC(None, screen);
                if let Some(e) = failure {
                    self.errs += 1;
                    return Err(e);
                }
                Ok(Some(Frame { width: pw, height: ph, rgb: bgra_to_rgb(&px) }))
            }
        }

        fn describe(&self) -> serde_json::Value {
            json!({
                "backend": "gdi",
                "monitor": self.monitor,
                "primary": self.primary,
                "width": self.r.w.max(0) as u64,
                "height": self.r.h.max(0) as u64,
                "dpi_aware": self.dpi_aware,
            })
        }

        fn errors(&self) -> usize {
            self.errs
        }
    }
}

#[cfg(windows)]
mod dxgi {
    use super::{bgra_to_rgb, Backend, Frame};
    use serde_json::json;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device,
        ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
        IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    };
    // 像素格式与采样描述在 Dxgi::Common 下，不在 Dxgi 本身；DXGI_OUTDUPL_DESC 也只给
    // 一个 ModeDesc，尺寸要从里面拿。
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };

    pub struct Dxgi {
        dup: IDXGIOutputDuplication,
        ctx: ID3D11DeviceContext,
        staging: ID3D11Texture2D,
        w: u32,
        h: u32,
        monitor: usize,
        errs: usize,
        rebuilds: usize,
    }

    pub fn opened(monitor: usize) -> Result<Box<dyn Backend>, String> {
        Dxgi::open(monitor).map(|d| Box::new(d) as Box<dyn Backend>)
    }

    struct Bound {
        dup: IDXGIOutputDuplication,
        staging: ID3D11Texture2D,
        w: u32,
        h: u32,
    }

    fn bind(monitor: usize) -> Result<(Bound, ID3D11DeviceContext), String> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1：{e}"))?;
            let adapter = factory
                .EnumAdapters1(0)
                .map_err(|e| format!("EnumAdapters1(0)：{e}（没有可用的显示适配器）"))?;
            let output = adapter
                .EnumOutputs(monitor as u32)
                .map_err(|e| format!("EnumOutputs({monitor})：{e}（没有第 {monitor} 号输出）"))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| format!("取 IDXGIOutput1：{e}"))?;
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(std::ptr::null_mut()),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(|e| format!("D3D11CreateDevice：{e}"))?;
            let device = device.ok_or("D3D11CreateDevice 没交出设备")?;
            let ctx = ctx.ok_or("D3D11CreateDevice 没交出上下文")?;
            let dup = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("DuplicateOutput：{e}（RDP / 虚机 / 基础显示驱动下通常拿不到）"))?;
            let d = dup.GetDesc();
            let (mw, mh) = (d.ModeDesc.Width, d.ModeDesc.Height);
            let staging = make_staging(&device, mw, mh)?;
            Ok((Bound { dup, staging, w: mw, h: mh }, ctx))
        }
    }

    unsafe fn make_staging(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w.max(1),
            Height: h.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(|e| format!("CreateTexture2D：{e}"))?;
        tex.ok_or("CreateTexture2D 没交出纹理".to_string())
    }

    impl Dxgi {
        fn open(monitor: usize) -> Result<Self, String> {
            let (b, ctx) = bind(monitor)?;
            Ok(Dxgi { dup: b.dup, ctx, staging: b.staging, w: b.w, h: b.h, monitor, errs: 0, rebuilds: 0 })
        }

        fn rebuild(&mut self) -> Result<(), String> {
            let (b, ctx) = bind(self.monitor)?;
            self.dup = b.dup;
            self.staging = b.staging;
            self.ctx = ctx;
            self.w = b.w;
            self.h = b.h;
            self.rebuilds += 1;
            Ok(())
        }

        /// 把桌面纹理搬到 staging 再读进内存。顺序不能反：先 `Map` 再 `CopyResource`
        /// 不会报错，但读到的是上一帧或垃圾——而画面看起来完全合理。
        /// `CopyResource` 在 windows 0.62 里不返回 `Result`（它真的没什么可失败的），
        /// 所以不能 `?`；它的失败会以“下一行 `Map` 报错”的形式暴露。
        unsafe fn read_frame(
            &self,
            res: Option<IDXGIResource>,
            w: usize,
            h: usize,
        ) -> Result<Option<Frame>, String> {
            let acquired = res.ok_or("AcquireNextFrame 成功却没给资源")?;
            let desktop: ID3D11Texture2D = acquired.cast().map_err(|e| format!("取桌面纹理：{e}"))?;
            // dst = staging（CPU 可读），src = 桌面纹理（只能 GPU 访问）。反了会直接失败。
            self.ctx.CopyResource(&self.staging, &desktop);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| format!("Map：{e}"))?;
            let pitch = mapped.RowPitch as usize;
            if mapped.pData.is_null() || pitch < w * 4 {
                self.ctx.Unmap(&self.staging, 0);
                return Err(format!("staging 行距 {pitch} 装不下 {w} 像素（或 pData 为空）"));
            }
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h);
            let mut bgra = vec![0u8; w * 4 * h];
            for y in 0..h {
                bgra[y * w * 4..(y + 1) * w * 4].copy_from_slice(&src[y * pitch..y * pitch + w * 4]);
            }
            self.ctx.Unmap(&self.staging, 0);
            Ok(Some(Frame { width: w, height: h, rgb: bgra_to_rgb(&bgra) }))
        }
    }

    impl Backend for Dxgi {
        fn grab(&mut self) -> Result<Option<Frame>, String> {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            // 超时 0：有没有新帧由合成器说，我们只按 poll_ms 的节奏去问一次。
            unsafe {
                match self.dup.AcquireNextFrame(0, &mut info, &mut res) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                    Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                        // 模式切换、显示器热插拔都会掐断复制接口。重建，并且把它记进数据。
                        self.errs += 1;
                        self.rebuild()?;
                        return Ok(None);
                    }
                    Err(e) => {
                        self.errs += 1;
                        return Err(format!("AcquireNextFrame：{e}"));
                    }
                }
            }
            let (w, h) = (self.w as usize, self.h as usize);
            // 不管下面读成功还是报错，纹理都必须归还：一次 ReleaseFrame 漏掉，
            // 下一整个复制接口就卡在那一帧上。
            let out = unsafe { self.read_frame(res, w, h) };
            unsafe {
                let _ = self.dup.ReleaseFrame();
            }
            if out.is_err() {
                self.errs += 1;
            }
            out
        }

        fn describe(&self) -> serde_json::Value {
            json!({
                "backend": "dxgi",
                "monitor": self.monitor,
                "width": self.w as u64,
                "height": self.h as u64,
                "rebuilds": self.rebuilds as u64,
            })
        }

        fn errors(&self) -> usize {
            self.errs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_list_is_the_contract_the_ui_copies() {
        assert_eq!(BACKENDS, ["auto", "gdi", "dxgi"]);
    }

    #[test]
    fn an_unknown_capture_is_refused_not_silently_switched() {
        // 非 Windows 上 open() 恒拒（连清单都不查），所以这条只在 Windows 侧断行为，
        // 另一侧断"这台机器上这个源确实起不来"。
        // 不用 unwrap_err：它要求 Ok 那一侧（Opened）也实现 Debug，而 Box<dyn Backend> 不满足。
        let e = open("x11", 0).err().expect("清单外的后端必须被当场拒，不能悄悄换一条路");
        #[cfg(windows)]
        assert!(e.contains("capture"), "{e}");
        #[cfg(not(windows))]
        assert!(e.contains("不是 Windows"), "{e}");
    }

    #[test]
    fn bgra_is_unrolled_to_rgb() {
        assert_eq!(bgra_to_rgb(&[10, 20, 30, 255, 40, 50, 60, 0]), vec![30, 20, 10, 60, 50, 40]);
        // 六字节 = 一个整像素 + 两个尾巴：整像素照旧产出，尾巴被丢掉而不 panic。
        // is_empty() 等于要求“不足 4 字节就全丢”，但实现不是这么写的。
        assert_eq!(bgra_to_rgb(&[0, 0, 0, 0, 0, 0]), vec![0, 0, 0], "不足一像素的尾巴被丢掉，不 panic");
        assert!(bgra_to_rgb(&[0, 0, 0]).is_empty(), "不足一像素就什么都没有");
    }
}
