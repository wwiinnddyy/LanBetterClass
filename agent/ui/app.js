// agent · 课堂观察端前端逻辑。零构建，纯静态；HTTP 走 Tauri 命令（无 CORS）。
const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);

// 跨标签页共享的当前状态。选中态不能只挂在 li 的 class 上：录音页与调参表单
// 都需要知道"现在看的是哪节课"，否则它们只能各自再维护一份会走样的真相。
const S = { sel: null, payload: null, audioRows: [], shown: 0 };
// 45 分钟课在 max_segment_ms=8000 下连续讲话的上限≈338 段，一次全画完会卡。
const AUDIO_PAGE = 300;

// ---- 连接参数（持久化到 localStorage）----
function base() {
  const trim = (v) => v.trim().replace(/\/+$/, "");
  return {
    client: trim($("#clientBase").value),
    server: trim($("#serverBase").value),
    token: $("#token").value.trim(),
  };
}
function saveCfg() {
  localStorage.setItem("agent.cfg", JSON.stringify(base()));
}
function loadCfg() {
  try {
    const c = JSON.parse(localStorage.getItem("agent.cfg") || "{}");
    if (c.client) $("#clientBase").value = c.client;
    if (c.server) $("#serverBase").value = c.server;
    if (c.token) $("#token").value = c.token;
  } catch (_) {}
}
["clientBase", "serverBase", "token"].forEach((id) =>
  $("#" + id).addEventListener("change", saveCfg)
);

function setStatus(text, ok) {
  const el = $("#status");
  el.textContent = text;
  el.className = "status" + (ok === true ? " ok" : ok === false ? " bad" : "");
}

// ---- HTTP 包装 ----
async function get(url) {
  return await invoke("http_get", { url });
}
async function post(url, body) {
  const t = base().token || null;
  return await invoke("http_post", { url, body: JSON.stringify(body), token: t });
}
// 安全 DOM 构建：数据一律走 textContent，不拼 innerHTML（网络响应半可信）。
function mk(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text != null) n.textContent = text;
  return n;
}
function emptyItem(host, tag, text) {
  host.textContent = "";
  host.appendChild(mk(tag, "empty", text));
}

// ---- 标签页切换 ----
document.querySelectorAll(".tabs button").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tabs button").forEach((b) => b.classList.remove("active"));
    document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
    btn.classList.add("active");
    $("#tab-" + btn.dataset.tab).classList.add("active");
    if (btn.dataset.tab === "audio") loadAudioTab();
  });
});

// ---- 测试连接：两端 health，然后拉数据 ----
$("#btnConnect").addEventListener("click", async () => {
  const { client, server } = base();
  const parts = [];
  let allOk = true;
  try {
    const h = JSON.parse(await get(client + "/api/health"));
    parts.push(`客户端 ✓ v${h.proto} 课程 ${h.lessons ?? "?"}`);
  } catch (e) {
    parts.push(`客户端 ✗`);
    allOk = false;
    console.error(e);
  }
  try {
    const h = JSON.parse(await get(server + "/health"));
    parts.push(`服务端 ✓ 已收 ${h.received ?? "?"}`);
  } catch (e) {
    parts.push(`服务端 ✗`);
    allOk = false;
    console.error(e);
  }
  setStatus(parts.join("  ·  "), allOk);
  loadClientLessons();
  loadAdapters();
  loadServer();
});

// ---- 课程摘要（客户端 core serve）----
async function loadClientLessons() {
  const { client } = base();
  const ul = $("#clientLessons");
  try {
    const ls = JSON.parse(await get(client + "/api/lessons"));
    ul.textContent = "";
    if (!ls.length) {
      emptyItem(ul, "li", "（无课程）");
      return;
    }
    ls.forEach((l) => {
      const li = document.createElement("li");
      const title = `${l.class || "-"} · ${l.subject || "-"}`;
      li.textContent = `${l.lesson_id}  ${title}`;
      li.addEventListener("click", () => {
        ul.querySelectorAll("li").forEach((x) => x.classList.remove("sel"));
        li.classList.add("sel");
        selectClientLesson(l.lesson_id);
      });
      ul.appendChild(li);
    });
  } catch (e) {
    emptyItem(ul, "li", "加载失败：" + e);
  }
}

async function selectClientLesson(id) {
  const { client } = base();
  S.sel = id;
  S.payload = null;
  $("#lessonTitle").textContent = id;
  $("#lessonDigest").textContent = "加载中…";
  $("#lessonStats").innerHTML = "";
  resetAudio();
  try {
    const digest = await get(`${client}/api/lesson/${id}/digest`);
    $("#lessonDigest").textContent = digest;
  } catch (e) {
    $("#lessonDigest").textContent = "digest 读取失败：" + e;
  }
  try {
    const st = JSON.parse(await get(`${client}/api/lesson/${id}/stats`));
    const s = st.stats || {};
    const cells = [
      ["时长", Math.round((s.duration_ms || 0) / 1000) + "s"],
      ["讲话", Math.round(((s.teacher_ms || 0) + (s.student_ms || 0)) / 1000) + "s"],
      ["笔迹", s.strokes ?? 0],
      ["转写句", s.utterances ?? 0],
      ["边讲边写", Math.round((s.writing_while_speaking_ms || 0) / 1000) + "s"],
      ["板面页", s.pages_touched ?? 0],
    ];
    const box = $("#lessonStats");
    box.textContent = "";
    cells.forEach(([k, v]) => {
      const d = mk("div", "stat");
      d.appendChild(mk("b", null, String(v)));
      d.appendChild(mk("span", null, k));
      box.appendChild(d);
    });
  } catch (e) {
    console.error(e);
  }
}

// ---- 数据源配置（客户端 /api/adapters + POST）----
async function loadAdapters() {
  const { client } = base();
  const tbody = $("#adapterTable tbody");
  // 调参表单要把“磁盘声明”与“本节课实际生效”并排：两者不一致就是“改好了但还没生效”的凭据。
  const eff = S.sel ? await payloadOrNull(S.sel) : null;
  const showEmpty = (text) => {
    tbody.textContent = "";
    const tr = document.createElement("tr");
    const td = mk("td", "empty", text);
    td.colSpan = 5;
    tr.appendChild(td);
    tbody.appendChild(tr);
  };
  try {
    const ad = JSON.parse(await get(client + "/api/adapters"));
    if (!ad.length) return showEmpty("adapters.d 下没有 *.adapter.json");
    ad.forEach((a) => {
      const on = a.enabled === true;
      const plats = Array.isArray(a.platforms) && a.platforms.length ? a.platforms.join(", ") : "任意";
      const tr = document.createElement("tr");
      tr.appendChild(mk("td", null, a.id ?? "?"));
      const fileTd = document.createElement("td");
      fileTd.appendChild(mk("code", null, a.file ?? ""));
      tr.appendChild(fileTd);
      tr.appendChild(mk("td", null, plats));
      const stTd = document.createElement("td");
      stTd.appendChild(mk("span", "pill " + (on ? "on" : "off"), on ? "启用" : "停用"));
      tr.appendChild(stTd);
      const actTd = document.createElement("td");
      const b = mk("button", null, on ? "停用" : "启用");
      b.addEventListener("click", () => toggleAdapter(a.file, !on, b));
      actTd.appendChild(b);
      tr.appendChild(actTd);
      tbody.appendChild(tr);
      if (a.params && typeof a.params === "object" && a.params.vad) {
        tbody.appendChild(tuneRow(a, eff));
      }
    });
  } catch (e) {
    showEmpty("加载失败：" + e);
  }
}

async function toggleAdapter(file, enabled, btn) {
  const { client } = base();
  btn.disabled = true;
  try {
    const ack = JSON.parse(await post(`${client}/api/adapter/${file}`, { enabled }));
    setStatus(`已${enabled ? "启用" : "停用"} ${file} · ${ack.note || "ok"}`, true);
    await loadAdapters();
  } catch (e) {
    setStatus("写失败（客户端是否以 --allow-write 启动？）：" + e, false);
    btn.disabled = false;
  }
}

// ---- 录音：话轮列表 + 回听 ----
// 读路径故意与看板摘要/CLI 共用同一个 /api/lesson/<id>：UI 不得成为第二个解释器，
// 否则窗口里看到的与导出给模型的会随时间分叉。
async function payloadOrNull(id) {
  try {
    return await ensurePayload(id);
  } catch (e) {
    console.error(e);
    return null;
  }
}

async function ensurePayload(id) {
  if (S.payload && S.payload.lesson.lesson_id === id) return S.payload;
  const { client } = base();
  const p = JSON.parse(await get(`${client}/api/lesson/${encodeURIComponent(id)}`));
  S.payload = p;
  return p;
}

function resetAudio() {
  S.audioRows = [];
  S.shown = 0;
  const tbody = $("#audioTable tbody");
  tbody.textContent = "";
  const tr = document.createElement("tr");
  const td = mk("td", "empty", "未加载");
  td.colSpan = 7;
  tr.appendChild(td);
  tbody.appendChild(tr);
  $("#audioStats").textContent = "";
  $("#audioFacts").textContent = "先在“课程摘要”里选一节课。";
  $("#audioMore").textContent = "";
}

async function loadAudioTab() {
  if (!S.sel) {
    $("#audioFacts").textContent = "先在“课程摘要”里选一节课。";
    return;
  }
  $("#audioFacts").textContent = "加载中…";
  let p;
  try {
    p = await ensurePayload(S.sel);
  } catch (e) {
    $("#audioFacts").textContent = "读不到这一节课：" + e;
    return;
  }
  renderAudio(p);
}

function audioSources(p) {
  // 一个源算一路：双音频源（麦克风 + 录音笔回放）同时报 close 时不能糊成一个数。
  return Object.entries(p.sources || {}).filter(
    ([, s]) => s.close || s.vad || (s.declared || []).includes("audio.chunk")
  );
}

function renderAudio(p) {
  const st = p.stats || {};
  const rows = (p.track || []).filter((t) => t.kind === "audio.chunk");
  S.audioRows = rows;
  S.shown = 0;
  $("#audioTable tbody").textContent = "";
  appendAudioRows(p, rows);

  const closes = Object.values(p.sources || {}).map((s) => s.close).filter(Boolean);
  const dropped = closes.reduce((a, c) => a + (c.dropped_short || 0), 0);
  const errs = st.stream_errors || 0;
  const box = $("#audioStats");
  box.textContent = "";
  const cells = [
    ["段数", rows.length, false],
    ["字节", fmtBytes(st.audio_bytes || 0), false],
    ["语音", fmtMs(st.audio_speech_ms || 0), false],
    ["短促丢弃", dropped, false],
    // 掉过帧就不能拿时长下结论，这个数大于 0 必须是显眼的。
    ["掉帧", errs, errs > 0],
  ];
  cells.forEach(([k, v, bad]) => {
    const d = mk("div", "stat");
    d.appendChild(mk("b", bad ? "bad" : null, String(v)));
    d.appendChild(mk("span", null, k));
    box.appendChild(d);
  });

  const facts = $("#audioFacts");
  facts.textContent = "";
  const srcs = audioSources(p);
  if (!srcs.length) {
    facts.appendChild(mk("div", null, "这一节没有任何录音源：任何言语互动类结论都不成立。"));
  }
  srcs.forEach(([id, s]) => {
    const v = s.vad;
    const c = s.close;
    const line = [];
    line.push(id);
    line.push(v ? `来路 ${v.input || "?"}${v.device ? "·" + v.device : ""} @${v.sample_rate || "?"}Hz` : "来路未自述");
    line.push(v ? `门限 开${v.rms_open}/关${v.rms_close}` : "门限未自述");
    line.push(v ? `最短语音 ${v.min_speech_ms}ms·单段上限 ${v.max_segment_ms}ms` : "");
    line.push(c ? `收课 ${c.closes} 次：${c.chunks} 段/${fmtBytes(c.bytes)}/语音 ${fmtMs(c.voiced_ms)}/丢弃 ${c.dropped_short}/掉帧 ${c.stream_errors}` : "没有收课记录（源挂在半路，或这一节没结束）");
    const el = mk("div", c && c.stream_errors > 0 ? "bad" : null, line.filter(Boolean).join("｜"));
    facts.appendChild(el);
    if (c && c.closes > 1) {
      facts.appendChild(mk("div", "bad", `！这一节用过 ${c.closes} 套采集参数（中途重启过），时长是几段拼起来的`));
    }
  });
  if (errs > 0) {
    facts.appendChild(mk("div", "bad", `！录音掉过 ${errs} 次采集帧：跨过这些点的时长类结论不成立`));
  }
  (p.warnings || []).filter((w) => w.includes("录音") || w.includes("收课")).forEach((w) => {
    facts.appendChild(mk("div", "bad", "！" + w));
  });
}

function appendAudioRows(p, rows) {
  rows = rows || S.audioRows;
  const tbody = $("#audioTable tbody");
  if (!tbody.children.length || tbody.children[0].firstElementChild?.className === "empty") tbody.textContent = "";
  // 横条与两条门限参考线共用一个标尺：本节最响的那一段。标尺是局部的，
  // 但"这段过不过门限线"这个判断也是局部的——调参要看的恰恰是这一节课里差多少。
  const maxRms = Math.max(1, ...rows.map((r) => (r.detail && r.detail.rms) || 0));
  const withVad = audioSources(p).find(([, s]) => s.vad);
  const vad = withVad ? withVad[1].vad : null;
  const upto = Math.min(rows.length, S.shown + AUDIO_PAGE);
  for (let i = S.shown; i < upto; i++) {
    const r = rows[i];
    const d = r.detail || {};
    const tr = document.createElement("tr");
    tr.appendChild(mk("td", null, `${fmtClock(r.t0_ms)} → ${fmtClock(r.t1_ms)}`));
    tr.appendChild(mk("td", null, fmtMs(r.t1_ms - r.t0_ms)));
    tr.appendChild(mk("td", null, d.speech_ms != null ? fmtMs(d.speech_ms) : "—"));
    const rmsTd = document.createElement("td");
    const bar = mk("div", "bar");
    if (d.rms != null) {
      bar.appendChild(mk("i", "bar-fill"));
      bar.lastChild.style.width = Math.min(100, (d.rms / maxRms) * 100).toFixed(1) + "%";
      if (vad) {
        const open = mk("i", "bar-line open");
        open.style.left = Math.min(100, (vad.rms_open / maxRms) * 100).toFixed(1) + "%";
        const close = mk("i", "bar-line close");
        close.style.left = Math.min(100, (vad.rms_close / maxRms) * 100).toFixed(1) + "%";
        bar.appendChild(open);
        bar.appendChild(close);
      }
      bar.title = `rms ${d.rms}${d.peak != null ? " / peak " + d.peak : ""}（绿线=开段门限，灰线=关段门限）`;
      rmsTd.appendChild(bar);
      rmsTd.appendChild(mk("span", "bar-num", String(Math.round(d.rms))));
    } else {
      // 没报就是没报：补一个 0 会把"旧版源没自述电平"读成"这段完全是静音"。
      rmsTd.textContent = "未报";
    }
    tr.appendChild(rmsTd);
    tr.appendChild(mk("td", null, d.peak != null ? String(d.peak) : "—"));
    const blob = (r.refs && r.refs[0]) || "";
    const nameTd = document.createElement("td");
    nameTd.appendChild(mk("code", null, blob));
    tr.appendChild(nameTd);
    const actTd = document.createElement("td");
    const btn = mk("button", null, "回听");
    btn.disabled = !blob;
    btn.addEventListener("click", () => playBlob(S.sel, blob, r));
    actTd.appendChild(btn);
    tr.appendChild(actTd);
    tbody.appendChild(tr);
  }
  S.shown = upto;
  const more = $("#audioMore");
  more.textContent = "";
  if (S.shown < rows.length) {
    const b = mk("button", null, `再看 ${Math.min(AUDIO_PAGE, rows.length - S.shown)} 段（共 ${rows.length} 段，已显示 ${S.shown}）`);
    b.addEventListener("click", () => appendAudioRows(p));
    more.appendChild(b);
  } else if (rows.length) {
    more.textContent = `共 ${rows.length} 段，全部已列出`;
  } else {
    more.textContent = "这一节没切出任何话轮：环境噪声在开段门限之下（去“数据源配置”把 rms_open 调低）。";
  }
}

// 回听：二进制不经 invoke("http_get")——那个命令把响应体按文本读（from_utf8_lossy），
// WAV 字节必被毁。媒体元素自己发的是 no-cors 请求，serve 不返 CORS 头也能播。
function playBlob(id, name, row) {
  const { client } = base();
  const url = `${client}/api/lesson/${encodeURIComponent(id)}/blob/${encodeURIComponent(name)}`;
  const el = $("#player");
  el.src = url;
  $("#playerName").textContent = `${id} · ${name} · 起于 ${fmtClock(row.t0_ms)} · ${fmtMs(row.t1_ms - row.t0_ms)}`;
  el.play().catch((e) => setStatus("回听失败（不支持 Range，拖进度条会重取整段）：" + e, false));
}

// ---- 服务端视图 ----
async function loadServer() {
  const { server } = base();
  try {
    const h = JSON.parse(await get(server + "/health"));
    $("#serverHealth").textContent = `服务端 v${h.proto} · 已收 ${h.received ?? "?"} 节 · 鉴权 ${h.auth ? "开" : "关"}`;
  } catch (e) {
    $("#serverHealth").textContent = "服务端未连接";
    return;
  }
  const ul = $("#serverLessons");
  try {
    const ls = JSON.parse(await get(server + "/api/lessons"));
    ul.textContent = "";
    if (!ls.length) {
      emptyItem(ul, "li", "（服务端还没收到课）");
      return;
    }
    ls.forEach((l) => {
      const li = document.createElement("li");
      li.textContent = `${l.lesson_id}  (${Math.round((l.bytes || 0) / 1024)} KB)`;
      li.addEventListener("click", () => {
        ul.querySelectorAll("li").forEach((x) => x.classList.remove("sel"));
        li.classList.add("sel");
        selectServerLesson(l.lesson_id);
      });
      ul.appendChild(li);
    });
  } catch (e) {
    emptyItem(ul, "li", "加载失败：" + e);
  }
}

async function selectServerLesson(id) {
  const { server } = base();
  $("#serverTitle").textContent = id + " · AI 请求单";
  $("#serverDetail").textContent = "加载中…";
  try {
    const txt = await get(`${server}/api/lesson/${id}/ai-request`);
    $("#serverDetail").textContent = pretty(txt);
  } catch (e) {
    $("#serverDetail").textContent = "读取失败：" + e;
  }
}

function pretty(txt) {
  try {
    return JSON.stringify(JSON.parse(txt), null, 2);
  } catch (_) {
    return txt;
  }
}

// ---- 调参：只提交改过的字段，写盘后下一节课生效 ----
// 字段名必须与适配器认识的键一字不差（vad.rs 的七个 + main.rs 的三个），
// 名字错了不报错，只是静默无效——所以 agent.yml 里有一条文本守卫钉它们。
const VAD_FIELDS = [
  ["rms_open", "开段门限 rms_open"],
  ["rms_close", "关段门限 rms_close"],
  ["hangover_ms", "尾巴时长 hangover_ms"],
  ["preroll_ms", "起音预滚 preroll_ms"],
  ["min_speech_ms", "最短语音 min_speech_ms"],
  ["max_segment_ms", "单段上限 max_segment_ms"],
];
const TOP_FIELDS = [
  ["frame_ms", "帧长 frame_ms"],
  ["sample_rate", "采样率（偏好值）sample_rate"],
];
const BOOL_FIELDS = [["emit_silence", "报静音段 emit_silence"]];

function fmtMs(ms) {
  if (ms == null) return "—";
  return ms < 1000 ? ms + "ms" : (ms / 1000).toFixed(ms < 10000 ? 2 : 1) + "s";
}

function fmtClock(ms) {
  const t = Math.floor((ms || 0) / 1000);
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const s = t % 60;
  const p = (n) => String(n).padStart(2, "0");
  return h ? `${h}:${p(m)}:${p(s)}` : `${p(m)}:${p(s)}`;
}

function fmtBytes(b) {
  if (!b) return "0B";
  if (b < 1024) return b + "B";
  if (b < 1024 * 1024) return Math.round(b / 1024) + "KB";
  return (b / 1048576).toFixed(1) + "MB";
}

function tuneRow(a, effPayload) {
  const tr = document.createElement("tr");
  const td = document.createElement("td");
  td.colSpan = 5;
  const box = mk("details", "tune");
  box.appendChild(mk("summary", null, `改采集参数 ${a.id}（写 adapters.d/${a.file}，下一节课生效）`));
  const body = document.createElement("div");
  const grid = mk("div", "tune-grid");
  const eff = (effPayload && effPayload.sources && effPayload.sources[a.id] && effPayload.sources[a.id].vad) || null;
  const inputs = [];
  const addNum = (label, cur, path) => {
    const l = document.createElement("label");
    l.appendChild(mk("span", null, label));
    const inp = document.createElement("input");
    inp.type = "number";
    inp.step = "any";
    inp.value = cur == null ? "" : String(cur);
    // 磁盘上没写的字段不是 0，是"用适配器默认"。把默认值当 0 填进去，下次保存就真把它钉住了。
    if (cur == null) inp.placeholder = eff && eff[path[path.length - 1]] != null ? `未写·实际 ${eff[path[path.length - 1]]}` : "未写";
    l.appendChild(inp);
    grid.appendChild(l);
    inputs.push([path, inp, cur]);
    return inp;
  };
  VAD_FIELDS.forEach(([k, label]) => addNum(label, a.params.vad ? a.params.vad[k] : null, ["vad", k]));
  TOP_FIELDS.forEach(([k, label]) => addNum(label, a.params[k], [k]));
  BOOL_FIELDS.forEach(([k, label]) => {
    const l = document.createElement("label");
    l.className = "tune-check";
    const inp = document.createElement("input");
    inp.type = "checkbox";
    inp.checked = a.params[k] === true;
    l.appendChild(inp);
    l.appendChild(mk("span", null, label));
    grid.appendChild(l);
    inputs.push([[k], inp, a.params[k] === true]);
  });
  body.appendChild(grid);

  // 磁盘值 vs 本节课实际生效：不一致就直说"下一节课才生效"，别让教师自己猜。
  const effLine = document.createElement("div");
  effLine.className = "tune-eff";
  if (eff) {
    const disk = a.params.vad || {};
    // 磁盘上没写的键走的是适配器默认值，那个差异不是"还没生效"，不该拿它报警。
    const same = VAD_FIELDS.every(([k]) => disk[k] == null || Number(disk[k]) === Number(eff[k]));
    const parts = VAD_FIELDS.map(([k]) => `${k}=${eff[k]}`).join(" ");
    effLine.textContent = `${S.sel || "本节课"} 实际生效：${parts}${
      eff.input ? "｜来路 " + eff.input + " @" + eff.sample_rate + "Hz" : ""
    }`;
    if (!same) {
      effLine.className = "tune-eff pending";
      effLine.textContent += "——与磁盘声明不一致：改好了，下一节课才生效";
    }
  } else {
    effLine.textContent = S.sel
      ? "本节课没报出自述参数（这一节没跑过这个源，或它不是录音源）"
      : "选一节课后，这里会列出本节课实际生效的门限";
  }
  body.appendChild(effLine);
  body.appendChild(mk("div", "tune-eff", "sample_rate 是偏好值：设备不一致时以设备真实采样率为准（不重采样）。"));

  const row = document.createElement("div");
  row.style.marginTop = "8px";
  const save = mk("button", "primary", "保存（下一节课生效）");
  save.addEventListener("click", () => saveTune(a, inputs, save));
  row.appendChild(save);
  body.appendChild(row);
  box.appendChild(body);
  td.appendChild(box);
  tr.appendChild(td);
  return tr;
}

async function saveTune(a, inputs, btn) {
  const { client } = base();
  // 只提交改过的：没改的字段不写，才能保住磁盘上不属于这个表单的那些键（source/fixture/note）。
  const params = {};
  let n = 0;
  for (const [path, inp, initial] of inputs) {
    let val;
    if (inp.type === "checkbox") {
      val = inp.checked;
      if (val === initial) continue;
    } else {
      if (inp.value.trim() === "") continue;
      val = Number(inp.value);
      if (!Number.isFinite(val)) {
        setStatus(`${path.join(".")} 不是数：${inp.value}`, false);
        return;
      }
      if (val === Number(initial)) continue;
    }
    if (path.length === 2) {
      (params[path[0]] = params[path[0]] || {})[path[1]] = val;
    } else {
      params[path[0]] = val;
    }
    n++;
  }
  if (!n) {
    setStatus("没改任何字段，不提交", true);
    return;
  }
  btn.disabled = true;
  try {
    const ack = JSON.parse(await post(`${client}/api/adapter/${a.file}`, { params }));
    setStatus(`已写 ${a.file} · ${ack.note || "ok"}`, true);
    await loadAdapters();
  } catch (e) {
    // 400 的文案里带着"为什么不能这样写"（例如 rms_close 必须低于 rms_open），必须原样回显。
    setStatus("保存被拒：" + e, false);
    btn.disabled = false;
  }
}

loadCfg();
