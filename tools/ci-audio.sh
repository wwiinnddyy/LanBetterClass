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
# 反向断言同样重要：把门限调到不可能越过，这节课必须"一段都没有"，同时留下一次正常开场。
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
mkdir -p "$WORK/adp" "$WORK/adp-quiet" "$WORK/adp-nodev" "$WORK/adp-cutoff" \
         "$WORK/data" "$WORK/data-quiet" "$WORK/data-nodev" "$WORK/data-cutoff"

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

# 不设 fixture：走真设备路径。CI runner 上没有声卡，期望是"一条事件都不发"——
# 一个全程没发事件的源不会出现在健康表里；同时进程不能 panic，也不能伪造数据。
nodev = json.loads(json.dumps(src))
nodev['params'].pop('fixture', None)
nodev['params'].update({'source': 'device'})
json.dump(nodev, open(f'{work}/adp-nodev/a-audio.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)

# 课到一半被叫停（提前下课 / 断电前最后一手）：不限速，5 秒 wall = 5 秒课堂时间轴，
# 正好切在第二段讲话（4.5s~5.7s）中间——这一段的 blob 与 session.close 只可能在
# StopLesson 之后才发出来，所以它是对"收尾时序"最直接的守卫：客户端一旦提前停止
# 读管道，这两条就永远落不了盘（真麦克风上踩过）。
cut = json.loads(json.dumps(src))
cut['params'].update({'source': 'fixture', 'fixture': f'{work}/lesson.wav', 'speed': 1, 'emit_silence': False})
json.dump(cut, open(f'{work}/adp-cutoff/a-audio.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
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

echo "--- ③ 真去开设备（runner 无声卡）：应一条事件都不发 ---"
"$CLIENT" run --data "$WORK/data-nodev" --adapters "$WORK/adp-nodev" --lesson examples/lesson.demo.json --max-seconds 6 \
  < /dev/null > "$WORK/run3.log" 2>&1 || true
"$CLIENT" export --data "$WORK/data-nodev" --lesson "$LESSON" > "$WORK/export3.log" 2>&1 || true

echo "--- ④ 课到一半下课：尾段与 session.close 是 StopLesson 之后才发的 ---"
"$CLIENT" run --data "$WORK/data-cutoff" --adapters "$WORK/adp-cutoff" --lesson examples/lesson.demo.json --max-seconds 5 \
  < /dev/null > "$WORK/run4.log" 2>&1 || true
"$CLIENT" export --data "$WORK/data-cutoff" --lesson "$LESSON" > "$WORK/export4.log" 2>&1 || true

echo "--- ⑤ 教师改门限：写盘后下一节课真能读到 ---"
# 这是教师真实走的那条路：看板进程里 POST 改声明 → 杀掉看板 → 下一节课用同一个目录开课。
# 它钉的是 2.1（写盘与深合并）加上"磁盘 params → Configure → 适配器自述 → 导出"整条链；
# "同一个进程里 stop → 改 → start 也要生效"那一半由 ci-smoke.sh 的 RELOAD 段负责。
ADP5="$WORK/adp-reload"
DATA5="$WORK/data-reload"
rm -rf "$ADP5" "$DATA5"; mkdir -p "$ADP5" "$DATA5"
cp "$WORK/adp/a-audio.adapter.json" "$ADP5/a-audio.adapter.json"
cp "$WORK/adp/a-audio.adapter.json" "$WORK/adp5.before"
PORT5=${PORT5:-8797}
"$CLIENT" serve --data "$DATA5" --adapters "$ADP5" --port "$PORT5" --allow-write > "$WORK/serve5.log" 2>&1 &
SRV5=$!
trap 'kill $SRV5 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$PORT5/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
# 只提交 vad：其它参数（source/fixture/speed）必须原样留在磁盘上，否则这节课根本不会回放那份 wav
HTTP5=$(curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"params":{"vad":{"rms_open":30000.0,"rms_close":29000.0}}}' \
  -o "$WORK/p5.json" -w '%{http_code}' "http://127.0.0.1:$PORT5/api/adapter/a-audio.adapter.json")
echo "$HTTP5" > "$WORK/p5.code"
kill $SRV5 2>/dev/null || true; trap - EXIT
"$CLIENT" run --data "$DATA5" --adapters "$ADP5" --lesson examples/lesson.demo.json --max-seconds 8 \
  < /dev/null > "$WORK/run5.log" 2>&1 || true
"$CLIENT" export --data "$DATA5" --lesson "$LESSON" > "$WORK/export5.log" 2>&1 || true

echo "--- ⑥ 回听链路：blob 的 MIME 与字节 ---"
# 观察端是拿 <audio src> 直连这个接口回听的（桌面进程不做二进制中转），
# 所以 content_type 与字节完整性就是它能出声的全部条件。
PORT6=${PORT6:-8798}
WAV_NAME=$(basename "$(find "$WORK/data/lessons/$LESSON/blobs" -name '*.wav' | head -1)")
"$CLIENT" serve --data "$WORK/data" --adapters "$WORK/adp" --port "$PORT6" > "$WORK/serve6.log" 2>&1 &
SRV6=$!
trap 'kill $SRV6 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$PORT6/api/health" >/dev/null 2>&1 && break
  sleep 0.25
done
BLOB_CT=$(curl -s -o "$WORK/blob.wav" -w '%{content_type}' \
  "http://127.0.0.1:$PORT6/api/lesson/$LESSON/blob/$WAV_NAME")
echo "$BLOB_CT" > "$WORK/blob.ctype"
BLOB_N=$(curl -s -o /dev/null -w '%{http_code}' \
  "http://127.0.0.1:$PORT6/api/lesson/$LESSON/blob/..%2Fmeta.json")
echo "$BLOB_N" > "$WORK/blob.traversal"
kill $SRV6 2>/dev/null || true; trap - EXIT

python3 - "$WORK" "$WAV_NAME" <<'PY'
import json, os, sys, wave
work, wav_name = sys.argv[1], sys.argv[2]

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
# 回放路径没有真流，掉帧次数必须是 0；这个字段的存在本身也是契约——设备路径靠它把
# "这节课的时长结论能不能下"暴露给课后复盘，而不是只在 stderr 里响一声。
need(cp['stream_errors'] == 0, f'fixture 路径不该有流错误：{cp["stream_errors"]}')
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

# ---------- ①b 音频事实要能被看见：导出里的 close / vad / detail ----------
# 这三个字段是观察端与摘要的全部依据。它们缺一个，教师看到的就只剩"采到了点什么"。
c1 = health['a-audio'].get('close')
need(isinstance(c1, dict), '收课统计没进健康表：那它就还只是 track 里一条被截断的"未识别事件"')
if isinstance(c1, dict):
    need(c1['closes'] == 1, f'一节课一条收课记录：{c1["closes"]}')
    need(c1['chunks'] == cp['chunks'] == 2, f'导出里的段数要等于适配器自报的：{c1["chunks"]} vs {cp["chunks"]}')
    need(c1['bytes'] == cp['bytes'] == total, f'导出里的字节要等于 blob 之和：{c1["bytes"]} vs {total}')
    # 语音时长只有一个来源：适配器自己报的。服务端重算一遍就是第二份会漂移的算法。
    need(c1['voiced_ms'] == cp['voiced_ms'], f'语音时长被算了两遍：{c1["voiced_ms"]} vs {cp["voiced_ms"]}')
    need(c1['dropped_short'] == cp['dropped_short'] == 1, f'短促丢弃次数没透出：{c1["dropped_short"]}')
    need(c1['stream_errors'] == 0, f'fixture 路径不该有掉帧：{c1["stream_errors"]}')
    need(c1['audio_ms'] == cp['audio_ms'] and c1['wall_ms'] == cp['wall_ms'],
         f'两个时钟必须原样透出：{c1["audio_ms"]}/{cp["audio_ms"]}，{c1["wall_ms"]}/{cp["wall_ms"]}')
    need(c1['error'] is None, f'正向路径不该带错误：{c1["error"]}')
    need(payload['stats']['stream_errors'] == 0, '掉帧总量要和各源一致')

v1 = health['a-audio'].get('vad')
need(isinstance(v1, dict), f'session.open 的自述没进健康表：{v1}')
if isinstance(v1, dict):
    need(v1['rms_open'] == 500.0, f'本节课生效的门限要看得见（"磁盘声明 vs 实际生效"右边那栏）：{v1}')
    need(v1['rms_close'] == 300.0 and v1['min_speech_ms'] == 250 and v1['max_segment_ms'] == 8000, f'门限快照不全：{v1}')
    need(v1['input'] == 'fixture' and v1['sample_rate'] == 16000, f'来路与真实采样率要在：{v1}')

need(all(t.get('detail', {}).get('rms', 0) > 500 for t in audio_rows), '每段都要带 rms，否则电平横条画不出来')
need(all('peak' in t.get('detail', {}) and 'speech_ms' in t.get('detail', {}) for t in audio_rows),
     f'peak/speech_ms 是判断"这段是不是真话轮"的根据：{[t.get("detail") for t in audio_rows]}')
need(payload['stats']['audio_speech_ms'] == sum(t['detail']['speech_ms'] for t in audio_rows),
     '摘要里的语音时长必须等于逐段之和，不能有两个数')
need(not any('未识别事件' in t['text'] for t in track),
     f'出现了未识别事件，新 kind 没被接住：{[t["text"] for t in track if "未识别" in t["text"]]}')
need(not any(t['kind'] == 'session.close' for t in track),
     '收课统计进了 track：它是关于采集过程本身的事实，不是课堂上发生的事，还会把 duration_ms 顶成进程墙钟')

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
# 这条正面钉死"磁盘 params → Configure → 适配器自述 → 导出"：② 套把 rms_open 改成了 30000，
# 导出里看到的就得是 30000。门限没传下去时它仍然是 ① 套的 500，两套房同一份代码跑不出这个差异。
v2 = p2['sources']['a-audio'].get('vad')
need(isinstance(v2, dict) and v2['rms_open'] == 30000.0, f'② 套写的是 30000，导出的却是：{v2}')
need(p2['sources']['a-audio'].get('close', {}).get('chunks') == 0, '源活着但没采到话轮：收课统计得是 0 段')
# 它确实跑起来了（open/close 都在），所以不该被标 silent——"源活着但没采到话轮"
# 和"源根本没起来"是两种不同的故障，健康表不能把它们糊在一起。
need(p2['sources']['a-audio']['silent'] is False,
     '开过场的源不该被标 silent，否则真没麦克风的机器会被误判成同一类')

# ---------- ③ 本机没麦克风：一条事件都不发 ----------
ev3 = read_events(os.path.join(work, 'data-nodev'))
mine = [e for e in ev3 if e['adapter_id'] == 'a-audio']
need(not mine, f'无设备时不该留任何事件（包含 open/close），实得 {len(mine)}')
p3 = json.load(open(os.path.join(work, 'data-nodev', 'lessons', 'L-demo-0001', 'ai_payload.json'), encoding='utf-8'))
# 健康表是按"有事件"建的键：一个全程没发过事件的源会直接不出现在表里。
# silent 留给另一种情：发得出事件但全被拒收/超预算。两者别糊成一个断言。
need('a-audio' not in p3['sources'] or p3['sources']['a-audio']['silent'] is True,
     f'无设备时这个源要么不出现、要么被标 silent，实得 {p3["sources"].get("a-audio")}')
need(p3['stats']['audio_chunks'] == 0, '无设备时导出的音频统计必须是 0')
log3 = open(os.path.join(work, 'run3.log'), encoding='utf-8').read()
need('无法开始采集' in log3, '适配器的失败原因要能在日志里看到，而不是只表现为“缺一个源”')

# ---------- ④ 中途下课：收尾的那两条必须还在同一节课里 ----------
ev4 = read_events(os.path.join(work, 'data-cutoff'))
mine4 = [e for e in ev4 if e['adapter_id'] == 'a-audio']
kinds4 = [e['envelope']['kind'] for e in mine4]
need(kinds4[-1] == 'session.close',
     f'课是被中途叫停的，那这条 session.close 只能在 StopLesson 之后才发出——客户端提前停读管道它就会消失：{kinds4}')
ck4 = chunks_of(ev4)
need(ck4, '切到一半的讲话也该被 flush 落盘，而不是跟着进程一起没了')
c4 = [e for e in mine4 if e['envelope']['kind'] == 'session.close'][0]['envelope']['payload']
need(c4['chunks'] == len(ck4), f'收课统计要等于真正落盘的事件数：{c4["chunks"]} vs {len(ck4)}')
need(c4['error'] is None, f'中途下课不是错误：{c4["error"]}')
last = ck4[-1]['envelope']['payload']
need(os.path.exists(os.path.join(work, 'data-cutoff', 'lessons', 'L-demo-0001', 'blobs', last['blob'])),
     '最后一段（被下课切断的那句）的 blob 必须在')
need(last['t0_ms'] <= 5000, f'尾段应落在被切断的时刻附近：t0={last["t0_ms"]}')
bad4 = [e for e in mine4 if e['status'] != 'accepted']
need(not bad4, f'收尾事件也必须全部被接受：{[(b["status"], b.get("raw")) for b in bad4]}')
# 关课前收进来的尾巴不能流落到 misc.ndjson：那是“记在课外面”的另一种说法。
misc4 = os.path.join(work, 'data-cutoff', 'misc.ndjson')
need(not os.path.exists(misc4) or 'audio' not in open(misc4, encoding='utf-8').read(),
     '收尾事件不能写进 misc.ndjson')

# ---------- ⑤ 教师改门限：写盘后下一节课真能读到 ----------
code5 = open(os.path.join(work, 'p5.code'), encoding='utf-8').read().strip()
need(code5 == '200', f'写 params 没成功（{code5}）：{open(os.path.join(work, "p5.json"), encoding="utf-8").read()}')
if code5 == '200':
    need('下一节课' in json.load(open(os.path.join(work, 'p5.json'), encoding='utf-8')).get('note', ''),
         '改完参数不告诉教师什么时候生效，等于让他猜')
b5 = json.load(open(os.path.join(work, 'adp5.before'), encoding='utf-8'))
a5 = json.load(open(os.path.join(work, 'adp-reload', 'a-audio.adapter.json'), encoding='utf-8'))
need(a5['params']['vad']['rms_open'] == 30000.0, f'磁盘上没读到新门限：{a5["params"]["vad"]}')
need(a5['params']['vad']['rms_close'] == 29000.0, '迟滞带下限也该跟着写进去')
need(a5['argv'] == b5['argv'], f'写 params 碰到 argv 了：{a5["argv"]}')
need(a5['enabled'] == b5['enabled'], '只提交 params 时不该动 enabled')
for k in ('source', 'fixture', 'speed', 'emit_silence'):
    need(a5['params'].get(k) == b5['params'].get(k), f'深合并丢了 params.{k}：{a5["params"]}')
# 这一节真的是用新门限采的：适配器自己报了 30000，并且一段都没切出来
p5 = json.load(open(os.path.join(work, 'data-reload', 'lessons', 'L-demo-0001', 'ai_payload.json'), encoding='utf-8'))
need(p5['stats']['audio_chunks'] == 0,
     f'门限改成 30000 后这节课该一段都没有，实得 {p5["stats"]["audio_chunks"]}：写盘没生效')
v5 = p5['sources']['a-audio'].get('vad')
need(isinstance(v5, dict) and v5['rms_open'] == 30000.0, f'适配器自述的门限必须是磁盘上那一份：{v5}')
c5 = p5['sources']['a-audio'].get('close')
need(isinstance(c5, dict) and c5['closes'] == 1 and c5['chunks'] == 0, f'收课统计不对：{c5}')

# ---------- ⑥ 回听：blob 走 HTTP 的 MIME 与字节 ----------
# 观察端用 webview 原生 <audio src> 直连这个接口，不经桌面进程的文本中转（那会毁掉字节）。
ct = open(os.path.join(work, 'blob.ctype'), encoding='utf-8').read().strip()
need(ct == 'audio/wav', f'回听能不能出声只看 content_type：{ct!r}')
disk = os.path.join(work, 'data', 'lessons', 'L-demo-0001', 'blobs', wav_name)
got = open(os.path.join(work, 'blob.wav'), 'rb').read()
need(bool(wav_name) and os.path.getsize(disk) > 0, f'找不到 ① 段的 wav：{wav_name!r}')
need(len(got) == os.path.getsize(disk), f'HTTP 取回 {len(got)} 字节，磁盘上 {os.path.getsize(disk)} 字节')
need(got == open(disk, 'rb').read(), '取回的字节与磁盘上的不是同一份（文本中转会把 WAV 毁掉）')
need(got[:4] == b'RIFF' and got[8:12] == b'WAVE', '回来的不是合法 wav 头')
tr = open(os.path.join(work, 'blob.traversal'), encoding='utf-8').read().strip()
need(tr in ('400', '404'), f'blob 读路径的穿越没挡住（{tr}）——观察端把 blob 名拼进 URL，这条是唯一护栏')

print('AUDIO OK  2 个话轮 / %d 字节 / 丢弃 1 记短促噪音 / 中途下课不丢收尾 / 无设备时不产出也不伪装 / '
      '改门限后下一节课读到 30000 / blob 能按 audio/wav 原样回听' % total)
PY
