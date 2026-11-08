// Shared helpers for the UI pages. Everything goes through the JSON API
// under /api; temperatures there are Celsius.

async function api(method, path, body) {
  const options = { method, headers: {} };
  if (body !== undefined) {
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  const response = await fetch("/api" + path, options);
  const text = await response.text();
  if (!response.ok) {
    throw new Error(text || `${method} ${path}: HTTP ${response.status}`);
  }
  return text ? JSON.parse(text) : null;
}

// The unit temperatures show in, chosen with the °F/°C switch in the header
// and remembered in this browser. Fahrenheit until changed.
const UNIT_KEY = "anova-oven-unit";
let displayUnit = "F";
try {
  if (localStorage.getItem(UNIT_KEY) === "C") displayUnit = "C";
} catch (_) {}

// Wire up the header's unit switch. `onChange` re-renders the page.
function initUnitSwitch(onChange) {
  const buttons = document.querySelectorAll("[data-unit]");
  const sync = () => buttons.forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.unit === displayUnit)));
  buttons.forEach((b) => {
    b.onclick = () => {
      displayUnit = b.dataset.unit;
      try { localStorage.setItem(UNIT_KEY, displayUnit); } catch (_) {}
      sync();
      onChange();
    };
  });
  sync();
}

function toDisplay(celsius) {
  return displayUnit === "F" ? celsius * 1.8 + 32 : celsius;
}

function fromDisplay(value) {
  return displayUnit === "F" ? (value - 32) / 1.8 : value;
}

function formatTemp(celsius) {
  if (celsius === null || celsius === undefined) return "—";
  return `${Math.round(toDisplay(celsius))}°${displayUnit}`;
}

function formatDuration(secs) {
  if (secs === null || secs === undefined) return "—";
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const mm = String(m).padStart(h ? 2 : 1, "0");
  const ss = String(s).padStart(2, "0");
  return h ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}

function showMessage(el, text, kind = "error") {
  el.className = `message ${kind}`;
  el.textContent = text;
}

function clearMessage(el) {
  el.textContent = "";
}

function escapeHtml(text) {
  return String(text).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

// ─── Start in / Start at ────────────────────────────────────────────────────

// When a scheduled cook starts, in this browser's time: "16:30 today (in
// 25 min)".
function formatStartsAt(at) {
  const when = new Date(at);
  const time = when.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  const day = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((day(when) - day(new Date())) / 86400000);
  const date = days === 0 ? "today" : days === 1 ? "tomorrow"
    : when.toLocaleDateString([], { weekday: "short", day: "numeric", month: "short" });
  const mins = Math.max(0, Math.round((when - Date.now()) / 60000));
  const left = mins < 60 ? `${mins} min` : `${Math.floor(mins / 60)} h ${mins % 60} min`;
  return `${time} ${date} (in ${left})`;
}

// Ask when to start: `kind` is "in" or "at". Resolves to the fields to add
// to a POST /start, or null if cancelled. "Start at" sends the time as
// typed, with this browser's zone so the server can refuse if its own
// differs.
function askStartLater(kind, title) {
  return new Promise((resolve) => {
    const dialog = document.createElement("dialog");
    dialog.className = "start-later";
    const input = kind === "in"
      ? '<label>Start in (minutes)<input name="when" type="number" min="1" max="1440" step="1" value="30" required></label>'
      : '<label>Start at<input name="when" type="time" required></label>';
    dialog.innerHTML = `
      <form method="dialog">
        <h2>${kind === "in" ? "Start in" : "Start at"}</h2>
        <p class="muted small" style="margin: 0 0 12px;">${escapeHtml(title)}</p>
        ${input}
        <p class="hint" style="margin: 8px 0 0;">The oven turns on then. Food left out before a cook should be safe to sit that long.</p>
        <div class="row" style="margin-top: 16px; justify-content: flex-end;">
          <button type="button" value="cancel">Cancel</button>
          <button type="submit" class="primary" value="ok">${kind === "in" ? "Start in" : "Start at"}</button>
        </div>
      </form>`;
    const field = dialog.querySelector("input");
    if (kind === "at") {
      const soon = new Date(Date.now() + 60 * 60000);
      field.value = `${String(soon.getHours()).padStart(2, "0")}:00`;
    }
    const done = (value) => { dialog.close(); dialog.remove(); resolve(value); };
    dialog.querySelector('button[value="cancel"]').onclick = () => done(null);
    dialog.addEventListener("cancel", () => done(null));
    dialog.querySelector("form").onsubmit = (event) => {
      event.preventDefault();
      if (!field.reportValidity()) return;
      if (kind === "in") done({ start_in_minutes: Number(field.value) });
      else done({ start_at: field.value, time_zone: Intl.DateTimeFormat().resolvedOptions().timeZone });
    };
    document.body.appendChild(dialog);
    dialog.showModal();
    field.focus();
  });
}

// "Start in" or "Start at" `body` (a POST /start for a recipe or a manual
// cook). Offers to replace a cook already scheduled. Resolves to the
// scheduled cook, or null if not scheduled.
async function startLater(kind, title, body) {
  const when = await askStartLater(kind, title);
  if (!when) return null;
  try {
    return await api("POST", "/start", { ...body, ...when });
  } catch (e) {
    if (!/already set to start/.test(e.message)) throw e;
    const existing = await api("GET", "/scheduled");
    const what = existing ? `${existing.title} is already set to start at ${formatStartsAt(existing.at)}` : "A cook is already scheduled";
    if (!confirm(`${what}. Replace it?`)) return null;
    return await api("POST", "/start", { ...body, ...when, replace: true });
  }
}
