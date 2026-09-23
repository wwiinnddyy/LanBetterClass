#!/usr/bin/env bash
# 端到端守卫，Linux 与 Windows 两个 runner 上跑同一份。
# 本机磁盘不允许本地构建，所以这些断言只在 CI 上执行；但它们断的是真行为：
# 投递、seq 缺口、崩溃重启后的 seq 空间、blob 约定、预算强制、时间轴对齐。
#
# 两个刻意的平台处理：
# 1. 所有产物放在仓库内的 .ci-tmp/ 下，不用 /tmp。Git Bash 的 /tmp 指向 Temp，
#    而 runner 上的 Windows Python 把 "/tmp/x" 解析成 C:\tmp\x——两边会写到不同
#    地方，表现为"文件明明生成了却读不到"。相对路径 MSYS 不做转换，两边同源。
# 2. 每个 open() 显式 encoding='utf-8'。Windows Python 默认按本地代码页打开，
#    而声明文件里有中文，用默认编码会在 Windows 上直接 UnicodeDecodeError。
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT) EXE=.exe ;;
  *) EXE= ;;
esac
# Windows 上 Python 的 stdout 默认按本地代码页（cp1252）编码，中文输出会直接
# UnicodeEncodeError。这是工装的编码问题，不是采集端的——Rust 侧写的是 UTF-8。
export PYTHONIOENCODING=utf-8
echo "platform=$(uname -s)  exe='${EXE:-无}'"

BIN=${BIN:-target/debug}
# 不要叫 TMP：它是标准环境变量，runner 上是 Temp 目录，`${TMP:-.ci-tmp}` 会被它
# 覆盖，产物就悄悄跑到仓库外面去了。
WORK=${SMOKE_DIR:-.ci-tmp}
DATA=$WORK/data
ADP=$WORK/adapters
TONE=$WORK/tone.raw
LESSON=L-demo-0001
LOG=$WORK/client.log
MAX_SECONDS=${MAX_SECONDS:-20}

rm -rf "$WORK"
mkdir -p "$ADP" "$DATA"
CLIENT="$BIN/classagent-client$EXE"
[ -f "$CLIENT" ] || { echo "FAIL 找不到 $CLIENT"; exit 1; }

# 300 秒 16kHz 单声道 s16le 正弦波。故意长到能在 1 秒墙钟内灌完，
# 好把 a-audiofile 的 60 事件/秒预算真的顶穿。
python3 - "$TONE" <<'PY'
import array, math, sys
sr, secs = 16000, 300
a = array.array('h', (int(12000 * math.sin(2 * math.pi * 220 * i / sr)) for i in range(sr * secs)))
with open(sys.argv[1], 'wb') as f:
    a.tofile(f)
print('tone bytes =', a.itemsize * len(a))
PY

# 为本次运行生成一套适配器声明：真实音频路径 + 让 a-fake 制造缺口并主动崩溃。
python3 - "$ADP" "$TONE" <<'PY'
import json, sys
adp, tone = sys.argv[1], sys.argv[2]
fake = json.load(open('adapters.d/a-fake.adapter.json', encoding='utf-8'))
fake['enabled'] = True
fake['params'].update({'speed': 400, 'minutes': 45, 'skip_seq_at': [500, 501], 'crash_after_ticks': 3000})
json.dump(fake, open(f'{adp}/a-fake.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
af = json.load(open('adapters.d/a-audiofile.adapter.json', encoding='utf-8'))
af['enabled'] = True
af['params'].update({'path': tone.replace('\\', '/'), 'speed': 400})
json.dump(af, open(f'{adp}/a-audiofile.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
PY

set +e
"$CLIENT" run --data "$DATA" --adapters "$ADP" --lesson examples/lesson.demo.json --max-seconds "$MAX_SECONDS" \
  < /dev/null > "$LOG" 2>&1
RC=$?
set -e
echo "--- client 日志尾部 ---"
tail -25 "$LOG"
[ "$RC" = 0 ] || { echo "FAIL client 退出码 $RC"; exit 1; }
grep -q '退避后重启' "$LOG" || { echo "FAIL 没有观察到适配器重启（Windows 侧 spawn/管道未验证过的话就是这里）"; exit 1; }

"$CLIENT" export --data "$DATA" --lesson "$LESSON" || { echo "FAIL 导出失败"; exit 1; }
"$CLIENT" status --data "$DATA"

python3 - "$DATA" "$LESSON" <<'PY'
import json, os, sys
data, lesson = sys.argv[1], sys.argv[2]
ldir = os.path.join(data, 'lessons', lesson)
bdir = os.path.join(ldir, 'blobs')
p = json.load(open(os.path.join(ldir, 'ai_payload.json'), encoding='utf-8'))
st, src = p['stats'], p['sources']
blobs = os.listdir(bdir)
lines = sum(1 for _ in open(os.path.join(ldir, 'events.ndjson'), encoding='utf-8'))

print('== 实测 ==')
for k in ('strokes', 'utterances', 'erases', 'pages_touched', 'audio_chunks', 'audio_bytes',
          'ink_time_ms', 'writing_while_speaking_ms', 'longest_silence_ms', 'duration_ms'):
    print(f'  {k} = {st[k]}')
print(f'  events.ndjson 行数 = {lines}, track 条目 = {len(p["track"])}, blob 文件 = {len(blobs)}')
max_t1 = max((i['t1_ms'] for i in p['track']), default=0)
print(f'  track 最大时间 = {max_t1}ms')
for k, v in sorted(src.items()):
    print(f'  源 {k}: events={v["events"]} 缺口={v["gaps"]} 丢失={v["lost_events"]} '
          f'重启={v["restarts"]} 重复={v["duplicates"]} 超预算={v["over_budget"]} 拒绝={v["rejected"]} silent={v["silent"]}')
print(f'  warnings = {p["warnings"]}')

fails = []
def need(cond, msg):
    if not cond:
        fails.append(msg)

need(st['strokes'] > 300, '笔迹事件太少，投递链路可能没跑通')
need(st['utterances'] > 500, '转写事件太少')
need(st['pages_touched'] > 5, 'page_id 没有真的区分开，"跨节课认出同一页"无从验证')
need(st['audio_chunks'] > 200, '音频分段太少')
need(abs(st['audio_bytes'] - 9_600_000) < 4_000, '音频字节数与 300 秒 16k 单声道 s16le 不符')
need(len(blobs) == st['audio_chunks'], 'blob 文件数与 audio.chunk 事件数不一致')
refs = {r for i in p['track'] for r in i.get('refs', []) if r.endswith('.pcm')}
need(bool(refs) and all(os.path.exists(os.path.join(bdir, r)) for r in refs), 'track 里的 blob 引用找不到实体文件')
need(src['a-fake']['lost_events'] >= 2, '没检测到人为制造的 seq 缺口（skip_seq_at 失效）')
need(src['a-fake']['restarts'] >= 1, '没观察到崩溃重启')
need(src['a-fake']['duplicates'] == 0, '重启后 seq 被误判成重复事件——这是回归，后半节课会整段作废')
need(src['a-audiofile']['over_budget'] > 0, '预算强制从没触发过，等于没测')
need(all(v['rejected'] == 0 for v in src.values()), '有事件解析失败')
need(all(v['silent'] is False for v in src.values()), '有源声明了 produces 却全程无事件')
need(st['writing_while_speaking_ms'] > 0, '墨迹与语音零重叠，时间轴对齐可疑')
# 两条不变量：重叠量不可能超过总书写量；三次重启的三段课堂时间必须首尾相接而不是叠在一起
need(st['writing_while_speaking_ms'] <= st['ink_time_ms'],
     f"重叠 {st['writing_while_speaking_ms']}ms 超过总书写 {st['ink_time_ms']}ms，时间轴有代际叠加")
need(max_t1 > 2_600_000,
     f'track 最大只有 {max_t1}ms：三次重启的三段 sim 时间叠在了同一根轴上，没有按 respawn 边界接起来')
need(any('a-whiteboard' in w for w in p['warnings']), '期望适配器缺失的告警没触发')
need(200 <= st['longest_silence_ms'] <= 6_000,
     f"最长静默 {st['longest_silence_ms']}ms：重启接缝应当是秒级，几百秒说明整段 sim 时间被复制了一份")
# 两个时钟必须分开且自洽：占比的分母是课堂时间轴，不是采集进程墙钟
need(st['wall_ms'] > 0, '没记录采集进程墙钟')
need(0.0 <= st['speech_ratio'] <= 1.05, f"讲话占比 {st['speech_ratio']}，超过 100% 说明分母用错了时钟")
need(any('时间轴' in w and '墙钟' in w for w in p['warnings']),
     '模拟源是 400 倍速的，时间轴远长于墙钟，却没触发双时钟告警')

if fails:
    print('SMOKE FAIL')
    for f in fails:
        print('  -', f)
    sys.exit(1)
print('SMOKE OK')
PY

# 摘要层：看板显示的就是这段文本，所以它先于 UI 被断言。
"$BIN/classagent-client" digest --data "$DATA" --lesson "$LESSON" | tee "$WORK/digest.txt"

python3 - "$WORK/digest.txt" <<'PY'
import sys
t = open(sys.argv[1], encoding='utf-8').read()
fails = []
def has(sub, why):
    if sub not in t:
        fails.append(f'{why}：缺 {sub!r}')

has('初二(3)班 · 数学', '抬头没有班级学科')
has('一、量的分布', '缺第一段')
has('二、逐', '缺时间轴格子段')
has('三、板面轨迹', '缺板面轨迹段')
has('四、采集健康', '缺健康段')
has('五、按本轮采集，以下结论不能下', '缺"不能下什么结论"段——这段比数字更重要')
has('边讲边写', '没有把 overlap 讲成人话')
has('缺口 3 处/丢 6 条', 'a-fake 的缺口没进健康段（每次 spawn 跳 2 号 × 3 次）')
has('重启 2 次', '重启次数没进健康段')
has('没有评估数据', 'eval 缺失时没告诉读者达成度类结论不能做')
has('没有屏幕证据', '关键帧缺失时没说明希沃那路未采')
has('无法回溯', '同上')
if 'SMOKE' in t:
    fails.append('摘要里混进了测试文本')
if len(t) < 800:
    fails.append(f'摘要只有 {len(t)} 字符，明显没渲染完整')
if fails:
    print('DIGEST FAIL')
    for f in fails:
        print('  -', f)
    sys.exit(1)
print('DIGEST OK')
PY

# 开着课却一个数据源都没有：必须几秒内喊出来。安静地产出一节空课是现场最贵的失败。
EMPTY=$WORK/empty-adapters
rm -rf "$EMPTY" /tmp/ci-no-src; mkdir -p "$EMPTY"
"$BIN/classagent-client" run --data /tmp/ci-no-src --adapters "$EMPTY" --lesson examples/lesson.demo.json --max-seconds 8 \
  < /dev/null > "$WORK/no-source.log" 2>&1 || true
if grep -q '仍收到 0 条事件' "$WORK/no-source.log"; then
  echo "OK 空源告警已触发"
else
  echo "FAIL 空源没有告警"; tail -8 "$WORK/no-source.log"; exit 1
fi

# ---------------------------------------------------------------------------
# 看板服务。这里不验"页面好不好看"，验的是接口与边界：
# 路径穿越必须挡、只读模式必须拒写、blob 要按字节原样取回、开写后声明文件真被改。
# ---------------------------------------------------------------------------
PORT=${PORT:-8799}
code() { curl -s --path-as-is -o "$2" -w '%{http_code}' "$1"; }   # code <url> <outfile>

"$BIN/classagent-client" serve --data "$DATA" --adapters "$ADP" --port "$PORT" > "$WORK/serve.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done

HTML=$(code "http://127.0.0.1:$PORT/"                                   "$WORK/page.html")
LESS=$(code "http://127.0.0.1:$PORT/api/lessons"                        "$WORK/lessons.json")
STA=$(code "http://127.0.0.1:$PORT/api/lesson/L-demo-0001/stats"        "$WORK/stats.json")
DIG=$(code "http://127.0.0.1:$PORT/api/lesson/L-demo-0001/digest"       "$WORK/digest2.txt")
ADP_JSON=$(code "http://127.0.0.1:$PORT/api/adapters"                   "$WORK/adapters.json")
MISS=$(code "http://127.0.0.1:$PORT/api/nope"                           "$WORK/nope.json")
BLOB_NAME=$(basename "$(find "$DATA/lessons/L-demo-0001/blobs" -type f | head -1)")
BLOB=$(code "http://127.0.0.1:$PORT/api/lesson/L-demo-0001/blob/$BLOB_NAME" "$WORK/blob.bin")
# 四种类别的穿越尝试：编码斜杠、编码点、裸 ../（--path-as-is 阻止 curl 本地归一）、混合
T1=$(code "http://127.0.0.1:$PORT/api/lesson/..%2F..%2Fetc%2Fpasswd/blob/x"  "$WORK/t1.json")
T2=$(code "http://127.0.0.1:$PORT/api/lesson/L-demo-0001/blob/..%2Fmeta.json" "$WORK/t2.json")
T3=$(code "http://127.0.0.1:$PORT/api/lesson/L-demo-0001/blob/../../meta.json" "$WORK/t3.json")
T4=$(code "http://127.0.0.1:$PORT/api/lesson/L-demo-0001/blob/%2e%2e%2fmeta.json" "$WORK/t4.json")
W1=$(curl -s -X POST -H 'Content-Type: application/json' -d '{"enabled":false}' \
     -o "$WORK/w1.json" -w '%{http_code}' "http://127.0.0.1:$PORT/api/adapter/a-fake.adapter.json")
echo "--- serve 日志 ---"; cat "$WORK/serve.log"
kill $SRV 2>/dev/null || true; trap - EXIT

# 第二个实例：开写。改的是本次 CI 生成的声明副本，不动仓库。
PORT2=$((PORT + 1))
"$BIN/classagent-client" serve --data "$DATA" --adapters "$ADP" --port "$PORT2" --allow-write > "$WORK/serve2.log" 2>&1 &
SRV2=$!
trap 'kill $SRV2 2>/dev/null || true' EXIT
for _ in $(seq 1 20); do
  curl -fsS "http://127.0.0.1:$PORT2/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
W2=$(curl -s -X POST -H 'Content-Type: application/json' -d '{"enabled":false}' \
     -o "$WORK/w2.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/a-fake.adapter.json")
W3=$(curl -s -X POST -H 'Content-Type: application/json' -d '{"enabled":"yes"}' \
     -o "$WORK/w3.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/a-fake.adapter.json")
W4=$(curl -s --path-as-is -X POST -H 'Content-Type: application/json' -d '{"enabled":true}' \
     -o "$WORK/w4.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/../../Cargo.toml")

# --- 写 params：这是本轮新开的口子，也是唯一的安全边界 ---
# 每写一次前留一份字节快照："没被顺手改动"只能靠比对字节断，靠字段名猜是猜不出来的。
cp "$ADP/a-fake.adapter.json" "$WORK/p0.before"
P1=$(curl -s -X POST -H 'Content-Type: application/json' -d '{"params":{"vad":{"rms_open":777}}}' \
     -o "$WORK/p1.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/a-fake.adapter.json")
cp "$ADP/a-fake.adapter.json" "$WORK/p1.before"
# argv 是 RCE 闸门：能改 argv 的写接口不是配置接口
P2=$(curl -s -X POST -H 'Content-Type: application/json' -d '{"argv":["calc.exe"]}' \
     -o "$WORK/p2.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/a-fake.adapter.json")
# 迟滞带写反：适配器会静默钳成 rms_open*0.6，所以必须在写盘前就拒
P3=$(curl -s -X POST -H 'Content-Type: application/json' -d '{"params":{"vad":{"rms_close":900,"rms_open":400}}}' \
     -o "$WORK/p3.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/a-fake.adapter.json")
P4=$(curl -s -X POST -H 'Content-Type: application/json' -d '{"params":"x"}' \
     -o "$WORK/p4.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/a-fake.adapter.json")
P5=$(curl -s -X POST -H 'Content-Type: application/json' -d '{}' \
     -o "$WORK/p5.json" -w '%{http_code}' "http://127.0.0.1:$PORT2/api/adapter/a-fake.adapter.json")
# 磁盘是事实来源：写进去的值必须能从只读接口读回来（观察端就是这么自证"改了没生效"的）
ADP2_JSON=$(code "http://127.0.0.1:$PORT2/api/adapters" "$WORK/adapters2.json")
kill $SRV2 2>/dev/null || true; trap - EXIT

cat > "$WORK/codes.txt" <<EOF
html=$HTML lessons=$LESS stats=$STA digest=$DIG adapters=$ADP_JSON missing=$MISS blob=$BLOB
t1=$T1 t2=$T2 t3=$T3 t4=$T4 post_readonly=$W1 post_write=$W2 post_badbody=$W3 post_traversal=$W4
post_params=$P1 post_argv=$P2 vad_reversed=$P3 params_notobj=$P4 params_empty=$P5 adapters_readback=$ADP2_JSON
EOF

python3 - "$WORK" "$DATA" "$ADP" "$BLOB_NAME" <<'PY'
import json, os, sys
work, data, adp, blob_name = sys.argv[1:5]
codes = dict(tok.split('=', 1) for tok in open(os.path.join(work, 'codes.txt'), encoding='utf-8').read().split() if '=' in tok)
rd = lambda n: open(os.path.join(work, n), encoding='utf-8').read()
rb = lambda n: open(os.path.join(work, n), 'rb').read()
size = lambda p: os.path.getsize(os.path.join(work, p))
fails = []
def need(c, m):
    if not c: fails.append(m)

print('== 服务实测 ==', json.dumps(codes, ensure_ascii=False))

# 正常路径
need(size('page.html') > 3000, '首页太小，前端可能没被 include_str! 编进二进制')
need('课堂观察' in rd('page.html'), '首页缺标题')
# 布局回归守卫：grid 子项不设 min-width:0 会把摘要横向裁掉（真截图时发现的）
need('minmax(0,1fr)' in rd('page.html'), '主栏没锁 minmax(0,1fr)，宽内容会把摘要裁掉')
need('min-width:0' in rd('page.html'), 'section 没设 min-width:0，同上')
# 设置页唯一的控件必须是真按钮：span+onclick 键盘到不了、辅助技术读不出来
need('role="switch"' in rd('page.html'), '数据源开关不是 role=switch，键盘不可达')
need('NaN' not in rd('page.html'), '页面里出现 NaN')
ls = json.loads(rd('lessons.json'))
need(isinstance(ls, list) and len(ls) == 1, f'课程列表应 1 条，实际 {ls}')
need(ls[0]['lesson_id'] == 'L-demo-0001' and ls[0]['class'] == '初二(3)班', '课程元信息（含中文）在 HTTP 链路上坏了')
st = json.loads(rd('stats.json'))
need(st['stats']['strokes'] > 300, 'stats 接口没有笔迹数')
need('a-fake' in st['sources'], 'stats 接口缺源健康表')
d2 = rd('digest2.txt')
for sec in ('一、量的分布', '四、采集健康', '五、按本轮采集，以下结论不能下'):
    need(sec in d2, f'digest 接口缺段：{sec}')
need('NaN' not in d2, '摘要里出现 NaN——某处把字符串拼进了数值参数')
need('静默 00:00' not in d2, '亚秒级静默被 mm:ss 抹成 00:00，读起来像全程没停过')
# 没有关键帧就不该占一列（摘要要在窄窗口里读）
grid = d2.split('二、')[1].split('三、')[0] if '二、' in d2 and '三、' in d2 else ''
need(bool(grid), '摘要缺第二段的格子')
need('帧' not in grid, '本轮没有关键帧，格子行里却还留着"帧"列')
need(max((len(l) for l in grid.splitlines()), default=0) < 78,
     f"格子行最宽 {max((len(l) for l in grid.splitlines()), default=0)} 字符，窄窗口会横向滚动")
ad = json.loads(rd('adapters.json'))
need(any(a.get('id') == 'a-fake' and a.get('enabled') is True for a in ad), 'adapters 接口没报出 a-fake')
blob_disk = os.path.join(data, 'lessons', 'L-demo-0001', 'blobs', blob_name)
need(os.path.getsize(os.path.join(work, 'blob.bin')) == os.path.getsize(blob_disk) > 0,
     'blob 取回字节数与磁盘不一致')
for k in ('html', 'lessons', 'stats', 'digest', 'adapters', 'blob'):
    need(codes.get(k) == '200', f'{k} 接口不是 200：{codes.get(k)}')

# 边界
need(codes['missing'] == '404', '未知接口应 404')
for k in ('t1', 't2', 't3', 't4'):
    need(codes[k] in ('400', '404'), f'路径穿越 {k} 没被挡（{codes[k]}）——这条最要命')
    need('error' in json.loads(rd(f'{k}.json')), f'{k} 没有错误说明')
need(codes['post_readonly'] == '403', '只读模式竟然接受了 POST')
need('allow-write' in rd('w1.json'), '403 没告诉用户怎么开')
need(codes['post_write'] == '200', '开写后 POST 失败')
decl = json.loads(open(os.path.join(adp, 'a-fake.adapter.json'), encoding='utf-8').read())
need(decl.get('enabled') is False, '声明文件里的 enabled 没被真的改写')
need(decl.get('params', {}).get('crash_after_ticks') == 3000, '改写丢了其它字段')
need(codes['post_badbody'] == '400', '非法 body 竟然通过')
need(codes['post_traversal'] in ('400', '404'), f'写接口穿越没被挡（{codes["post_traversal"]}）')
need(os.path.getsize(os.path.join('Cargo.toml')) > 100, 'Cargo.toml 被动过？')

# --- 写 params：能力、边界、以及“只改我改的那一项” ---
decl_path = os.path.join(adp, 'a-fake.adapter.json')
need(codes['post_params'] == '200', f'写 params 没成功（{codes["post_params"]}）：{rd("p1.json")}')
now = json.loads(open(decl_path, encoding='utf-8').read())
before = json.loads(rb('p1.before').decode('utf-8'))
need(now['params']['vad']['rms_open'] == 777, f'只提交一个门限就要只改它一个：{now["params"]}')
need(now['argv'] == before['argv'], f'argv 被改动了：{now["argv"]}')
need(now['enabled'] == before['enabled'], '只提交 params 时不该动 enabled')
for k in ('speed', 'minutes', 'skip_seq_at', 'crash_after_ticks'):
    need(now['params'].get(k) == before['params'].get(k), f'深合并丢了别人的字段：params.{k}')
need('下一节课' in json.loads(rd('p1.json')).get('note', ''),
     '改了参数却不告诉教师什么时候生效，等于让他猜')

# argv 闸门：400 并且磁盘字节一字未动（只拒不写才算闸）
need(codes['post_argv'] == '400', f'写 argv 竟然通过（{codes["post_argv"]}）——这等于本机任意命令执行')
need(rb('p1.before') == open(decl_path, 'rb').read(), '被拒的写请求竟然动了磁盘')
need('只能改 enabled/params' in rd('p2.json'), f'400 得说清能改什么：{rd("p2.json")}')

need(codes['vad_reversed'] == '400', 'rms_close >= rms_open 会被适配器静默钳制，必须写盘前就拒')
need(codes['params_notobj'] == '400', 'params 不是对象竟然通过')
need(codes['params_empty'] == '400', '空对象不该被当成一次成功的写入')
need(rb('p1.before') == open(decl_path, 'rb').read(), '被拒的写请求把声明文件改坏了')

# 磁盘是事实来源：写完能从只读接口读回同一个值
ad2 = json.loads(rd('adapters2.json'))
fake2 = [a for a in ad2 if a.get('id') == 'a-fake']
need(bool(fake2), '写过的源在读接口里看不到了')
need(fake2[0]['params']['vad']['rms_open'] == 777, '读接口没反映磁盘上的新值')
need(codes['adapters_readback'] == '200', '写完后读接口不是 200')

if fails:
    print('SERVE FAIL')
    for f in fails: print('  -', f)
    sys.exit(1)
print('SERVE OK')
PY

# ---------------------------------------------------------------------------
# 开课重读声明。看板的写接口只能改文件（serve 与 run 是两个进程、中间没有 IPC），
# 采集用的却是内存里的 spec —— 两者靠 start_lesson 里的 reload_params 接上。
# 这一段钉的就是这根接头：同一个进程内 stop → 改文件 → start，下一节课的行为
# 必须跟着新参数走。拿旧 spec 的话，"改完参数"要重启客户端才生效，没人会知道。
# 不用 a-audio：CI runner 没麦克风。a-audiofile 的 chunk_ms 是个数得出来的参数。
# ---------------------------------------------------------------------------
ADP2=$WORK/adapters-reload
RDATA=$WORK/data-reload
RLOG=$WORK/reload.log
rm -rf "$ADP2" "$RDATA"; rm -f "$RLOG"; mkdir -p "$ADP2" "$RDATA"
python3 - "$ADP2" "$TONE" <<'PY'
import json, sys
adp, tone = sys.argv[1], sys.argv[2]
af = json.load(open('adapters.d/a-audiofile.adapter.json', encoding='utf-8'))
af['enabled'] = True
af['params'].update({'path': tone.replace('\\', '/'), 'speed': 400, 'chunk_ms': 1000})
json.dump(af, open(f'{adp}/a-audiofile.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
PY
{
  sleep 2
  echo stop
  # 等核心自己那句"收课"落到日志里再改文件（适配器的同名日志会先一步出现，所以带上 [client]）：
  # 改早了撞上上一节的收尾，改晚了撞上下一节的开课。
  for _ in $(seq 1 80); do
    grep -q '\[client\] 收课 L-demo-0001' "$RLOG" 2>/dev/null && break
    sleep 0.25
  done
  python3 - "$ADP2" <<'PY'
import json, sys
p = f'{sys.argv[1]}/a-audiofile.adapter.json'
d = json.load(open(p, encoding='utf-8'))
d['params']['chunk_ms'] = 250   # 段长除以 4，段数就乘以 4：这个变化在导出里数得出来
json.dump(d, open(p, 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
PY
  echo 'start {"lesson_id":"L-reload-2"}'
  sleep 8
  echo quit
} | "$CLIENT" run --data "$RDATA" --adapters "$ADP2" --lesson examples/lesson.demo.json \
    --max-seconds 60 > "$RLOG" 2>&1
grep -q '已按最新声明重新配置' "$RLOG" || { echo "FAIL 开课时没重读声明"; tail -25 "$RLOG"; exit 1; }
for LESSON_ID in L-demo-0001 L-reload-2; do
  "$CLIENT" export --data "$RDATA" --lesson "$LESSON_ID" > "$WORK/export-$LESSON_ID.log" 2>&1 \
    || { echo "FAIL 导出 $LESSON_ID 失败"; tail -25 "$RLOG"; exit 1; }
done

tail -12 "$RLOG"

python3 - "$RDATA" <<'PY'
import json, os, sys
data = sys.argv[1]
fails = []
def need(c, m):
    if not c: fails.append(m)

def read(lesson):
    path = os.path.join(data, 'lessons', lesson, 'events.ndjson')
    if not os.path.exists(path):
        return []
    return [json.loads(l) for l in open(path, encoding='utf-8') if l.strip()]

ck = lambda evs: [e['envelope']['payload'] for e in evs if e['envelope']['kind'] == 'audio.chunk']
e1, e2 = read('L-demo-0001'), read('L-reload-2')
k1, k2 = ck(e1), ck(e2)
need(bool(k1) and bool(k2), f'两节课都得有音频分段：{len(k1)} / {len(k2)}')
if fails:
    print('RELOAD FAIL')
    for f in fails:
        print('  -', f)
    sys.exit(1)

# 段长是唯一能分辨"这一节确实是用新参数采的"的证据：光看段数变多，可能是时序凑巧。
# s16le 单声道 16k 下 chunk_ms=1000 → 32000 字节，250 → 8000 字节。
s1 = {p['len'] for p in k1}
s2 = {p['len'] for p in k2}
print(f'  第一节 {len(k1)} 段，段长字节={sorted(s1)}；第二节 {len(k2)} 段，段长字节={sorted(s2)}')
need(s1 == {32000}, f'第一节每段都该是 32000 字节（chunk_ms=1000）：{sorted(s1)}')
need(s2 == {8000}, f'第二节每段都该是 8000 字节（chunk_ms=250）：开课时没重读声明？{sorted(s2)}')
need(len(k2) >= len(k1) * 3, f'段数没随 chunk_ms 变小而变多：{len(k1)} → {len(k2)}')

# 重读只换参数，不该弄乱 seq 空间，也不该把上一节课的扫到课外。
for lesson in ('L-demo-0001', 'L-reload-2'):
    p = json.load(open(os.path.join(data, 'lessons', lesson, 'ai_payload.json'), encoding='utf-8'))
    src = p['sources'].get('a-audiofile')
    need(bool(src), f'{lesson} 健康表里没有 a-audiofile')
    if not src:
        continue
    need(src['duplicates'] == 0, f"{lesson} 出现 {src['duplicates']} 条重复事件：重读不该动 seq 空间")
    need(src['gaps'] == 0, f"{lesson} 出现 {src['gaps']} 处缺口")
    need(src['close'] and src['close']['chunks'] > 0, f"{lesson} 的收课统计丢了：{src['close']}")
    c = [e for e in read(lesson) if e['envelope']['kind'] == 'session.close']
    need(len(c) == 1, f'{lesson} 应当只有一条 session.close（重读不是重启），实得 {len(c)}')

if os.path.exists(os.path.join(data, 'misc.ndjson')):
    need('a-audiofile' not in open(os.path.join(data, 'misc.ndjson'), encoding='utf-8').read(),
         '第二节课的事件流到了课外面')

if fails:
    print('RELOAD FAIL')
    for f in fails:
        print('  -', f)
    sys.exit(1)
print('RELOAD OK')
PY
