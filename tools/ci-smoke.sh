#!/usr/bin/env bash
# 端到端守卫。本机磁盘不允许跑构建，所以这些断言只在 CI 上执行；
# 但它们断的是真行为：投递、seq 缺口、崩溃重启后的 seq 空间、blob 约定、
# 预算强制、时间轴对齐。任何一条红掉都比"能编译"有用。
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=${BIN:-./target/debug}
DATA=${DATA:-/tmp/ci-data}
ADP=${ADP:-/tmp/ci-adapters}
TONE=${TONE:-/tmp/ci-tone.raw}
LESSON=L-demo-0001
LOG=/tmp/ci-core.log

rm -rf "$DATA" "$ADP" "$TONE"
mkdir -p "$ADP"

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

# 为本次 CI 生成一套适配器装载声明：真实音频文件路径 + 让 a-fake 制造缺口并崩溃一次。
python3 - "$ADP" "$TONE" <<'PY'
import json, sys
adp, tone = sys.argv[1], sys.argv[2]
fake = json.load(open('adapters.d/a-fake.adapter.json'))
fake['enabled'] = True
fake['params'].update({'speed': 400, 'minutes': 45, 'skip_seq_at': [500, 501], 'crash_after_ticks': 3000})
json.dump(fake, open(f'{adp}/a-fake.adapter.json', 'w'), ensure_ascii=False, indent=2)
af = json.load(open('adapters.d/a-audiofile.adapter.json'))
af['enabled'] = True
af['params'].update({'path': tone, 'speed': 400})
json.dump(af, open(f'{adp}/a-audiofile.adapter.json', 'w'), ensure_ascii=False, indent=2)
PY

set +e
"$BIN/classagent-core" run --data "$DATA" --adapters "$ADP" --lesson examples/lesson.demo.json --max-seconds 15 \
  < /dev/null > "$LOG" 2>&1
RC=$?
set -e
echo "--- core 日志尾部 ---"
tail -25 "$LOG"
[ "$RC" = 0 ] || { echo "FAIL core 退出码 $RC"; exit 1; }
grep -q '退避后重启' "$LOG" || { echo "FAIL 没有观察到适配器重启"; exit 1; }

"$BIN/classagent-core" export --data "$DATA" --lesson "$LESSON" || { echo "FAIL 导出失败"; exit 1; }
"$BIN/classagent-core" status --data "$DATA"

python3 - "$DATA" "$LESSON" <<'PY'
import json, os, sys
data, lesson = sys.argv[1], sys.argv[2]
ldir = os.path.join(data, 'lessons', lesson)
bdir = os.path.join(ldir, 'blobs')
p = json.load(open(os.path.join(ldir, 'ai_payload.json')))
st, src = p['stats'], p['sources']
blobs = os.listdir(bdir)
lines = sum(1 for _ in open(os.path.join(ldir, 'events.ndjson')))

print('== 实测 ==')
for k in ('strokes', 'utterances', 'erases', 'pages_touched', 'audio_chunks', 'audio_bytes',
          'ink_time_ms', 'writing_while_speaking_ms', 'longest_silence_ms', 'duration_ms'):
    print(f'  {k} = {st[k]}')
print(f'  events.ndjson 行数 = {lines}, track 条目 = {len(p["track"])}, blob 文件 = {len(blobs)}')
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
need(refs and all(os.path.exists(os.path.join(bdir, r)) for r in refs), 'track 里的 blob 引用找不到实体文件')
need(src['a-fake']['lost_events'] >= 2, '没检测到人为制造的 seq 缺口（skip_seq_at 失效）')
need(src['a-fake']['restarts'] >= 1, '没观察到崩溃重启')
need(src['a-fake']['duplicates'] == 0, '重启后 seq 被误判成重复事件——这是回归，后半节课会整段作废')
need(src['a-audiofile']['over_budget'] > 0, '预算强制从没触发过，等于没测')
need(all(v['rejected'] == 0 for v in src.values()), '有事件解析失败')
need(all(v['silent'] is False for v in src.values()), '有源声明了 produces 却全程无事件')
need(st['writing_while_speaking_ms'] > 0, '墨迹与语音零重叠，时间轴对齐可疑')
need(any('a-whiteboard' in w for w in p['warnings']), '期望适配器缺失的告警没触发')
need(200 <= st['longest_silence_ms'] <= 1000, f"最长静默 {st['longest_silence_ms']}ms 与模拟节奏(600ms)不符")

if fails:
    print('SMOKE FAIL')
    for f in fails:
        print('  -', f)
    sys.exit(1)
print('SMOKE OK')
PY
