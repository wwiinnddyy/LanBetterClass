#!/usr/bin/env bash
# 端到端集成守卫：客户端(classagent-core) → 远程服务端(classagent-server) 的 HTTP 链路。
# 与 ci-smoke.sh 一样，Linux 与 Windows 两个 runner 各跑一遍同一份脚本。
# 断的是真行为：真实 push（走 std::net 手搓的 HTTP/1.1 客户端）打到 tiny_http 服务端，
# 落盘去重、AI 请求单生成、token 鉴权、路径穿越防护。本机磁盘不允许本地构建，只在 CI 上跑。
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT) EXE=.exe ;;
  *) EXE= ;;
esac
export PYTHONIOENCODING=utf-8
echo "platform=$(uname -s)  exe='${EXE:-无}'"

BIN=${BIN:-target/debug}
WORK=${INTEG_DIR:-.ci-integ}
DATA=$WORK/data
ADP=$WORK/adapters
SRV=$WORK/server-data
LOG=$WORK/core.log
LESSON=L-demo-0001
PORT=${PORT:-8899}
TOKEN=ci-secret-token

CORE="$BIN/classagent-core$EXE"
SERVER="$BIN/classagent-server$EXE"
[ -f "$CORE" ]   || { echo "FAIL 找不到 $CORE"; exit 1; }
[ -f "$SERVER" ] || { echo "FAIL 找不到 $SERVER"; exit 1; }

rm -rf "$WORK"
mkdir -p "$ADP" "$DATA" "$SRV"

# 只用 a-fake 产一节课（笔迹 + 转写），够把 ai_payload 撑起来。
python3 - "$ADP" <<'PY'
import json, sys
adp = sys.argv[1]
fake = json.load(open('adapters.d/a-fake.adapter.json', encoding='utf-8'))
fake['enabled'] = True
fake['params'].update({'speed': 400, 'minutes': 45})
json.dump(fake, open(f'{adp}/a-fake.adapter.json', 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
PY

# 启动服务端（带 token 鉴权）
"$SERVER" --listen "127.0.0.1:$PORT" --data "$SRV" --token "$TOKEN" > "$WORK/server.log" 2>&1 &
SP=$!
trap 'kill $SP 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.25
done

# 采集 + 导出一节课
"$CORE" run --data "$DATA" --adapters "$ADP" --lesson examples/lesson.demo.json --max-seconds 8 \
  < /dev/null > "$LOG" 2>&1 || true
"$CORE" export --data "$DATA" --lesson "$LESSON" > "$WORK/export.log" 2>&1 \
  || { echo "FAIL export 失败"; tail -20 "$LOG"; exit 1; }

code() { curl -s --path-as-is -o "$2" -w '%{http_code}' "$1"; }   # code <url> <outfile>

# ① 真实 push（客户端 → 服务端），带正确 token
set +e
"$CORE" push --data "$DATA" --lesson "$LESSON" --server "127.0.0.1:$PORT" --token "$TOKEN" \
  > "$WORK/push1.log" 2>&1
PUSH1_RC=$?
set -e

# ② 再 push 一次：应幂等（deduped）
"$CORE" push --data "$DATA" --lesson "$LESSON" --server "127.0.0.1:$PORT" --token "$TOKEN" \
  > "$WORK/push2.log" 2>&1 || true

# ③ 错误 token 的 push：客户端应非零退出
set +e
"$CORE" push --data "$DATA" --lesson "$LESSON" --server "127.0.0.1:$PORT" --token WRONG \
  > "$WORK/pushbad.log" 2>&1
PUSHBAD_RC=$?
set -e

# 服务端只读接口 + 边界
HEALTH=$(code "http://127.0.0.1:$PORT/health"                          "$WORK/health.json")
LS=$(code "http://127.0.0.1:$PORT/api/lessons"                         "$WORK/lessons.json")
PAY=$(code "http://127.0.0.1:$PORT/api/lesson/$LESSON/payload"         "$WORK/payload.json")
AIR=$(code "http://127.0.0.1:$PORT/api/lesson/$LESSON/ai-request"      "$WORK/ai_request.json")
TRA=$(code "http://127.0.0.1:$PORT/api/lesson/..%2F..%2Fetc%2Fpasswd/payload" "$WORK/traversal.json")
BADPOST=$(curl -s -X POST -H 'Content-Type: application/json' -H 'X-ClassAgent-Token: nope' \
          --data-binary "@$DATA/lessons/$LESSON/ai_payload.json" \
          -o "$WORK/badpost.json" -w '%{http_code}' "http://127.0.0.1:$PORT/api/ingest")

kill $SP 2>/dev/null || true; trap - EXIT

cat > "$WORK/codes.txt" <<EOF
push1_rc=$PUSH1_RC pushbad_rc=$PUSHBAD_RC
health=$HEALTH lessons=$LS payload=$PAY ai_request=$AIR traversal=$TRA badpost=$BADPOST
EOF

python3 - "$WORK" "$DATA" "$LESSON" <<'PY'
import json, os, sys
work, data, lesson = sys.argv[1:4]
rd = lambda n: open(os.path.join(work, n), encoding='utf-8').read()
codes = dict(tok.split('=', 1) for tok in rd('codes.txt').split() if '=' in tok)
fails = []
def need(c, m):
    if not c: fails.append(m)

print('== 集成实测 ==', json.dumps(codes, ensure_ascii=False))

# ① 真实 push 成功，ack 说 stored 且非 dedup
p1 = rd('push1.log')
need(codes['push1_rc'] == '0', f"push#1 退出码 {codes['push1_rc']}（应 0）")
need('"ok": true' in p1,        'push#1 ack 没有 ok:true')
need('"stored": true' in p1,    'push#1 应当真的落盘 stored:true')
need('"deduped": false' in p1,  'push#1 首投不该是 deduped')

# ② 重投幂等
p2 = rd('push2.log')
need('"deduped": true' in p2,   'push#2 重投应 deduped:true')
need('"stored": false' in p2,   'push#2 内容未变不应重写 stored:false')

# ③ 错误 token：客户端非零退出
need(codes['pushbad_rc'] != '0', '错误 token 的 push 竟然成功了（鉴权没生效）')
need('401' in rd('pushbad.log'),  '错误 token 应收到 HTTP 401')

# 只读接口
need(codes['health'] == '200',   f"/health 不是 200：{codes['health']}")
need(json.loads(rd('health.json')).get('received', 0) >= 1, '/health 的 received 计数没记上')
need(codes['lessons'] == '200',  '/api/lessons 不是 200')
ls = json.loads(rd('lessons.json'))
need(any(x.get('lesson_id') == lesson for x in ls), f'服务端课程列表没有 {lesson}：{ls}')
need(codes['payload'] == '200',  '取 ai_payload 不是 200')
pay = json.loads(rd('payload.json'))
need('stats' in pay and 'track' in pay, '落盘的 ai_payload 缺 stats/track')
need(pay.get('lesson', {}).get('lesson_id') == lesson, 'payload 里的 lesson_id 对不上')

# AI 请求单：四次投影齐全（服务端到此为止，不接模型）
need(codes['ai_request'] == '200', '取 ai_request 不是 200')
air = json.loads(rd('ai_request.json'))
names = {p['name'] for p in air.get('projections', [])}
for want in ('observe_log', 'lesson_notes', 'activity_analysis', 'next_steps'):
    need(want in names, f'AI 请求单缺投影 {want}')
need(air.get('stats', {}).get('strokes', 0) > 0, 'AI 请求单没带上 stats（应透传 payload 的统计）')

# 边界
need(codes['traversal'] in ('400', '404'), f'路径穿越没挡住（{codes["traversal"]}）')
need(codes['badpost'] == '401',            f'错误 token 的 POST /api/ingest 应 401，实际 {codes["badpost"]}')

if fails:
    print('INTEG FAIL')
    for f in fails: print('  -', f)
    sys.exit(1)
print('INTEG OK')
PY
