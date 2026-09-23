#!/usr/bin/env bash
# 录音适配器（a-audio）端到端守卫，Linux 与 Windows 两个 runner 各跑一遍同一份。
#
# runner 上没有麦克风，所以断言分两层，各断各的事：
# 1. 单元层（cargo test --workspace 已经跑过）：VAD 切分、WAV 读写、混单声道——
#    这些是纯函数，合成信号就能钉死，不需要声卡。
# 2. 集成层（本脚本）：用 Python 的 wave 模块生成一段"有讲话有停顿有关门声"的录音，
#    让 a-audio 走 fixture 回放，断真事件、真 blob、真导出。用 wave 而不是自研写头，
#    是为了让适配器解析一个第三方写出来的文件——现场拿到的录音笔文件就是这种。
#
# 反向断言同样重要：把门限调到不可能越过，这节课必须"一段都没有"且被核心标成 silent。
# "没采到"如果能被伪装成"采到了"，整条观察链就是假的。
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT) EXE=.exe ;;
  *) EXE= ;;
esac
export PYTHONIOENCODING=utf-8
echo "platform=$(uname -s)  exe='${EXE:-无}'"

BIN=${BIN:-target/debug}
WORK=${AUDIO_DIR:-.ci-audio}
LESSON=L-demo-0001
CLIENT="$BIN/classagent-client$EXE"
ADAPTER="$BIN/a-audio$EXE"
[ -f "$CLIENT" ] || { echo "FAIL 找不到 $CLIENT"; exit 1; }
[ -f "$ADAPTER" ] || { echo "FAIL 找不到 $ADAPTER"; exit 1; }

rm -rf "$WORK"
mkdir -p "$WORK/adp" "$WORK/adp-quiet" "$WORK/adp-nodev" "$WORK/data" "$WORK/data-quiet" "$WORK/data-nodev"

# ---- 造一节课的录音：讲话—停顿—讲话—关门声—安静 ----
# 结构（秒）：1.0 静音 / 2.0 讲话 / 1.5 停顿 / 1.2 讲话 / 0.5 静音 / 0.06 关门声 / 2.0 静音
python3 - "$WORK/lesson.wav" <<'PY'
import array, math, sys, wave
sr = 16000
def tone(ms, amp, freq):
    return array.array('h', (int(amp * math.sin(2 * math.pi * freq * i / sr)) for i in range(sr * ms // 1000)))
def noise(ms, amp=12):
    # 真实教室有噪声底；纯 0 会让"过得了门限"这件事被高估。
    return array.array('h', ((i % 7) * amp - amp * 3 for i in range(sr * ms // 1000)))
stream = array.array('h')
for part in (noise(1000), tone(2000, 9000, 220), noise(1500), tone(1200, 8000, 180),
             noise(500), tone(60, 15000, 90), noise(2000)):
    stream.extend(part)
with wave.open(sys.argv[1], 'wb') as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr)
    w.writeframes(stream.tobytes())
print('fixture samples =', len(stream), '= %.2fs 课堂时间轴' % (len(stream) / sr))
PY

# 三套声明：正常门限、不可能越过的门限、以及真去开设备（runner 上没麦克风）。
python3 - "$WORK" <<'PY'
import json, os, sys
work = sys.argv[1]
src = json.load(open('adapters.d/a-audio.adapter.json', encoding='utf-8'))
src['enabled'] = True
src['argv'] = ['$TARGET_DIR/a-audio']

loud = json.loads(json.dumps(src))
loud['params'].update({'source': 'fixture', 'fixture': f'{work}/lesson.wav', 'speed': 60, 'emit_silence': False})
json.dump(loud, open(f'{work}/adp/a-audio.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)

quiet = json.loads(json.dumps(src))
quiet['params'].update({
    'source': 'fixture', 'fixture': f'{work}/lesson.wav', 'speed': 60, 'emit_silence': False,
    'vad': dict(src['params']['vad'], rms_open=30000.0, rms_close=29000.0),
})
json.dump(quiet, open(f'{work}/adp-quiet/a-audio.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)

# 不设 fixture：走真设备路径。CI runner 上没有声卡，期望是"一条事件都不发"，
# 从而被核心标成 silent；同时进程不能 panic，也不能伪造数据。
nodev = json.loads(json.dumps(src))
nodev['params'].pop('fixture', None)
nodev['params'].update({'source': 'device'})
json.dump(nodev, open(f'{work}/adp-nodev/a-audio.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
PY

echo "--- ① 正常门限：应切出话轮 ---"
"$CLIENT" run --data "$WORK/data" --adapters "$WORK/adp" --lesson examples/lesson.demo.json --max-seconds 8 \
  < /dev/null > "$WORK/run1.log" 2>&1 || true
tail -6 "$WORK/run1.log"
"$CLIENT" export --data "$WORK/data" --lesson "$LESSON" > "$WORK/export1.log" 2>&1 \
  || { echo "FAIL export 失败"; tail -20 "$WORK/run1.log"; exit 1; }

echo "--- ② 门限不可能越过：应一段都没有 ---"
"$CLIENT" run --data "$WORK/data-quiet" --adapters "$WORK/adp-quiet" --lesson examples/lesson.demo.json --max-seconds 8 \
  < /dev/null > "$WORK/run2.log" 2>&1 || true
"$CLIENT" export --data "$WORK/data-quiet" --lesson "$LESSON" > "$WORK/export2.log" 2>&1 || true

echo "--- ③ 真去开设备（runner 无声卡）：应一条事件不发并被标成 silent ---"
"$CLIENT" run --data "$WORK/data-nodev" --adapters "$WORK/adp-nodev" --lesson examples/lesson.demo.json --max-seconds 6 \
  < /dev/null > "$WORK/run3.log" 2>&1 || true
"$CLIENT" export --data "$WORK/data-nodev" --lesson "$LESSON" > "$WORK/export3.log" 2>&1 || true

python3 - "$WORK" <<'PY'
import json, os, sys, wave
work = sys.argv[1]

def need(cond, msg):
    if not cond:
        print('FAIL', msg)
        sys.exit(1)

def read_events(data):
    path = os.path.join(data, 'lessons', 'L-demo-0001', 'events.ndjson')
    need(os.path.exists(path), f'没有事件文件 {path}')
    out = []
    for line in open(path, encoding='utf-8'):
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out

def chunks_of(events):
    return [e for e in events if e['envelope']['kind'] == 'audio.chunk']

# ---------- ① 正向 ----------
ev = read_events(os.path.join(work, 'data'))
ck = chunks_of(ev)
need(len(ck) == 2, f'应该正好切出 2 个话轮（2.0s 与 1.2s 两段讲话），实得 {len(ck)}')

blob_dir = os.path.join(work, 'data', 'lessons', 'L-demo-0001', 'blobs')
total = 0
prev_t0 = -1
for i, e in enumerate(ck):
    p = e['envelope']['payload']
    name = p['blob']
    need(name.endswith('.wav'), f'blob 应是 wav，实为 {name}')
    need(p['codec'] == 'pcm_s16le', f'codec 要如实写 pcm_s16le：{p["codec"]}')
    need(p['sample_rate'] == 16000, f'采样率必须是文件真实值：{p["sample_rate"]}')
    need(p['channels'] == 1, f'混成单声道了吗：{p["channels"]}')
    need(p['t0_ms'] > prev_t0, f'分段起点必须严格递增：{prev_t0} -> {p["t0_ms"]}')
    prev_t0 = p['t0_ms']
    need(p['trigger'] == 'vad', '分段要说明触发原因是 VAD')
    need(p['rms'] > 500, f'落盘的段应该真的响过：rms={p["rms"]}')
    need(p['speech_ms'] >= 250, f'短促噪音不该被当成话轮：{p["speech_ms"]}ms')

    path = os.path.join(blob_dir, name)
    need(os.path.exists(path), f'事件报了引用却没有文件：{name}')
    # 用第三方库读回来：证明这些 blob 不是"只有我们自己能解开"的私有格式。
    with wave.open(path) as w:
        frames = w.readframes(w.getnframes())
        need(w.getnchannels() == 1, 'blob 必须是单声道')
        need(w.getframerate() == 16000, f'blob 采样率：{w.getframerate()}')
        need(w.getsampwidth() == 2, 'blob 必须是 16 bit')
        need(len(frames) + 44 == p['len'], f'len 要等于头+数据：{len(frames)}+44 vs {p["len"]}')
    total += p['len']
    dur = len(frames) / 2 / 16000 * 1000
    need(abs(dur - p['dur_ms']) <= 40, f'dur_ms 要和文件真实时长一致：{p["dur_ms"]} vs {dur:.0f}')

need(ck[0]['envelope']['payload']['t0_ms'] <= 1000, '起音前的预滚不该把话轮推到 1s 之后')
need(ck[0]['envelope']['payload']['dur_ms'] >= 2000, f'第一段应覆盖 2s 讲话：{ck[0]["envelope"]["payload"]["dur_ms"]}')
need(ck[1]['envelope']['payload']['dur_ms'] >= 1200, f'第二段应覆盖 1.2s 讲话：{ck[1]["envelope"]["payload"]["dur_ms"]}')

close = [e for e in ev if e['envelope']['kind'] == 'session.close' and e['adapter_id'] == 'a-audio']
need(len(close) == 1, f'一节课应该只有一条收课记录，实得 {len(close)}')
cp = close[0]['envelope']['payload']
need(cp['chunks'] == 2, f'收课统计与事件数要一致：{cp["chunks"]}')
need(cp['bytes'] == total, f'字节统计要等于 blob 之和：{cp["bytes"]} vs {total}')
need(cp.get('error') is None, f'正向路径不该带错误：{cp.get("error")}')
need(cp['dropped_short'] == 1, f'那记 60ms 关门声要被丢弃并计数：{cp["dropped_short"]}')
need(2900 <= cp['voiced_ms'] <= 3600, f'语音总时长应接近 3.2s：{cp["voiced_ms"]}')
# 采到的音频时长与进程墙钟是两件事：回放加速了，两者不该被混用。
need(cp['audio_ms'] > cp['wall_ms'], f'fixture 是加速回放的，audio_ms 必然远大于 wall_ms：{cp["audio_ms"]} vs {cp["wall_ms"]}')

aud = [e for e in ev if e['adapter_id'] == 'a-audio']
bad = [e for e in aud if e['status'] not in ('accepted',)]
need(not bad, f'a-audio 的事件必须全部被接受，实有 {[(b["status"], b.get("raw")) for b in bad]}')

payload = json.load(open(os.path.join(work, 'data', 'lessons', 'L-demo-0001', 'ai_payload.json'), encoding='utf-8'))
st = payload['stats']
need(st['audio_chunks'] == 2, f'导出载荷里要看得见 2 段音频：{st["audio_chunks"]}')
need(st['audio_bytes'] == total, f'导出字节数：{st["audio_bytes"]} vs {total}')
health = payload['sources']  # HashMap：按 adapter_id 索引的对象
need(health['a-audio']['accepted'] >= 4, 'a-audio 的 accepted 计数要包含 open/chunks/close')
need(health['a-audio']['silent'] is False, '产出了音频的源不该被标 silent')
need(health['a-audio']['gaps'] == 0, 'seq 不该有洞')
need(health['a-audio']['over_budget'] == 0, f'话轮级事件不该超预算：{health["a-audio"]["over_budget"]}')
track = payload['track']
audio_rows = [t for t in track if t['kind'] == 'audio.chunk']
need(len(audio_rows) == 2, f'时间轴上要有两条音频记录：{len(audio_rows)}')
need(all(t['refs'] for t in audio_rows), '时间轴上的音频记录必须带 blob 引用，否则人无法回听')

# ---------- ② 门限之上没信号：源活着，但没有话轮 ----------
ev2 = read_events(os.path.join(work, 'data-quiet'))
ck2 = chunks_of(ev2)
need(not ck2, f'门限之上没有信号，一段都不该产出，实得 {len(ck2)}')
need(not [f for f in os.listdir(os.path.join(work, 'data-quiet', 'lessons', 'L-demo-0001', 'blobs'))
          if f.endswith('.wav')], '没有话轮就不该留下 wav')
c2 = [e for e in ev2 if e['envelope']['kind'] == 'session.close' and e['adapter_id'] == 'a-audio']
need(len(c2) == 1, '源正常起止了，就该留下一条收课记录')
need(c2[0]['envelope']['payload']['chunks'] == 0, '收课统计必须是 0')
p2 = json.load(open(os.path.join(work, 'data-quiet', 'lessons', 'L-demo-0001', 'ai_payload.json'), encoding='utf-8'))
need(p2['stats']['audio_chunks'] == 0, '统计里也要是 0')
# 它确实跑起来了（open/close 都在），所以不该被标 silent——"源活着但没采到话轮"
# 和"源根本没起来"是两种不同的故障，健康表不能把它们糊在一起。
need(p2['sources']['a-audio']['silent'] is False,
     '开过场的源不该被标 silent，否则真没麦克风的机器会被误判成同一类')

# ---------- ③ 本机没麦克风：一条事件都不发，让 silent 说话 ----------
ev3 = read_events(os.path.join(work, 'data-nodev'))
mine = [e for e in ev3 if e['adapter_id'] == 'a-audio']
need(not mine, f'无设备时不该留任何事件（包含 open/close），实得 {len(mine)}')
p3 = json.load(open(os.path.join(work, 'data-nodev', 'lessons', 'L-demo-0001', 'ai_payload.json'), encoding='utf-8'))
h3 = p3['sources']
need(h3['a-audio']['silent'] is True, '声明了 produces 却零事件，必须被标成 silent——这是现场排障的第一信号')
need(p3['stats']['audio_chunks'] == 0, '无设备时导出的音频统计必须是 0')
log3 = open(os.path.join(work, 'run3.log'), encoding='utf-8').read()
need('无法开始采集' in log3, '适配器的失败原因要能在日志里看到，而不是只剩一个 silent')

print('AUDIO OK  2 个话轮 / %d 字节 / 丢弃 1 记短促噪音 / 无声卡已标 silent' % total)
PY
