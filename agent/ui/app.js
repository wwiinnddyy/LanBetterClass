// agent · 课堂观察端前端逻辑。零构建，纯静态；HTTP 走 Tauri 命令（无 CORS）。
const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);

// 跨标签页共享的当前状态。选中态不能只挂在 li 的 class 上：录音页、关键帧页与调参表单
// 都需要知道"现在看的是哪节课"，否则它们只能各自再维护一份会走样的真相。
const S = { sel: null, payload: null, audioRows: [], frameRows: [], shown: 0, frameShown: 0 };
// 45 分钟课在 max_segment_ms=8000 下连续讲话的上限≈338 段，一次全画完会卡。
const AUDIO_PAGE = 300;
// 关键帧按"尽量密"的默认参数能到几千张，而每行都带一个可点开的引用；
// 表格只算 DOM，比图片便宜得多，但一次上几千行仍然会卡住主线程。
const FRAME_PAGE = 200;

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
    if (btn.dataset.tab === "frames") loadFramesTab();
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
  resetFrames();
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
      if (a.params && typeof a.params === "object") {
        // 两个适配器各有各的调参面：vad 那套是电平门限，screen 那套是变化门限。
        if (a.params.vad) tbody.appendChild(tuneRow(a, eff));
        else if (a.params.screen || a.params.capture) tbody.appendChild(screenTuneRow(a, eff));
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

  const srcs = audioSources(p);
  // 本行的错误只算音频源自己的。stats.stream_errors 是全源之和，拿它来写"录音掉帧"
  // 会把抓屏后端的重建算到录音头上——两个源现在共用这一个字段。
  const closes = srcs.map(([, s]) => s.close).filter(Boolean);
  const dropped = closes.reduce((a, c) => a + (c.dropped_short || 0), 0);
  const errs = closes.reduce((a, c) => a + (c.stream_errors || 0), 0);
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
  // 警告按源归属，不靠关键词猜：措辞一改（"录音"→"采集流错误"）关键词就静默失配，
  // 而失配的表现是这一页什么都不提示——正是最难发现的那种错。
  sourceWarnings(srcs, p).forEach((w) => facts.appendChild(mk("div", "bad", "！" + w)));
}

// 属于这几个源的警告。timeline 的警告文案一律以 `<源id> ` 开头，认这个就够。
function sourceWarnings(srcs, p) {
  const ids = srcs.map(([id]) => id);
  return (p.warnings || []).filter((w) => ids.some((id) => w.includes(id)));
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

// 二进制一律不经 invoke("http_get")——那个命令把响应体按文本读（from_utf8_lossy），
// WAV 与 PNG 字节必被毁。媒体元素自己发的是 no-cors 请求，serve 不返 CORS 头也能播。
function blobUrl(id, name) {
  const { client } = base();
  return `${client}/api/lesson/${encodeURIComponent(id)}/blob/${encodeURIComponent(name)}`;
}

function playBlob(id, name, row) {
  const el = $("#player");
  el.src = blobUrl(id, name);
  $("#playerName").textContent = `${id} · ${name} · 起于 ${fmtClock(row.t0_ms)} · ${fmtMs(row.t1_ms - row.t0_ms)}`;
  el.play().catch((e) => setStatus("回听失败（不支持 Range，拖进度条会重取整段）：" + e, false));
}

// ---- 关键帧：时间轴刻度 + 帧列表 + 看原图 ----
// 与录音页共用同一条读路径（/api/lesson/<id>）与同一条取图路径（/blob/ 直连）。
// 刻意不做缩略图预取：一节课可能几千张，先省掉 DOM 与请求，看哪张取哪张。
function resetFrames() {
  S.frameRows = [];
  S.frameShown = 0;
  const tbody = $("#frameTable tbody");
  tbody.textContent = "";
  const tr = document.createElement("tr");
  const td = mk("td", "empty", "未加载");
  td.colSpan = 7;
  tr.appendChild(td);
  tbody.appendChild(tr);
  $("#frameMeta").textContent = "";
  $("#frameTicks").textContent = "";
  $("#frameFacts").textContent = "先在“课程摘要”里选一节课。";
  $("#frameMore").textContent = "";
  $("#frameView").removeAttribute("src");
  $("#frameHint").textContent = "点下面任意一格看那张图（原图走 /blob/，不经文本通道）";
}

async function loadFramesTab() {
  if (!S.sel) {
    $("#frameFacts").textContent = "先在“课程摘要”里选一节课。";
    return;
  }
  $("#frameFacts").textContent = "加载中…";
  let p;
  try {
    p = await ensurePayload(S.sel);
  } catch (e) {
    $("#frameFacts").textContent = "读不到这一节课：" + e;
    return;
  }
  renderFrames(p);
}

function screenSources(p) {
  return Object.entries(p.sources || {}).filter(
    ([, s]) => s.screen || s.close || (s.declared || []).includes("screen.keyframe")
  );
}

const n0 = (x) => (typeof x === "number" && Number.isFinite(x) ? x : 0);

// 四个 trigger 是适配器自己定的四个词（main.rs::consider），界面只负责说人话。
const TRIGGER_LABELS = {
  open: "开场",
  phash: "结构变了",
  mass: "整块变亮/变暗",
  close: "下课补采",
};

function renderFrames(p) {
  const st = p.stats || {};
  const rows = (p.track || []).filter((t) => t.kind === "screen.keyframe");
  S.frameRows = rows;
  S.frameShown = 0;
  $("#frameTable tbody").textContent = "";
  appendFrameRows(p, rows);
  renderTicks(rows);

  const srcs = screenSources(p);
  const extra = srcs.map(([, s]) => s.close_extra).filter(Boolean);
  const sum = (k) => extra.reduce((a, x) => a + n0(x[k]), 0);
  const errs = srcs
    .map(([, s]) => s.close)
    .filter(Boolean)
    .reduce((a, c) => a + n0(c.stream_errors), 0);
  const box = $("#frameMeta");
  box.textContent = "";
  [
    ["帧数", rows.length, false],
    ["字节", fmtBytes(st.keyframe_bytes || 0), false],
    ["问过几次", sum("polls"), false],
    ["判为无变化", sum("unchanged"), false],
    ["被节流", sum("throttled"), false],
    // 触顶 = 自己设的张数/字节上限把采集停下来了，那一节课后半段是没有证据的。
    ["触顶停采", sum("capped"), sum("capped") > 0],
    ["后端出错", errs, errs > 0],
  ].forEach(([k, v, bad]) => {
    const d = mk("div", "stat");
    d.appendChild(mk("b", bad ? "bad" : null, String(v)));
    d.appendChild(mk("span", null, k));
    box.appendChild(d);
  });

  const facts = $("#frameFacts");
  facts.textContent = "";
  if (!srcs.length) {
    facts.appendChild(mk("div", null, "这一节没有任何屏幕源：任何“老师展示了什么”类结论都不成立。"));
  }
  srcs.forEach(([id, s]) => {
    const v = s.screen || {};
    const c = s.close;
    const e = s.close_extra || {};
    const sc = v.screen || {};
    const line = [id];
    if (v.input === "fixture") {
      line.push(`回放 ${v.dir || "?"}·${v.frames ?? "?"} 张·${v.speed ?? "?"}x`);
    } else if (v.backend) {
      line.push(
        `后端 ${v.backend}${v.monitor != null ? "·显示器 " + v.monitor : ""}` +
          (v.width ? `·${v.width}x${v.height}` : "") +
          (v.dpi_aware ? "·DPI 已感知" : "")
      );
      if (v.wanted_capture && v.wanted_capture !== v.backend && v.wanted_capture !== "auto") {
        line.push(`想要 ${v.wanted_capture}，实际用了 ${v.backend}`);
      }
    } else {
      line.push("来路未自述");
    }
    if (v.fallback) line.push(String(v.fallback));
    line.push(
      `生效门限 dist≥${sc.min_dist ?? "?"} 且均差≥${sc.min_mad ?? "?"}｜问隔 ${sc.poll_ms ?? "?"}ms｜最小间隔 ${sc.min_interval_ms ?? "?"}ms｜格子 ${sc.bbox ?? "?"}`
    );
    line.push(
      `上限 宽 ${sc.max_width || "不限"}/${sc.max_frames_per_lesson || "不限张"}/${
        sc.max_bytes_per_lesson ? fmtBytes(sc.max_bytes_per_lesson) : "不限字节"
      }`
    );
    line.push(
      c
        ? `收课 ${c.closes} 次：${c.chunks} 帧/${fmtBytes(c.bytes)}${c.stream_errors ? "/后端出错 " + c.stream_errors : ""}`
        : "没有收课记录（源挂在半路，或这一节没结束）"
    );
    const bad = n0(c && c.stream_errors) > 0 || n0(e.capped) > 0;
    facts.appendChild(mk("div", bad ? "bad" : null, line.filter(Boolean).join("｜")));
    if (c && c.closes > 1) {
      facts.appendChild(mk("div", "bad", `！这一节用过 ${c.closes} 套采集参数（中途重启过），帧序会换过一次代号`));
    }
    // "一张"与"全被拦下"在上面那几个数里看不出差别，所以这里把两种情形状分开说一句。
    if (rows.length <= 1 && n0(e.unchanged) > 0) {
      facts.appendChild(mk("div", null, `问 ${n0(e.polls)} 次里有 ${n0(e.unchanged)} 次判为无变化——屏幕整节课几乎不动时这是正常的；若你确定翻过页，去“数据源配置”把 min_dist / min_mad 调低。`));
    }
    if (n0(e.capped) > 0) {
      facts.appendChild(mk("div", "bad", `！自设上限触发过 ${n0(e.capped)} 次：之后的画面没有存档，要么放宽 max_* 要么接受证据不全。`));
    }
  });
  sourceWarnings(srcs, p).forEach((w) => facts.appendChild(mk("div", "bad", "！" + w)));
}

// 时间轴刻度：让看课的人不用滚几千行也能扫过整节课的画面变化。
// 只画等距采样的 ≤120 个：一帧一个按钮在长课上会把主线程卡住，而密到看不见也失去意义。
function renderTicks(rows) {
  const host = $("#frameTicks");
  host.textContent = "";
  if (!rows.length) return;
  const span = Math.max(1, rows[rows.length - 1].t0_ms || 1);
  const stride = Math.max(1, Math.ceil(rows.length / 120));
  for (let i = 0; i < rows.length; i += stride) {
    const r = rows[i];
    const d = r.detail || {};
    const b = mk("button", "tick", String(i + 1));
    b.style.left = ((r.t0_ms / span) * 100).toFixed(2) + "%";
    b.title = `第 ${i + 1} 帧 · ${fmtClock(r.t0_ms)} · ${TRIGGER_LABELS[d.trigger] || d.trigger || "?"} · dist ${d.dist ?? "—"} / 均差 ${d.mad ?? "—"}`;
    b.addEventListener("click", () => selectFrame(r));
    host.appendChild(b);
  }
  if (stride > 1) {
    host.appendChild(mk("span", "hint", `共 ${rows.length} 帧，刻度按每 ${stride} 帧取一个`));
  }
}

function appendFrameRows(p, rows) {
  rows = rows || S.frameRows;
  const tbody = $("#frameTable tbody");
  if (!tbody.children.length || tbody.children[0].firstElementChild?.className === "empty") tbody.textContent = "";
  const upto = Math.min(rows.length, S.frameShown + FRAME_PAGE);
  for (let i = S.frameShown; i < upto; i++) {
    const r = rows[i];
    const d = r.detail || {};
    const tr = document.createElement("tr");
    tr.appendChild(mk("td", null, fmtClock(r.t0_ms)));
    tr.appendChild(mk("td", null, TRIGGER_LABELS[d.trigger] || d.trigger || "—"));
    // 两个门限都要显：单看 dist 会读出"变化很小"，而整块变亮时 dist 可以是 0。
    tr.appendChild(mk("td", null, d.dist != null ? `Δ${d.dist} / 均差 ${d.mad ?? "—"}` : "—"));
    tr.appendChild(mk("td", null, fmtDirty(d.dirty)));
    tr.appendChild(mk("td", null, d.width ? `${d.width}×${d.height}` : "—"));
    const blob = (r.refs && r.refs[0]) || "";
    const nameTd = document.createElement("td");
    nameTd.appendChild(mk("code", null, blob));
    tr.appendChild(nameTd);
    const actTd = document.createElement("td");
    const btn = mk("button", null, "看原图");
    btn.disabled = !blob;
    btn.addEventListener("click", () => selectFrame(r));
    actTd.appendChild(btn);
    tr.appendChild(actTd);
    tbody.appendChild(tr);
  }
  S.frameShown = upto;
  const more = $("#frameMore");
  more.textContent = "";
  if (S.frameShown < rows.length) {
    const b = mk("button", null, `再看 ${Math.min(FRAME_PAGE, rows.length - S.frameShown)} 帧（共 ${rows.length} 帧，已显示 ${S.frameShown}）`);
    b.addEventListener("click", () => appendFrameRows(p));
    more.appendChild(b);
  } else if (rows.length) {
    more.textContent = `共 ${rows.length} 帧，全部已列出`;
  } else {
    more.textContent = "这一节一张关键帧都没有：要么屏幕真的全程没动，要么这个源压根没跑起来（看上面那行自述）。";
  }
}

function fmtDirty(d) {
  // dirty 是像素坐标 [x0,y0,x1,y1]；开场帧没有可比的上一帧，所以是"未报"而不是 0。
  if (!Array.isArray(d) || d.length !== 4) return "未报";
  return `${d[0]},${d[1]} → ${d[2] - d[0]}×${d[3] - d[1]}`;
}

function selectFrame(r) {
  const blob = (r.refs && r.refs[0]) || "";
  if (!blob) return;
  const img = $("#frameView");
  img.src = blobUrl(S.sel, blob);
  const d = r.detail || {};
  $("#frameHint").textContent =
    `${S.sel} · ${blob} · ${fmtClock(r.t0_ms)} · ${d.width || "?"}×${d.height || "?"} · ` +
    `触发 ${TRIGGER_LABELS[d.trigger] || d.trigger || "?"} · Δ${d.dist ?? "—"} / 均差 ${d.mad ?? "—"} · 变化区 ${fmtDirty(d.dirty)}`;
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

// 字段名必须与适配器认识的键一字不差（a-screen 的 main.rs::Cfg::from_params），
// 写错了不报错、只是静默无效——所以 ci-screen.sh 与 agent.yml 各有一条文本守卫钉它们。
const SCREEN_FIELDS = [
  ["poll_ms", "问屏间隔 poll_ms"],
  ["min_interval_ms", "最小落盘间隔 min_interval_ms"],
  ["min_dist", "变化门限 min_dist（dHash 位差，≤ 64）"],
  ["min_mad", "补位门限 min_mad（格子均差，≤ 255）"],
  ["max_width", "降采样宽度 max_width（0 = 不降）"],
  ["max_frames_per_lesson", "单节张数上限（0 = 不限）"],
  ["max_bytes_per_lesson", "单节字节上限（0 = 不限）"],
];
const SCREEN_TOP_FIELDS = [["monitor", "显示器序号 monitor（0 = 主屏）"]];
// 这三个取值就是 capture.rs 的 BACKENDS，改动必须同步——ci-screen.sh 第 ⑧ 场钉的就是它。
const SCREEN_SELECTS = [["capture", "抓屏后端 capture", ["auto", "gdi", "dxgi"]]];
const SCREEN_BOOLS = [["emit_dirty", "随帧报变化区 bbox emit_dirty"]];

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
  const addNum = (label, cur, path) =>
    addNumInput(grid, inputs, label, cur, path, eff ? eff[path[path.length - 1]] : null);
  VAD_FIELDS.forEach(([k, label]) => addNum(label, a.params.vad ? a.params.vad[k] : null, ["vad", k]));
  TOP_FIELDS.forEach(([k, label]) => addNum(label, a.params[k], [k]));
  BOOL_FIELDS.forEach(([k, label]) => addBoolInput(grid, inputs, label, a.params[k], [k]));
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
    } else if (inp.tagName === "SELECT") {
      // 停在"未写"那一格时什么也不提交：把默认值钉成显式值，以后改默认就少了那只手。
      if (inp.value === "") continue;
      val = inp.value;
      if (val === String(initial ?? "")) continue;
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

// ---- 表单控件工厂：录音与抓屏两张表共用，否则"只修了一张表"会变成默认行为 ----
function addNumInput(grid, inputs, label, cur, path, effHint) {
  const l = document.createElement("label");
  l.appendChild(mk("span", null, label));
  const inp = document.createElement("input");
  inp.type = "number";
  inp.step = "any";
  inp.value = cur == null ? "" : String(cur);
  // 磁盘上没写的字段不是 0，是"用适配器默认"。把默认值当 0 填进去，下次保存就真把它钉住了。
  if (cur == null) inp.placeholder = effHint != null ? `未写·实际 ${effHint}` : "未写";
  l.appendChild(inp);
  grid.appendChild(l);
  inputs.push([path, inp, cur]);
  return inp;
}

function addSelect(grid, inputs, label, cur, path, options, effHint) {
  const l = document.createElement("label");
  l.appendChild(mk("span", null, label));
  const sel = document.createElement("select");
  // 比数字框更需要这一格：下拉框没写时看起来像"选了第一项"，而那是个真值。
  const opts = cur == null ? [null, ...options] : options;
  opts.forEach((o) => {
    const el = document.createElement("option");
    if (o === null) {
      el.value = "";
      el.textContent = effHint ? `未写·本节课实际 ${effHint}` : "未写（用默认）";
    } else {
      el.value = o;
      el.textContent = o;
    }
    sel.appendChild(el);
  });
  sel.value = cur == null ? "" : String(cur);
  l.appendChild(sel);
  grid.appendChild(l);
  inputs.push([path, sel, cur]);
  return sel;
}

function addBoolInput(grid, inputs, label, cur, path) {
  const l = document.createElement("label");
  l.className = "tune-check";
  const inp = document.createElement("input");
  inp.type = "checkbox";
  inp.checked = cur === true;
  l.appendChild(inp);
  l.appendChild(mk("span", null, label));
  grid.appendChild(l);
  inputs.push([path, inp, cur === true]);
  return inp;
}

function screenTuneRow(a, effPayload) {
  const tr = document.createElement("tr");
  const td = document.createElement("td");
  td.colSpan = 5;
  const box = mk("details", "tune");
  box.appendChild(mk("summary", null, `改采集参数 ${a.id}（写 adapters.d/${a.file}，下一节课生效）`));
  const body = document.createElement("div");
  const grid = mk("div", "tune-grid");
  const src = (effPayload && effPayload.sources && effPayload.sources[a.id]) || null;
  const eff = (src && src.screen && src.screen.screen) || null;
  const disk = (a.params && a.params.screen) || {};
  const inputs = [];
  SCREEN_SELECTS.forEach(([k, label, opts]) =>
    addSelect(grid, inputs, label, a.params[k], [k], opts, src && src.screen && src.screen.backend)
  );
  SCREEN_FIELDS.forEach(([k, label]) =>
    addNumInput(grid, inputs, label, disk[k], ["screen", k], eff ? eff[k] : null)
  );
  SCREEN_TOP_FIELDS.forEach(([k, label]) => addNumInput(grid, inputs, label, a.params[k], [k], null));
  SCREEN_BOOLS.forEach(([k, label]) =>
    addBoolInput(grid, inputs, label, disk[k], ["screen", k])
  );
  body.appendChild(grid);

  // 磁盘值 vs 本节课实际生效：不一致就直说"下一节课才生效"，别让教师自己猜。
  const effLine = document.createElement("div");
  effLine.className = "tune-eff";
  if (eff) {
    const same = SCREEN_FIELDS.every(([k]) => disk[k] == null || Number(disk[k]) === Number(eff[k]));
    effLine.textContent = `${S.sel || "本节课"} 实际生效：${SCREEN_FIELDS.map(([k]) => `${k}=${eff[k]}`).join(" ")}｜格子 ${eff.bbox || "?"}`;
    if (!same) {
      effLine.className = "tune-eff pending";
      effLine.textContent += "——与磁盘声明不一致：改好了，下一节课才生效";
    }
  } else {
    effLine.textContent = S.sel
      ? "本节课没报出自述参数（这一节没跑过这个源，或它不是屏幕源）"
      : "选一节课后，这里会列出本节课实际生效的门限";
  }
  body.appendChild(effLine);
  // 两道门限的关系不写在界面上，教师只会看见"我把 min_dist 调高了，帧数怎么反而没降"——
  // 因为它们是“或”：任一道过了就落盘。
  body.appendChild(mk(
    "div",
    "tune-eff",
    "两道门限是“或”的关系：任一过了就落盘。dHash 看不见“整块变亮变暗”，均差看不见“同位格换了内容”，所以两个各管一头；要真降帧数，两个都得抬。"
  ));
  body.appendChild(mk("div", "tune-eff", "poll_ms 只决定多久问一次屏（不越门限就不落盘）；真正拉高帧数的是 min_interval_ms 与两道门限。"));
  body.appendChild(mk("div", "tune-eff", "capture=auto 时 DXGI 起不来会退到 GDI，并在自述里说清楚；只指定一条后端时，它起不来就是整节课真的没帧。"));

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

loadCfg();
