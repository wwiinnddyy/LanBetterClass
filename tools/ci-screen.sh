#!/usr/bin/env bash
# 屏幕关键帧适配器（a-screen）端到端守卫，Linux 与 Windows 两个 runner 各跑同一份。
#
# runner 上没有可用桌面（或只有一个全黑的），所以断言分两层，各断各的事：
# 1. 单元层（cargo test --workspace 已经跑过）：灰度采样不跳像素、dHash 与均差两道门限、
#    变化区 bbox、PNG 读写——这些是纯函数，合成图像就能钉死，不需要显示器。
# 2. 集成层（本脚本）：用 Python 标准库 zlib + struct 手搓 PNG 当"课件翻页"，让 a-screen
#    走 fixture 回放，断真事件、真 blob、真导出。手写而不是 PIL，是为了让适配器去解一个
#    **第三方写出来的文件**——现场从别的工具导出的图就是这种（与 ci-audio 用 wave 同理）。
#
# 反向断言同样重要：静止的一节课必须只有一帧。"没变化"如果能被伪装成"很多变化"，
# 一节课就变成几千张几乎一样的截图，那份证据从此没人看。
#
# 设备抓屏那条分支（capture=gdi / dxgi）在这里只能断"不许崩、不许报假引用"——
# runner 上有没有桌面是机器的事，不是代码的事。真机验证见 agent/README.md。
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT) EXE=.exe ;;
  *) EXE= ;;
esac
export PYTHONIOENCODING=utf-8
echo "platform=$(uname -s)  exe='${EXE:-无}'"

BIN=${BIN:-target/debug}
WORK=${SCREEN_DIR:-.ci-screen}
LESSON=L-demo-0001
CLIENT="$BIN/classagent-client$EXE"
ADAPTER="$BIN/a-screen$EXE"
[ -f "$CLIENT" ] || { echo "FAIL 找不到 $CLIENT"; exit 1; }
[ -f "$ADAPTER" ] || { echo "FAIL 找不到 $ADAPTER"; exit 1; }

rm -rf "$WORK"
mkdir -p "$WORK/slides" "$WORK/still" "$WORK/slow" \
         "$WORK/adp-slides" "$WORK/adp-still" "$WORK/adp-nodev" "$WORK/adp-cut" "$WORK/adp-throttle" \
         "$WORK/adp-reload" "$WORK/data-slides" "$WORK/data-still" "$WORK/data-nodev" \
         "$WORK/data-cut" "$WORK/data-throttle" "$WORK/data-reload"

# ---- 造两排"课件"：翻页要能被看见，静止不能被看成翻页 ----
python3 - "$WORK" <<'PY'
import os, struct, sys, zlib

work = sys.argv[1]


def png(path, w, h, pix):
    """标准库写 PNG：IHDR + IDAT(zlib) + IEND，逐行 filter=0，8bit RGB。

    刻意用最"笨"的容器：非隔行、无调色板、无透明。适配器连这种都要能读，
    而现场拿到的图比这花哨得多。
    """
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        for x in range(w):
            raw += bytes(pix(x, y))

    def chunk(tag, data):
        return struct.pack('>I', len(data)) + tag + data + struct.pack('>I', zlib.crc32(tag + data) & 0xffffffff)

    ihdr = struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)
    blob = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', ihdr) + chunk(b'IDAT', zlib.compress(bytes(raw), 6)) + chunk(b'IEND', b'')
    with open(path, 'wb') as f:
        f.write(blob)


W, H = 160, 100


def slide(n):
    """第 n 页课件：标题条 + n 个内容块，块的位置随 n 变。

    翻页时画面里过半的格子会动，所以 dHash 距离一定越得过默认门限——
    反过来说，任何"只改一个角"的操作都不该被它触发。
    """
    def pix(x, y):
        if y < 12:
            return (240, 240, 245)
        base = 250 if (x + y) % 7 else 244
        for i in range(n):
            bx = 8 + ((i * 37 + n * 11) % 120)
            by = 20 + ((i * 23 + n * 7) % 70)
            if bx <= x < bx + 34 and by <= y < by + 22:
                return (30 + 17 * i, 60, 200 - 11 * i)
        return (base, base, base)
    return pix


# ① 六页不同课件
for i in range(1, 7):
    png(os.path.join(work, 'slides', '%04d.png' % i), W, H, slide(i))

# ② 静止：同一页 12 张 + 最后一张只在一个角上动了 4x4 像素（低对比）
still = slide(1)
for i in range(1, 13):
    png(os.path.join(work, 'still', '%04d.png' % i), W, H, still)


def blink(x, y):
    c = still(x, y)
    if 40 <= x < 44 and 60 <= y < 64:
        return (c[0] - 30, c[1] - 30, c[2] - 30)
    return c


png(os.path.join(work, 'still', '0013.png'), W, H, blink)

# ④ 慢速回放：每页都不同，600ms 一页，够被中途下课切在中间
for i in range(1, 25):
    png(os.path.join(work, 'slow', '%04d.png' % i), W, H, slide(1 + (i % 5)))
PY

# ---- 各场景的装载声明：都从仓库里的声明派生，不动仓库 ----
python3 - "$WORK" <<'PY'
import json, os, sys

work = sys.argv[1]
src = json.load(open('adapters.d/a-screen.adapter.json', encoding='utf-8'))
src['enabled'] = True
src['argv'] = ['$TARGET_DIR/a-screen']


def decl(name, data):
    d = json.loads(json.dumps(src))
    d['params'] = data
    json.dump(d, open(os.path.join(work, name, 'a-screen.adapter.json'), 'w', encoding='utf-8'), ensure_ascii=False, indent=2)


def fixture(dir_name, **screen):
    p = {'source': 'fixture', 'fixture_dir': f'{work}/{dir_name}', 'speed': 1000, 'capture': 'gdi',
         'monitor': 0, 'screen': {'poll_ms': 40, 'min_interval_ms': 0, 'min_dist': 3, 'min_mad': 2,
                                  'max_width': 0, 'max_frames_per_lesson': 0, 'max_bytes_per_lesson': 0,
                                  'emit_dirty': True}}
    p['screen'].update(screen)
    return p


# ① 翻页：六页都该落盘
decl('adp-slides', fixture('slides'))
# ② 静止：默认门限下只该有开场那一帧
decl('adp-still', fixture('still', min_dist=6, min_mad=4))
# ③ 真去抓屏（runner 通常没有桌面）：不许崩，也不许报假引用
d = json.loads(json.dumps(src))
d['params'].update({'source': 'device', 'capture': 'gdi', 'screen': {'poll_ms': 100, 'min_interval_ms': 0}})
json.dump(d, open(os.path.join(work, 'adp-nodev', 'a-screen.adapter.json'), 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
# ④ 课到一半下课：最后那一帧与 session.close 都是 StopLesson 之后才发的
decl('adp-cut', dict(fixture('slow', poll_ms=600), speed=1))
# ⑤ 节流：100ms 问一次、300ms 才许落一张
decl('adp-throttle', fixture('slides', poll_ms=100, min_interval_ms=300))
# ⑥ 教师改门限：先给一份能跑起来的，再由写接口改它
decl('adp-reload', fixture('slides', min_dist=6, min_mad=4))
PY

run_lesson() {  # run_lesson <声明目录> <数据目录> <秒数> <日志名>
  "$CLIENT" run --data "$2" --adapters "$1" --lesson examples/lesson.demo.json --max-seconds "$3" \
    < /dev/null > "$WORK/$4" 2>&1 || true
  tail -4 "$WORK/$4"
  "$CLIENT" export --data "$2" --lesson "$LESSON" > "$WORK/${4%.log}.export" 2>&1 || true
}

echo "--- ① 翻页序列：每页都该成为一帧 ---"
run_lesson "$WORK/adp-slides" "$WORK/data-slides" 8 run1.log

echo "--- ② 静止序列：只该有开场那一帧 ---"
run_lesson "$WORK/adp-still" "$WORK/data-still" 8 run2.log

echo "--- ③ 真去抓屏（runner 多半没有桌面）：不许崩，不许报假引用 ---"
run_lesson "$WORK/adp-nodev" "$WORK/data-nodev" 6 run3.log

echo "--- ④ 课到一半下课：尾帧与收课记录还在这一节课里 ---"
run_lesson "$WORK/adp-cut" "$WORK/data-cut" 5 run4.log

echo "--- ⑤ 节流：问得勤不等于落得勤 ---"
run_lesson "$WORK/adp-throttle" "$WORK/data-throttle" 8 run5.log

echo "--- ⑥ 教师改门限与非法参数：写盘后下一节课读得回来 ---"
ADP6="$WORK/adp-reload"
DATA6="$WORK/data-reload"
PORT6=${PORT6:-8801}
cp "$ADP6/a-screen.adapter.json" "$WORK/p6.before"
"$CLIENT" serve --data "$DATA6" --adapters "$ADP6" --port "$PORT6" --allow-write > "$WORK/serve6.log" 2>&1 &
SRV6=$!
trap 'kill $SRV6 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$PORT6/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
# 只提交 screen 里的两个门限：source / fixture_dir / speed 必须原样留在磁盘上，否则这节课不会回放
H_OK=$(curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"params":{"screen":{"min_dist":64,"min_mad":255}}}' \
  -o "$WORK/p6.json" -w '%{http_code}' "http://127.0.0.1:$PORT6/api/adapter/a-screen.adapter.json")
# 不存在的后端：起不来只会让一节课安静地没帧，必须当场被拒
H_BAD=$(curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"params":{"capture":"x11"}}' \
  -o "$WORK/p6bad.json" -w '%{http_code}' "http://127.0.0.1:$PORT6/api/adapter/a-screen.adapter.json")
# dHash 只有 64 位：写 999 不是"更宽松"
H_BIG=$(curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"params":{"screen":{"min_dist":999}}}' \
  -o "$WORK/p6big.json" -w '%{http_code}' "http://127.0.0.1:$PORT6/api/adapter/a-screen.adapter.json")
printf '%s %s %s\n' "$H_OK" "$H_BAD" "$H_BIG" > "$WORK/p6.codes"
kill $SRV6 2>/dev/null || true; trap - EXIT
run_lesson "$ADP6" "$DATA6" 8 run6.log

echo "--- ⑦ 回看链路：blob 的 MIME 与字节 ---"
# 观察端拿 <img src> 直连这个接口看关键帧（桌面进程不做二进制中转），
# 所以 content_type 与字节完整性就是那张图能不能出现在窗口里的全部条件。
PORT7=${PORT7:-8802}
# 与下面 Python 的 sorted()[0] 同一把尺：find 的默认序不保证，两边选不同文件会假报字节不符
PNG_NAME=$(basename "$(find "$WORK/data-slides/lessons/$LESSON/blobs" -name '*.png' | sort | head -1)")
"$CLIENT" serve --data "$WORK/data-slides" --adapters "$WORK/adp-slides" --port "$PORT7" > "$WORK/serve7.log" 2>&1 &
SRV7=$!
trap 'kill $SRV7 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$PORT7/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
BLOB_CT=$(curl -s -o "$WORK/blob.png" -w '%{content_type}' \
  "http://127.0.0.1:$PORT7/api/lesson/$LESSON/blob/$PNG_NAME")
echo "$BLOB_CT" > "$WORK/blob.ctype"
BLOB_N=$(curl -s -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$PORT7/api/lesson/$LESSON/blob/..%2Fmeta.json")
echo "$BLOB_N" > "$WORK/blob.traversal"
kill $SRV7 2>/dev/null || true; trap - EXIT

# ---- 清单交叉检查：三处都写着同一批后端名 ----
echo "--- ⑧ 后端清单：适配器、写接口校验、观察端下拉框必须一致 ----"
python3 - <<'PY'
import json, re, sys

caps = open('client/adapters/a-screen/src/capture.rs', encoding='utf-8').read()
serve = open('client/src/serve.rs', encoding='utf-8').read()
app = open('agent/ui/app.js', encoding='utf-8').read()
decl = json.load(open('adapters.d/a-screen.adapter.json', encoding='utf-8'))

fails = []


def need(c, m):
    if not c:
        fails.append(m)


m = re.search(r'pub const BACKENDS: \[[^\]]*\]\s*=\s*\[([^\]]*)\]', caps)
need(m, 'capture.rs 里的 BACKENDS 清单没找到——它是这条链的唯一出处')
have = sorted(re.findall(r'"([a-z]+)"', m.group(1))) if m else []
need(have == ['auto', 'dxgi', 'gdi'], f'BACKENDS 应当是 auto/dxgi/gdi，实得 {have}')

# 注意末项背没有 `|`：把分隔符写进重复单元会让这一条永远匹不上，守卫变成摆设
served = re.search(r'matches!\(\s*s\s*,\s*((?:"[a-z]+"\s*\|\s*)*"[a-z]+")\s*\)', serve)
need(served, 'serve.rs 里找不到 capture 的白名单（校验漂移了就是"能写进去但起不来"）')
in_serve = sorted(set(re.findall(r'"([a-z]+)"', served.group(1)))) if served else []
need(in_serve == have, f'serve.rs 认的后端与适配器不一致：{in_serve} ≠ {have}')

opts = re.search(r'\[\s*"auto",\s*"gdi",\s*"dxgi"\s*\]', app)
need(opts, 'app.js 的下拉框选项与 BACKENDS 不再一致：改适配器清单要同时改界面')

need(decl['params']['capture'] in have, f'默认声明写了个不存在的后端：{decl["params"]["capture"]}')
screen = decl['params']['screen']
ad = open('client/adapters/a-screen/src/main.rs', encoding='utf-8').read()
for k in screen:
    need(f'"{k}"' in ad, f'默认声明里的 {k} 适配器不认（写了也不会生效）：{sorted(screen)}')
    need(f'"{k}"' in app or k in ('emit_dirty',), f'观察端表单漏了 {k}：磁盘上改不到它')
need('"capture"' in ad and '"monitor"' in ad, '适配器不再读顶层 capture / monitor，界面却还在写')

# min_dist 的上界不是个魔数：它是 dHash 的位数。改了哈希尺寸就必须同时改写接口校验，
# 否则一边以为“最大 64”而另一边已经能算到 72，教师拿到的是“合法但永远不落帧”。
ph = open('client/adapters/a-screen/src/phash.rs', encoding='utf-8').read()
dim = re.findall(r'pub const (HASH_COLS|HASH_ROWS): usize = (\d+);', ph)
bits = {k: int(v) for k, v in dim}
need({'HASH_COLS', 'HASH_ROWS'} <= set(bits), f'phash.rs 没报出哈希尺寸：{dim}')
max_dist = bits.get('HASH_COLS', 0) * bits.get('HASH_ROWS', 0)
# 不拿中文文案做锁（改一句提示语就能把守卫拆掉）：只认 min_dist 之后紧跟着的那个上界数字
bnd = re.search(r'screen\.min_dist[^\d]{1,12}(\d+)', serve)
need(bnd, 'serve.rs 里找不到 min_dist 的上界文案')
if bnd:
    need(int(bnd.group(1)) == max_dist,
         f'写接口以为 dHash 只有 {bnd.group(1)} 位，适配器算出来是 {max_dist} 位（{bits}）')
mx = re.search(r'pub const MAX_DIST: u32 = \(HASH_COLS \* HASH_ROWS\) as u32;', ph)
need(mx, 'MAX_DIST 不再是 HASH_COLS × HASH_ROWS 的直接产物：上界与哈希尺寸脱钩了，两边会各说各话')

if fails:
    print('FAIL 清单交叉检查')
    for f in fails:
        print('  -', f)
    sys.exit(1)
print('清单一致：auto / gdi / dxgi 三处对齐，调参键与表单对齐')
PY

# ---- 主断言：从导出与事件里读事实 ----
python3 - "$WORK" <<'PY'
import glob, json, os, sys

work = sys.argv[1]
fails = []


def need(c, msg):
    if not c:
        fails.append(msg)


def kinds_of(evs, kind):
    return [e for e in evs if e['envelope']['kind'] == kind]


def events(data, lesson='L-demo-0001'):
    path = os.path.join(data, 'lessons', lesson, 'events.ndjson')
    if not os.path.exists(path):
        return []
    return [json.loads(l) for l in open(path, encoding='utf-8') if l.strip()]


def close_of(evs):
    c = kinds_of(evs, 'session.close')
    return c[-1]['envelope']['payload'] if c else None


def blobs(data):
    return set(os.path.basename(x) for x in glob.glob(os.path.join(data, 'lessons', '*', 'blobs', '*')))


def refs_of(evs):
    return [k['envelope']['payload'].get('blob') for k in kinds_of(evs, 'screen.keyframe')]


def exported(name):
    # `run` 不写 ai_payload.json，导出是 CLI 单独一步；上面那层把每场的导出存成 *.export。
    p = os.path.join(work, name)
    return json.load(open(p, encoding='utf-8')) if os.path.exists(p) else None


# ---------- ① 翻页 ----------
d1 = os.path.join(work, 'data-slides')
ev1 = events(d1)
kf1 = kinds_of(ev1, 'screen.keyframe')
need(len(kf1) == 6, f'① 六页课件应得 6 帧，实得 {len(kf1)}')
b1 = blobs(d1)
missing = [r for r in refs_of(ev1) if r not in b1]
need(not missing, f'① 报了引用却没有文件：{missing}')
c1 = close_of(ev1)
need(c1 and c1.get('chunks') == len(kf1), f'① 收课帧数要和事件数一致：{c1}')
need(c1 and c1.get('bytes') == sum(k['envelope']['payload']['len'] for k in kf1),
     f'① 收课字节要和 blob 之和一致：{c1}')
need(c1 and c1.get('error') is None, f'① 正向路径不该带错误：{c1}')
need(all(k['envelope']['payload']['len'] == os.path.getsize(os.path.join(d1, 'lessons', 'L-demo-0001', 'blobs', k['envelope']['payload']['blob'])) for k in kf1),
     '① payload.len 与磁盘字节数不符——下游会按错的长度读')
t1 = [k['envelope']['payload']['trigger'] for k in kf1]
need(t1[0] == 'open' and all(x in ('phash', 'mass') for x in t1[1:]), f'① 第一帧该是 open，其余是变化触发：{t1}')
need(all(k['envelope']['payload']['width'] == 160 and k['envelope']['payload']['height'] == 100 for k in kf1),
     '① 不降采样时自述尺寸就该等于屏幕尺寸')
need('dirty' in kf1[5]['envelope']['payload'], '① 翻页该带上变化区 bbox')
need('dirty' not in kf1[0]['envelope']['payload'], '① 开场帧没有可比的上一帧，不该凭空给一个变化区')
# 同一理由管到差值本身：开场帧报个 Δ64 会被下游读成“课上发生了一次大变化”，
# 而那一刻什么都还没发生。没量过就必须是 null，也不是 0（0 会被读成“完全没变”）。
need(kf1[0]['envelope']['payload'].get('dist') is None,
     f"① 开场帧不该报出一个测出来的差值：{kf1[0]['envelope']['payload']}")
need(kf1[0]['envelope']['payload'].get('mad') is None, '① 开场帧的均差同样是凭空补的')
need(all(k['envelope']['payload'].get('dist') is not None for k in kf1[1:]),
     '① 变化帧必须带上与上一落盘帧比出来的差值，否则调门限没有依据')
p1 = exported('run1.export')
need(p1 is not None, '① 导出没跑出来')
if p1:
    need(p1['stats']['keyframes'] == 6, f"① 导出的帧数不对：{p1['stats']['keyframes']}")
    need(p1['stats']['keyframe_bytes'] == c1['bytes'], '① 导出的关键帧字节总和应与收课自述一致')
    row = [x for x in p1['track'] if x['kind'] == 'screen.keyframe'][0]
    need(row.get('refs') and row['refs'][0].endswith('.png'), f'① track 行要带 blob 引用：{row}')
    need(row.get('detail', {}).get('trigger') == 'open', f'① detail 没把触发原因透出来：{row}')
    h = p1['sources']['a-screen']
    # 回放模式不会报 backend（根本没有抓屏），这里断的是开场自述真到了导出；
    # “用了哪条后端”归 ③ 的设备分支断。
    need(h['screen']['input'] == 'fixture', f"① 自述没说清来路：{h['screen']}")
    need(h['screen'].get('frames'), f"① 自述该报出回放了几个文件：{h['screen']}")
    need(h['screen']['screen']['min_dist'] == 3, f"① 本节课生效的门限没进导出：{h['screen']}")

# ---------- ② 静止 ----------
d2 = os.path.join(work, 'data-still')
ev2 = events(d2)
kf2 = kinds_of(ev2, 'screen.keyframe')
need(len(kf2) == 1, f'② 静止的一节课只该有开场那一帧，实得 {len(kf2)}')
c2 = close_of(ev2)
need(c2 and c2.get('unchanged') == 12, f'② 十二次"没变"要被记下来（含最后一张只动了 4x4 像素的）：{c2}')
need(c2 and c2.get('throttled') == 0, f'② 这一套没设节流，不该有 throttled：{c2}')
need(c2 and c2['polls'] >= 13, f'② 问了几年？polls 应覆盖整排图：{c2}')

# ---------- ③ 设备路径 ----------
d3 = os.path.join(work, 'data-nodev')
ev3 = events(d3)
kf3 = kinds_of(ev3, 'screen.keyframe')
c3 = close_of(ev3)
# 有没有桌面是机器的事：这里只断"不许说谎、不许崩"。
need(all(r in blobs(d3) for r in refs_of(ev3)), f'③ 设备路径报了不存在的 blob：{refs_of(ev3)}')
need(not any('panic' in l.lower() for l in open(os.path.join(work, 'run3.log'), encoding='utf-8', errors='ignore')),
     '③ 设备路径 panic 了：没有桌面必须走"该源缺席"，不是把进程弄崩')
if not ev3:
    p3 = exported('run3.export')
    need(p3 is None or 'a-screen' not in p3['sources'], '③ 全程零事件的源不该带着"已工作"的计数进健康表')
    print('③ 本机没有可采的桌面：该源按设计从健康表缺席')
else:
    need(c3 is not None, '③ 既然产出了事件，收课记录也必须在')
    need(c3 and c3['chunks'] == len(kf3), f'③ 帧数与自述不一致：{c3}')
    # 只有真设备分支才有 backend 可报；这一条在跑得动桌面的 runner 上才是有效守卫。
    p3 = exported('run3.export')
    if p3 and 'a-screen' in p3['sources']:
        b = p3['sources']['a-screen']['screen'].get('backend')
        need(b in ('gdi', 'dxgi'), f"③ 设备路径必须自述用了哪条后端：{p3['sources']['a-screen']['screen']}")
    print(f"③ 这台 runner 有桌面：采到 {len(kf3)} 帧")

# ---------- ④ 中途下课 ----------
d4 = os.path.join(work, 'data-cut')
ev4 = events(d4)
kf4 = kinds_of(ev4, 'screen.keyframe')
need(len(kf4) >= 2, f'④ 5 秒里 600ms 一页，至少该有几帧，实得 {len(kf4)}')
c4 = close_of(ev4)
need(c4 is not None, '④ 收课记录不见了：收尾时序又回到"客户端提前停止读管道"那个老 bug')
need(kf4[-1]['envelope']['payload']['trigger'] == 'close',
     f"④ 下课那一刻补采的帧要能被认出来，实得 {kf4[-1]['envelope']['payload']['trigger']}")
misc = os.path.join(d4, 'misc.ndjson')
if os.path.exists(misc):
    m = [json.loads(l) for l in open(misc, encoding='utf-8') if l.strip()]
    need(not [x for x in m if x.get('envelope', {}).get('kind') in ('screen.keyframe', 'session.close')],
         '④ 关键帧与收课记录掉到了课外面（misc.ndjson）——那是整节课证据消失的同一种掉法')
need(all(r in blobs(d4) for r in refs_of(ev4)), '④ 尾帧报了引用却没有文件')

# ---------- ⑤ 节流 ----------
d5 = os.path.join(work, 'data-throttle')
ev5 = events(d5)
kf5 = kinds_of(ev5, 'screen.keyframe')
c5 = close_of(ev5)
need(len(kf5) == 2, f'⑤ 300ms 节流下六页只该落 2 张（t=0 与 t=300），实得 {len(kf5)}')
need(c5 and c5.get('throttled') == 4, f'⑤ 被节流拦下的四次要数得出来：{c5}')
need(c5 and c5.get('unchanged') == 0, f'⑤ 这些帧彼此差别很大，不该被算成"无变化"：{c5}')

# ---------- ⑥ 教师改门限 ----------
codes = open(os.path.join(work, 'p6.codes'), encoding='utf-8').read().split()
need(codes[0] == '200', f'⑥ 合法门限被拒：{codes[0]} {open(os.path.join(work, "p6.json"), encoding="utf-8").read()}')
need(codes[1] == '400', f'⑥ 不存在的后端竟然写进去了：{codes[1]}')
need(codes[2] == '400', f'⑥ min_dist=999 竟然写进去了（会被适配器静默钳到 64）：{codes[2]}')
after = json.load(open(os.path.join(work, 'adp-reload', 'a-screen.adapter.json'), encoding='utf-8'))
need(after['params']['screen']['min_dist'] == 64, f"⑥ 门限没落盘：{after['params']['screen']}")
need(after['params']['screen']['min_mad'] == 255, '⑥ 深合并只该改提交的那一个键')
need(after['params']['screen']['poll_ms'] == 40, f"⑥ 没提交的兄弟键被抹掉了：{after['params']['screen']}")
need(after['params'].get('source') == 'fixture' and after['params'].get('fixture_dir'),
     f"⑥ 只提交 screen 却把回放路径弄丢了：{sorted(after['params'])}")
need(after['argv'] == json.load(open(os.path.join(work, 'p6.before'), encoding='utf-8'))['argv'], '⑥ argv 被改动了')
for k in ('p6bad', 'p6big'):
    body = open(os.path.join(work, k + '.json'), encoding='utf-8').read()
    need('error' in json.loads(body), f'⑥ {k} 的 400 没带原因：{body}')
# 两次被拒的写请求没动过盘：门限仍是刚写进去的 64/255，其它键也没被顺手动过
need(after['params']['screen']['min_dist'] == 64 and after['params']['screen']['min_mad'] == 255,
     f"被拒的请求竟然也改了盘：{after['params']['screen']}")
d6 = os.path.join(work, 'data-reload')
ev6 = events(d6)
kf6 = kinds_of(ev6, 'screen.keyframe')
need(len(kf6) == 1, f'⑥ 门限调到不可能越过之后，只该剩开场那一帧，实得 {len(kf6)}')
p6 = exported('run6.export')
v6 = p6['sources']['a-screen']['screen']
need(v6['screen']['min_dist'] == 64,
     f'⑥ 磁盘写了 64，适配器自述的却是 {v6["screen"]["min_dist"]}：整条链没接上')
need(v6['input'] == 'fixture', f'⑥ 自述没说清这一节是回放：{v6}')

# ---------- ⑦ 回看链路 ----------
ct = open(os.path.join(work, 'blob.ctype'), encoding='utf-8').read().strip()
need(ct.split(';')[0].strip() == 'image/png', f'⑦ blob 的 content_type 不是 image/png：{ct}')
got = open(os.path.join(work, 'blob.png'), 'rb').read()
names = sorted(os.path.basename(x) for x in glob.glob(os.path.join(work, 'data-slides', 'lessons', '*', 'blobs', '*.png')))
need(bool(names), '⑦ 没有任何 blob 可看')
disk = open(os.path.join(work, 'data-slides', 'lessons', 'L-demo-0001', 'blobs', names[0]), 'rb').read() if names else b''
need(got == disk, f'⑦ 经 HTTP 回来的字节与盘上的不一样（{len(got)} vs {len(disk)}）：<img> 会显示成破图')
trav = open(os.path.join(work, 'blob.traversal'), encoding='utf-8').read().strip()
need(trav in ('400', '404'), f'⑦ 路径穿越没被拦住：{trav}')

if fails:
    print('FAIL 关键帧守卫')
    for f in fails:
        print('  -', f)
    sys.exit(1)
print(f"关键帧链路已闭合：翻页 {len(kf1)} 帧静止 {len(kf2)} 帧节流 {len(kf5)} 帧中途下课 {len(kf4)} 帧；写接口 {codes}")
PY

echo "SCREEN OK"
