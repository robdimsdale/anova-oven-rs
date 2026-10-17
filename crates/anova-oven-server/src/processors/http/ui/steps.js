// The stage editor, shared by the recipe editor and the manual cook on the
// status page. Needs common.js.
//
// It works in steps: a cook stage, optionally led by a preheat with the same
// settings, as Anova's app builds them. Recipes keep the oven's flat stage
// list; `toSteps`/`toStages` convert. Stage objects are edited in place, so
// fields the form doesn't show (IDs, fan, vent, rack, preheat titles)
// survive a save.

// The oven's limits, in Celsius (Anova's oven-commands docs).
const LIMITS = { dry: [25, 250], wet: [25, 100] };

// Settings a preheat shares with the cook stage it leads into.
const SHARED = [
  "temperature_c", "temperature_bulbs_mode", "steam_pct", "fan_speed",
  "heating_element_top", "heating_element_rear", "heating_element_bottom",
  "vent_open", "rack_position",
];

// The heating-element combinations the oven allows: any one or two, never
// all three.
const ELEMENTS = [
  ["rear", "Rear"],
  ["top", "Top"],
  ["bottom", "Bottom"],
  ["top+rear", "Top + rear"],
  ["bottom+rear", "Bottom + rear"],
  ["top+bottom", "Top + bottom"],
];

function toSteps(stages) {
  const out = [];
  for (let i = 0; i < stages.length; i++) {
    const stage = stages[i];
    const next = stages[i + 1];
    if (stage.kind === "preheat" && next && next.kind !== "preheat") {
      out.push({ preheat: stage, cook: next });
      i++;
    } else if (stage.kind === "preheat") {
      out.push({ preheat: stage, cook: null });
    } else {
      out.push({ preheat: null, cook: stage });
    }
  }
  return out;
}

function toStages(steps) {
  const stages = [];
  for (const step of steps) {
    if (step.preheat && step.cook) {
      for (const field of SHARED) {
        if (step.cook[field] === undefined) delete step.preheat[field];
        else step.preheat[field] = step.cook[field];
      }
    }
    for (const stage of [step.preheat, step.cook]) {
      if (!stage) continue;
      const copy = { ...stage };
      if (!copy.timer_added) delete copy.duration_secs;
      if (!copy.probe_added) delete copy.probe_target_c;
      stages.push(copy);
    }
  }
  return stages;
}

// The stage holding a step's settings, and the one the step starts with.
const mainStage = (step) => step.cook || step.preheat;
const firstStage = (step) => step.preheat || step.cook;

function newStep() {
  return {
    preheat: null,
    cook: {
      kind: "cook",
      // A round number in the unit on screen: 350 °F or 180 °C.
      temperature_c: displayUnit === "F" ? Math.round(fromDisplay(350) * 10) / 10 : 180,
      temperature_bulbs_mode: "dry",
      steam_pct: 0,
      fan_speed: 100,
      heating_element_top: false,
      heating_element_rear: true,
      heating_element_bottom: false,
      user_action_required: false,
      timer_added: false,
      probe_added: false,
    },
  };
}

function shown(celsius) {
  return Math.round(toDisplay(celsius));
}

function elementsOf(stage) {
  const on = ["top", "bottom", "rear"].filter((e) => stage["heating_element_" + e]);
  const key = on.join("+");
  return ELEMENTS.some(([k]) => k === key) ? key : null;
}

// How a cook stage ends. The oven takes a timer or a probe, not both; with
// neither, it holds until the next stage is started or the cook is stopped.
function endsOf(cook) {
  return cook.timer_added ? "timer" : cook.probe_added ? "probe" : "none";
}

function stepProblems(step) {
  const s = mainStage(step);
  const out = [];
  const mode = s.temperature_bulbs_mode === "wet" ? "wet" : "dry";
  const [lo, hi] = LIMITS[mode];
  const t = s.temperature_c;
  if (!(t >= lo - 0.05 && t <= hi + 0.05)) {
    out.push(`${mode === "wet" ? "Sous vide temperature" : "Temperature"} must be ${shown(lo)}–${shown(hi)}°${displayUnit}`);
  }
  if (!(s.steam_pct >= 0 && s.steam_pct <= 100)) out.push("Steam must be 0–100%");
  if (!elementsOf(s)) out.push("Pick the heating elements");
  if (step.cook && step.cook.timer_added && !(step.cook.duration_secs > 0)) {
    out.push("Set how long the timer runs");
  }
  if (step.cook && step.cook.probe_added) {
    const p = step.cook.probe_target_c;
    if (p == null || Number.isNaN(p)) out.push("Set the probe target");
    else if (!(p >= 0.95 && p <= 100.05)) out.push(`Probe target must be ${shown(1)}–${shown(100)}°${displayUnit}`);
  }
  return out;
}

function readNumber(input) {
  const value = Number(input.value);
  return input.value === "" || Number.isNaN(value) ? null : value;
}

// A stage editor in `container`. `name` keeps its radio groups apart from
// any other editor's; `onChange(valid)` runs after every edit.
function createStepEditor(container, { name, onChange = () => {} }) {
  let steps = [];

  function check(step, target, field, label, invert = false) {
    const stage = target === "first" ? firstStage(step) : target === "cook" ? step.cook : mainStage(step);
    const on = invert ? !stage[field] : !!stage[field];
    return `<label class="check"><input type="checkbox" data-field="${field}" data-target="${target}" ${invert ? "data-invert" : ""} ${on ? "checked" : ""}> ${label}</label>`;
  }

  function elementsHtml(stage, i) {
    const current = elementsOf(stage);
    return `
      <fieldset class="ends">
        <legend>Heating elements</legend>
        <div class="checks" style="margin-top: 0;">
          ${ELEMENTS.map(([key, label]) =>
            `<label class="check"><input type="radio" name="${name}-elements-${i}" data-elements value="${key}" ${current === key ? "checked" : ""}> ${label}</label>`).join("")}
        </div>
      </fieldset>`;
  }

  function endsHtml(step, i, duration, probe) {
    const current = endsOf(step.cook);
    const radio = (value, label) =>
      `<label class="check"><input type="radio" name="${name}-ends-${i}" data-ends value="${value}" ${current === value ? "checked" : ""}> ${label}</label>`;
    return `
      <fieldset class="ends">
        <legend>Ends</legend>
        <div class="checks" style="margin-top: 0;">
          ${radio("none", "Until I move on")}
          ${radio("timer", "Timer")}
          ${radio("probe", "Probe")}
        </div>
        ${current === "timer" ? `
        <div class="fields">
          <label>Minutes
            <input type="number" data-field="minutes" value="${Math.floor(duration / 60)}" min="0" step="1">
          </label>
          <label>Seconds
            <input type="number" data-field="seconds" value="${duration % 60}" min="0" max="59" step="1">
          </label>
        </div>` : ""}
        ${current === "probe" ? `
        <div class="fields">
          <label>Probe target (°${displayUnit})
            <input type="number" data-field="probe" value="${probe}" min="${shown(1)}" max="${shown(100)}" step="1">
            <span class="hint">${shown(1)}–${shown(100)}°${displayUnit}</span>
          </label>
        </div>` : ""}
      </fieldset>`;
  }

  function stepHtml(step, i, count) {
    const s = mainStage(step);
    const cook = step.cook;
    const mode = s.temperature_bulbs_mode === "wet" ? "wet" : "dry";
    const [lo, hi] = LIMITS[mode].map(shown);
    const duration = cook && cook.duration_secs ? cook.duration_secs : 0;
    const probe = cook && cook.probe_target_c != null ? shown(cook.probe_target_c) : "";
    return `
      <section class="card" data-index="${i}">
        <div class="stage-head">
          <span class="stage-number">${i + 1}</span>
          <input data-field="title" placeholder="Stage title (optional)" value="${escapeHtml(s.title || "")}">
          <button type="button" class="icon" data-move="-1" ${i === 0 ? "disabled" : ""} aria-label="Move up">↑</button>
          <button type="button" class="icon" data-move="1" ${i === count - 1 ? "disabled" : ""} aria-label="Move down">↓</button>
          <button type="button" class="icon danger" data-remove aria-label="Remove stage">✕</button>
        </div>
        <div class="fields">
          <label>Mode
            <select data-field="temperature_bulbs_mode">
              <option value="dry" ${mode === "dry" ? "selected" : ""}>Oven (dry bulb)</option>
              <option value="wet" ${mode === "wet" ? "selected" : ""}>Sous vide (wet bulb)</option>
            </select>
          </label>
          <label>Temperature (°${displayUnit})
            <input type="number" data-field="temperature" value="${shown(s.temperature_c)}" min="${lo}" max="${hi}" step="1">
            <span class="hint">${lo}–${hi}°${displayUnit}</span>
          </label>
          <label>Steam (%)
            <input type="number" data-field="steam_pct" value="${s.steam_pct}" min="0" max="100" step="1">
          </label>
        </div>
        ${elementsHtml(s, i)}
        <div class="checks">
          <label class="check"><input type="checkbox" data-preheat ${step.preheat ? "checked" : ""} ${cook ? "" : "disabled"}> Preheat first</label>
          ${check(step, "first", "user_action_required", "Start stage automatically", true)}
          ${step.preheat && cook ? check(step, "cook", "user_action_required", "Continue automatically after preheating", true) : ""}
        </div>
        ${cook ? endsHtml(step, i, duration, probe) : `<p class="hint" style="margin: 12px 0 0;">Preheat only: it ends when the oven reaches temperature</p>`}
        <ul class="problems"></ul>
      </section>`;
  }

  function render() {
    container.innerHTML = steps.map((s, i) => stepHtml(s, i, steps.length)).join("");
    validate();
  }

  // Refresh just the problem lists, so typing doesn't lose focus.
  function validate() {
    let ok = steps.length > 0;
    container.querySelectorAll("[data-index]").forEach((card) => {
      const list = stepProblems(steps[Number(card.dataset.index)]);
      card.querySelector(".problems").innerHTML = list.map((p) => `<li>${escapeHtml(p)}</li>`).join("");
      card.querySelector('[data-field="temperature"]')
        .setAttribute("aria-invalid", String(list.some((p) => p.includes("°"))));
      if (list.length) ok = false;
    });
    onChange(ok);
    return ok;
  }

  container.addEventListener("input", (event) => {
    const input = event.target;
    const card = input.closest("[data-index]");
    if (!card) return;
    const step = steps[Number(card.dataset.index)];
    const s = mainStage(step);

    if (input.hasAttribute("data-preheat")) {
      if (input.checked) {
        // The step now starts with the preheat: it takes over whether the
        // step starts on its own, and the cook carries straight on.
        step.preheat = { kind: "preheat", user_action_required: step.cook.user_action_required };
        step.cook.user_action_required = false;
      } else {
        step.cook.user_action_required = step.preheat.user_action_required;
        step.preheat = null;
      }
      render();
      return;
    }

    if (input.hasAttribute("data-elements")) {
      const on = input.value.split("+");
      for (const e of ["top", "bottom", "rear"]) s["heating_element_" + e] = on.includes(e);
      // The rear element is the convection one; the oven won't run it with the fan off.
      if (s.heating_element_rear && !(s.fan_speed > 0)) s.fan_speed = 100;
      validate();
      return;
    }

    if (input.hasAttribute("data-ends")) {
      step.cook.timer_added = input.value === "timer";
      step.cook.probe_added = input.value === "probe";
      render();
      return;
    }

    const field = input.dataset.field;
    if (!field) return;
    if (input.type === "checkbox") {
      const target = input.dataset.target;
      const stage = target === "first" ? firstStage(step) : target === "cook" ? step.cook : s;
      stage[field] = input.hasAttribute("data-invert") ? !input.checked : input.checked;
      validate();
      return;
    }
    switch (field) {
      case "title": s.title = input.value.trim() ? input.value : undefined; break;
      case "temperature_bulbs_mode":
        s.temperature_bulbs_mode = input.value;
        render();
        return;
      case "temperature": {
        const v = readNumber(input);
        s.temperature_c = v === null ? NaN : Math.round(fromDisplay(v) * 10) / 10;
        break;
      }
      case "probe": {
        const v = readNumber(input);
        step.cook.probe_target_c = v === null ? undefined : Math.round(fromDisplay(v) * 10) / 10;
        break;
      }
      case "steam_pct": s.steam_pct = readNumber(input) ?? NaN; break;
      case "minutes":
      case "seconds": {
        const minutes = readNumber(card.querySelector('[data-field="minutes"]')) ?? 0;
        const seconds = readNumber(card.querySelector('[data-field="seconds"]')) ?? 0;
        step.cook.duration_secs = Math.round(minutes * 60 + seconds);
        break;
      }
    }
    validate();
  });

  container.addEventListener("click", (event) => {
    const button = event.target.closest("button");
    const card = event.target.closest("[data-index]");
    if (!button || !card) return;
    const i = Number(card.dataset.index);
    if (button.dataset.move) {
      const j = i + Number(button.dataset.move);
      [steps[i], steps[j]] = [steps[j], steps[i]];
      render();
    } else if (button.hasAttribute("data-remove")) {
      steps.splice(i, 1);
      render();
    }
  });

  return {
    setStages(stages) {
      steps = toSteps(stages);
      render();
    },
    reset() {
      steps = [newStep()];
      render();
    },
    addStep() {
      steps.push(newStep());
      render();
    },
    stages: () => toStages(steps),
    steps: () => steps,
    render,
    validate,
  };
}
