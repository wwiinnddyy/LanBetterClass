// agent · 课堂观察端前端逻辑。零构建，纯静态；HTTP 走 Tauri 命令（无 CORS）。
const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);

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
  $("#lessonTitle").textContent = id;
  $("#lessonDigest").textContent = "加载中…";
  $("#lessonStats").innerHTML = "";
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

loadCfg();
