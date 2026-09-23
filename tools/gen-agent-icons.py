#!/usr/bin/env python3
"""生成 agent 观察端的最小图标集（纯字节，无需 PIL/网络）。

Tauri 在 Windows 编译期需要 icons/icon.ico 嵌入 Win32 资源，generate_context! 也会
读取 bundle.icon 里的图标——缺了就 proc-macro panic。这里用 zlib 手工拼合法 PNG，
再把 128px PNG 封进 ICO（Vista+ 支持 PNG-in-ICO）。产物提交进仓库，CI 直接可用。
"""
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "agent", "src-tauri", "icons")

# 观察端主题色（accent 蓝），纯色方块当占位图标。
RGBA = (122, 162, 247, 255)


def png(size: int, rgba=RGBA) -> bytes:
    raw = bytearray()
    row = bytes(rgba) * size
    for _ in range(size):
        raw.append(0)  # filter type: None
        raw += row

    def chunk(typ: bytes, data: bytes) -> bytes:
        body = typ + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8-bit RGBA
    idat = zlib.compress(bytes(raw), 9)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")


def ico(png_bytes: bytes, size: int) -> bytes:
    dim = size if size < 256 else 0
    header = struct.pack("<HHH", 0, 1, 1)  # reserved, type=1(icon), count=1
    entry = struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png_bytes), 6 + 16)
    return header + entry + png_bytes


def main() -> None:
    os.makedirs(OUT, exist_ok=True)
    files = {
        "32x32.png": png(32),
        "128x128.png": png(128),
        "128x128@2x.png": png(256),
        "icon.png": png(128),
    }
    for name, data in files.items():
        with open(os.path.join(OUT, name), "wb") as f:
            f.write(data)
    with open(os.path.join(OUT, "icon.ico"), "wb") as f:
        f.write(ico(png(128), 128))
    print("wrote icons to", os.path.normpath(OUT))
    for name in list(files) + ["icon.ico"]:
        p = os.path.join(OUT, name)
        print(f"  {name}: {os.path.getsize(p)} bytes")


if __name__ == "__main__":
    main()
