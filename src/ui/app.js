"use strict";
import { findMatches } from "./search.mjs";
const $ = (selector) => document.querySelector(selector);
const token = location.hash.slice(1) || sessionStorage.getItem("cryo-token");
if (token) sessionStorage.setItem("cryo-token", token);
history.replaceState(null, "", location.pathname);
const state = {
  offset: 0,
  total: 0,
  selectedId: null,
  sessions: [],
  currentMatches: [],
  currentMatch: 0,
  requestSequence: 0,
};
async function fetchJson(path, params = {}) {
  const query = new URLSearchParams(params);
  const response = await fetch(`${path}?${query}`, {
    headers: { "X-Cryo-Token": token },
    cache: "no-store",
  });
  if (!response.ok) {
    let msg;
    try {
      msg = (await response.json()).error;
    } catch {}
    throw new Error(msg || `Request failed (${response.status})`);
  }
  return response.json();
}
function createElement(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}
function formatDate(value) {
  if (value == null) return "Unknown date";
  const timestamp = new Date(value * 1000);
  return Number.isNaN(timestamp.getTime())
    ? "Unknown date"
    : timestamp.toLocaleDateString(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
        timeZone: "UTC",
      });
}
function sourceLabel(source) {
  return source || "Unknown source";
}
function getSearchFilters() {
  return {
    q: $("#query").value.trim(),
    source: $("#source").value,
    after: $("#after").value,
    before: $("#before").value,
    offset: String(state.offset),
  };
}
function showError(container, error) {
  container.replaceChildren(
    createElement(
      "p",
      "notice error",
      error.message || "Something went wrong.",
    ),
  );
}
async function loadSessions(reset = true) {
  if (reset) {
    state.offset = 0;
    state.sessions = [];
    $("#count").textContent = "";
    $("#more").hidden = true;
    $("#list").replaceChildren(
      createElement("p", "notice", "Loading conversations…"),
    );
  }
  const requestSequence = ++state.requestSequence;
  try {
    const data = await fetchJson("/api/sessions", getSearchFilters());
    if (requestSequence !== state.requestSequence) return;
    state.total = data.total;
    state.sessions.push(...data.items);
    state.offset = state.sessions.length;
    renderList();
    $("#count").textContent = `${data.total.toLocaleString()} found`;
    $("#more").hidden = state.offset >= state.total;
  } catch (error) {
    if (requestSequence === state.requestSequence) {
      $("#count").textContent = "";
      $("#more").hidden = true;
      showError($("#list"), error);
    }
  }
}
function renderList() {
  const list = $("#list");
  list.replaceChildren();
  if (!state.sessions.length) {
    list.append(
      createElement(
        "p",
        "notice",
        "No conversations match. Try another search or filter.",
      ),
    );
    return;
  }
  for (const session of state.sessions) {
    const button = createElement(
      "button",
      "session" + (session.id === state.selectedId ? " selected" : ""),
    );
    button.type = "button";
    button.append(
      createElement("strong", "", session.title || "Untitled"),
      createElement(
        "span",
        "meta",
        `${sourceLabel(session.source)} · ${formatDate(session.created_at)}`,
      ),
      createElement("span", "preview", session.preview || "No message preview"),
    );
    button.title = `${session.title || "Untitled"}\n${sourceLabel(session.source)} · ${formatDate(session.created_at)} · ${session.message_count} messages\n${session.preview || ""}`;
    button.addEventListener("click", () => openSession(session.id));
    list.append(button);
  }
}
async function openSession(id) {
  state.selectedId = id;
  activeMessages = [];
  state.currentMatches = [];
  state.currentMatch = 0;
  renderList();
  $("#empty").hidden = true;
  $("#conversation").hidden = false;
  $("#title").textContent = "Loading conversation…";
  $("#messages").replaceChildren();
  $("#subtitle").textContent = "";
  $("#breadcrumb").textContent = "";
  $("#find-count").textContent = "";
  $("#details").hidden = true;
  $("#find-panel").hidden = true;
  $("#find").value = "";
  $("#find-toggle").disabled = true;
  $("#details-toggle").disabled = true;
  try {
    const session = await fetchJson("/api/session", { id });
    if (state.selectedId !== id) return;
    if (!session) throw new Error("This conversation could not be found.");
    $("#title").textContent = session.title || "Untitled";
    $("#breadcrumb").textContent =
      `CONVERSATIONS / ${sourceLabel(session.source).toUpperCase()}`;
    $("#subtitle").textContent =
      `${formatDate(session.created_at)} · ${session.messages.length} messages`;
    renderDetails(session);
    renderMessages(session.messages);
    $("#find-toggle").disabled = false;
    $("#details-toggle").disabled = false;
  } catch (error) {
    if (state.selectedId === id) {
      $("#title").textContent = "Unable to open conversation";
      showError($("#messages"), error);
    }
  }
}
function renderDetails(session) {
  const details = $("#details");
  details.replaceChildren();
  const list = createElement("dl");
  let metadata = {};
  try {
    metadata = JSON.parse(session.metadata_json || "{}");
  } catch {}
  if (
    metadata === null ||
    typeof metadata !== "object" ||
    Array.isArray(metadata)
  ) {
    metadata = {};
  }
  const fields = [
    ["Source", sourceLabel(session.source)],
    ["Date", formatDate(session.created_at)],
    ["Messages", String(session.messages.length)],
    ["Model", session.model || "Not recorded"],
    ["Session ID", session.id],
  ];
  for (const [key, value] of Object.entries(metadata)) {
    if (value != null) {
      fields.push([
        key,
        typeof value === "object" ? JSON.stringify(value) : String(value),
      ]);
    }
  }
  for (const [label, value] of fields) {
    list.append(createElement("dt", "", label), createElement("dd", "", value));
  }
  details.append(list);
}
function appendHighlighted(parent, text) {
  const term = $("#find").value;
  if (!term) {
    parent.append(document.createTextNode(text));
    return;
  }
  let start = 0;
  for (const match of findMatches(text, term)) {
    parent.append(document.createTextNode(text.slice(start, match.start)));
    const mark = createElement("mark", "", text.slice(match.start, match.end));
    parent.append(mark);
    state.currentMatches.push(mark);
    start = match.end;
  }
  parent.append(document.createTextNode(text.slice(start)));
}
function renderCodeBlock(code, lang) {
  const wrapper = createElement("div", "code"),
    head = createElement("div", "code-head");
  head.append(createElement("span", "", lang || "CODE"));
  const copy = createElement("button", "", "Copy");
  copy.type = "button";
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(code);
      copy.textContent = "Copied";
    } catch {
      copy.textContent = "Copy unavailable";
    }
  });
  head.append(copy);
  const pre = createElement("pre"),
    node = createElement("code");
  const re =
    /(#[^\n]*|\/\/[^\n]*|"[^"\n]*"|'[^'\n]*'|\b(?:const|let|fn|pub|return|function|async|await|if|else|true|false|import|from)\b)/g;
  let pos = 0,
    m;
  while ((m = re.exec(code))) {
    appendHighlighted(node, code.slice(pos, m.index));
    const span = createElement(
      "span",
      m[0][0] === "#" || m[0].startsWith("//")
        ? "comment"
        : m[0][0] === '"' || m[0][0] === "'"
          ? "str"
          : "kw",
    );
    appendHighlighted(span, m[0]);
    node.append(span);
    pos = re.lastIndex;
  }
  appendHighlighted(node, code.slice(pos));
  pre.append(node);
  wrapper.append(head, pre);
  return wrapper;
}
function renderMessageContent(content) {
  const out = document.createDocumentFragment();
  const chunks = String(content || "").split(/```([^\n`]*)\n([\s\S]*?)```/g);
  for (let i = 0; i < chunks.length; i += 3) {
    if (chunks[i]) {
      const p = createElement("div", "content");
      appendHighlighted(p, chunks[i]);
      out.append(p);
    }
    if (i + 2 < chunks.length) {
      out.append(renderCodeBlock(chunks[i + 2], chunks[i + 1].trim()));
    }
  }
  return out;
}
let activeMessages = [];
function renderMessages(messages) {
  activeMessages = messages;
  state.currentMatches = [];
  const target = $("#messages");
  target.replaceChildren();
  if (!messages.length) {
    target.append(
      createElement("p", "notice", "This conversation has no messages."),
    );
    return;
  }
  for (const msg of messages) {
    const wrap = createElement("div", "message"),
      author = createElement("div", "author"),
      role =
        msg.role === "model"
          ? "Assistant"
          : msg.role === "user"
            ? "You"
            : msg.role || "Message";
    author.append(
      createElement(
        "span",
        "avatar " + (msg.role === "model" ? "model" : ""),
        role.charAt(0).toUpperCase(),
      ),
      document.createTextNode(role),
    );
    wrap.append(author, renderMessageContent(msg.content));
    if (msg.tool_calls?.length || msg.tool_outputs?.length) {
      const detail = createElement("details");
      detail.append(createElement("summary", "", "Tool activity"));
      const pre = createElement(
        "pre",
        "content",
        JSON.stringify(
          { calls: msg.tool_calls || [], outputs: msg.tool_outputs || [] },
          null,
          2,
        ),
      );
      detail.append(pre);
      wrap.append(detail);
    }
    target.append(wrap);
  }
  $("#find-count").textContent = state.currentMatches.length
    ? `${state.currentMatch + 1} of ${state.currentMatches.length}`
    : $("#find").value
      ? "No matches"
      : "";
  if (state.currentMatches.length) {
    state.currentMatch = Math.min(
      state.currentMatch,
      state.currentMatches.length - 1,
    );
    state.currentMatches[state.currentMatch].classList.add("current");
  }
}
function navigateMatches(delta) {
  if (!state.currentMatches.length) return;
  state.currentMatches[state.currentMatch].classList.remove("current");
  state.currentMatch =
    (state.currentMatch + delta + state.currentMatches.length) %
    state.currentMatches.length;
  state.currentMatches[state.currentMatch].classList.add("current");
  state.currentMatches[state.currentMatch].scrollIntoView({ block: "center" });
  $("#find-count").textContent =
    `${state.currentMatch + 1} of ${state.currentMatches.length}`;
}
let timer;
$("#query").addEventListener("input", () => {
  clearTimeout(timer);
  timer = setTimeout(() => loadSessions(), 250);
});
for (const id of ["source", "after", "before"]) {
  $(`#${id}`).addEventListener("change", () => loadSessions());
}
$("#more").addEventListener("click", () => loadSessions(false));
$("#refresh").addEventListener("click", () => {
  loadSessions();
  loadStats();
  loadSources();
});
$("#find-toggle").addEventListener("click", () => {
  $("#find-panel").hidden = !$("#find-panel").hidden;
  if (!$("#find-panel").hidden) $("#find").focus();
  else {
    $("#find").value = "";
    state.currentMatch = 0;
    renderMessages(activeMessages);
  }
});
$("#details-toggle").addEventListener("click", () => {
  $("#details").hidden = !$("#details").hidden;
});
$("#find").addEventListener("input", () => {
  state.currentMatch = 0;
  renderMessages(activeMessages);
});
$("#next").addEventListener("click", () => navigateMatches(1));
$("#previous").addEventListener("click", () => navigateMatches(-1));
async function loadStats() {
  try {
    const s = await fetchJson("/api/stats");
    $("#stats").textContent =
      `${s.session_count.toLocaleString()} conversations · ${s.message_count.toLocaleString()} messages`;
  } catch (e) {
    $("#stats").textContent = "Archive unavailable";
    $("#stats").title = e.message;
  }
}
async function loadSources() {
  const select = $("#source");
  try {
    const sources = await fetchJson("/api/sources");
    const selectedSource = select.value;
    select.replaceChildren(createElement("option", "", "All sources"));
    select.firstElementChild.value = "";
    for (const source of sources) {
      const option = createElement("option", "", source);
      option.value = source;
      select.append(option);
    }
    select.value = sources.includes(selectedSource) ? selectedSource : "";
    select.disabled = false;
    select.removeAttribute("title");
  } catch (error) {
    select.disabled = true;
    select.title = `Could not load sources: ${error.message}`;
  }
}
if (!token) {
  $("#list").replaceChildren(
    createElement(
      "p",
      "notice error",
      "Open the complete URL printed by cryo-vault-ui.",
    ),
  );
  $("#stats").textContent = "Access token missing";
} else {
  loadSessions();
  loadStats();
  loadSources();
}
