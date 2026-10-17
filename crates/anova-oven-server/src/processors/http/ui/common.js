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
