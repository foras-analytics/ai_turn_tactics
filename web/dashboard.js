const byId = (id) => document.getElementById(id);
const form = byId("match-form");
const field = (name) => form.elements.namedItem(name);
const palette = ["#def67b", "#80e5dc", "#ffa87e", "#c5abf7", "#f5cb66", "#8ebdff", "#eeb0cf", "#9be1a3"];

let defaults;
let view = "live";
let info;
let status;
let inference;
let frames = [];
let position = 0;
let selected = null;
let follow = true;
let playback = null;
let liveId;
let generation = 0;
let pollGeneration;
const promptDrafts = [];
const modelOverrides = [];
let installedModels = [];
let preferredSource;
let defaultModel;
let modelStatus = "";
let discovery = 0;
let launching = false;

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}

async function request(path, options) {
  const response = await fetch(path, options);
  const isJson = response.headers.get("content-type")?.includes("application/json");
  const body = isJson ? await response.json() : await response.text();
  if (!response.ok) throw new Error(typeof body === "string" ? body : body.error);
  return body;
}

function notice(message) {
  byId("notice").textContent = message ?? "";
  byId("notice").hidden = !message;
}

function showView(next) {
  view = next;
  generation += 1;
  info = undefined;
  status = undefined;
  inference = undefined;
  liveId = undefined;
  frames = [];
  position = 0;
  follow = next === "live";
  selected = null;
  stopPlayback();
  notice();
  for (const name of ["live", "replays"]) {
    const tab = byId(name + "-tab");
    tab.classList.toggle("nav-current", name === next);
    if (name === next) tab.setAttribute("aria-current", "page");
    else tab.removeAttribute("aria-current");
  }
  byId("title").textContent = next === "live" ? "Match control" : "Match replays";
  byId("subtitle").textContent = next === "live"
    ? "Launch a match and inspect every move."
    : "Step through a saved match and inspect player decisions.";
  byId("archive").hidden = next === "live";
  byId("archive-select").replaceChildren();
  render();
}

function color(player) {
  return palette[(player - 1) % palette.length];
}

function colored(node, player) {
  node.style.setProperty("--tank", color(player));
  return node;
}

function focusPlayer(id) {
  selected = id;
  render();
}

let gridSide = 0;
let cells = [];
let boardKey;
function renderBoard(frame) {
  const side = frame?.board_size ?? 10;
  const grid = byId("grid");
  if (side !== gridSide) {
    gridSide = side;
    boardKey = undefined;
    const coordinates = byId("coordinates");
    coordinates.style.setProperty("--side", side);
    grid.style.setProperty("--side", side);
    coordinates.replaceChildren(...Array.from({ length: side }, (_, x) => element("span", String(x))));
    cells = Array.from({ length: side * side }, (_, index) => {
      const x = index % side;
      const y = Math.floor(index / side);
      const cell = element("div", undefined, "cell");
      if ((x + y) % 2) cell.classList.add("alt");
      if (x === side - 1) cell.classList.add("axis-right");
      if (y === side - 1) cell.classList.add("axis-bottom");
      return cell;
    });
    const fragment = document.createDocumentFragment();
    for (const cell of cells) fragment.append(cell);
    grid.replaceChildren(fragment);
  }
  const tanks = frame?.players ?? [];
  const inFocus = tanks.find(({ id, health }) => id === selected && health > 0);
  const key = JSON.stringify([selected, inFocus?.range, tanks.map(({ id, position, health }) => [id, position, health > 0])]);
  if (key === boardKey) return;
  boardKey = key;
  const label = byId("range-label");
  label.hidden = !inFocus;
  label.textContent = inFocus ? `Tank ${inFocus.id} · Attack and gift range ${inFocus.range}` : "";
  if (inFocus) grid.style.setProperty("--reach", color(inFocus.id));
  // Match the engine's Manhattan distance on the open grid.
  cells.forEach((cell, index) => {
    const x = index % side;
    const y = Math.floor(index / side);
    const reached = inFocus && Math.abs(inFocus.position.x - x) + Math.abs(inFocus.position.y - y) <= inFocus.range;
    cell.classList.toggle("in-range", Boolean(reached));
  });
  for (const cell of grid.querySelectorAll(".tank")) {
    cell.classList.remove("tank", "wreck");
    cell.replaceChildren();
  }
  for (const player of tanks) {
    const { id, position: { x, y }, health } = player;
    const cell = colored(cells[y * side + x], id);
    cell.classList.add("tank");
    if (!health) cell.classList.add("wreck");
    const button = element("button", String(id));
    button.type = "button";
    button.setAttribute("aria-label", `${health ? "Tank" : "Wreck"} ${id} at ${x}, ${y}`);
    button.setAttribute("aria-pressed", String(id === selected));
    button.onclick = () => focusPlayer(id);
    cell.append(button);
  }
}

function renderRoster(players) {
  byId("player-count").textContent = players.length ? `${players.length} slots` : "—";
  if (!players.length) {
    byId("roster").replaceChildren(element("p", "No players deployed.", "meta"));
    return;
  }
  byId("roster").replaceChildren(...players.map((player) => {
    const row = colored(element("button", undefined, "player-row"), player.id);
    row.type = "button";
    row.dataset.wreck = String(player.health === 0);
    row.setAttribute("aria-pressed", String(player.id === selected));
    row.append(
      element("span", String(player.id), "token"),
      element("span", `Tank ${player.id}`),
      element("span", player.health ? `${player.health} HP · ${player.action_points} AP` : "Jury", "readings"),
    );
    row.onclick = () => focusPlayer(player.id);
    return row;
  }));
}

const words = (value) => value.replaceAll("_", " ");
function actionText(action) {
  const target = action.target === undefined ? "" : ` · Tank ${action.target}`;
  const amount = action.amount === undefined ? "" : ` × ${action.amount}`;
  const destination = action.destination ? ` · (${action.destination.x}, ${action.destination.y})` : "";
  return words(action.type) + target + amount + destination;
}

function renderInspector(frame) {
  const player = frame?.players.find(({ id }) => id === selected);
  byId("selected").textContent = player ? `Tank ${player.id}` : "—";
  if (!player) {
    byId("inspector").textContent = "Select a player to view their private state.";
    return;
  }
  const rooms = frame.rooms.filter((room) => room.members.includes(player.id));
  const values = [
    ["Position", `${player.position.x}, ${player.position.y}`],
    ["Range", player.range],
    ["Jury vote", player.vote ? `Tank ${player.vote}` : "—"],
    ["Jury bonuses", player.jury_bonus_ticks.join(", ") || "—"],
    ["Wake-up", [player.wake_at ? `tick ${player.wake_at}` : null, ...player.wake_events.map(words)].filter(Boolean).join(", ") || "—"],
    ["Pending orders", player.pending_orders.map((order) => `${order.tick} · ${actionText(order.action)}`).join("\n") || "—"],
    ["Peace", player.peace.map((peace) => `${peace.members.join(", ")} · ${peace.public ? "public" : "private"}`).join("\n") || "—"],
    ["Rooms", rooms.map((room) => `${room.id} · ${room.members.join(", ")}`).join("\n") || "—"],
    ["Memory", player.memory || "—"],
    ["Model", info?.player_models?.[player.id - 1] ?? "Demo player"],
    ["Starting prompt", info?.config.players[player.id - 1]?.starting_prompt || "—"],
  ];
  const list = element("dl");
  for (const [label, value] of values) list.append(element("dt", label), element("dd", String(value)));
  byId("inspector").replaceChildren(list);
}

function eventText(event) {
  const { data, player } = event;
  switch (event.kind) {
    case "message": {
      const audience = data.channel === "public" ? "public"
        : data.channel === "direct" ? `to Tank ${data.recipients[0]}` : `room ${data.room}`;
      return `Tank ${data.sender} · ${audience} · ${data.text}`;
    }
    case "public_event":
      return data.type === "ap_round" ? "AP grants issued"
        : `Tank ${data.player} ${data.active ? "declared" : "revoked"} public peace · ${data.members.join(", ")}`;
    case "damage":
      return `Tank ${player} took ${data.damage} damage${data.lethal ? " and was eliminated" : ""} · from ${[...new Set(data.attackers ?? [])].join(", ")}`;
    case "order": {
      const outcome = data.outcome;
      const result = outcome.status === "failed" ? words(outcome.reason)
        : outcome.status === "transferred" ? `${outcome.transferred} transferred` : words(outcome.status);
      return `Tank ${player} · ${actionText(data.action)} · ${result}`;
    }
    case "reply":
      return `Tank ${player} · ${data.status === "retry" ? "Retrying reply" : "Passed after retry"} · ${data.error}`;
    default:
      return "Match event";
  }
}

let activityKey;
function renderActivity(frame) {
  const key = frame ? `${info?.id}/${position}` : "";
  if (key === activityKey) return;
  activityKey = key;
  const entries = frame ? frames.slice(0, position + 1).flatMap((entry) => entry.log) : [];
  byId("activity-count").textContent = entries.length ? `${entries.length} events` : "—";
  const feed = byId("activity");
  const fragment = document.createDocumentFragment();
  if (!entries.length) fragment.append(element("li", "Match activity will appear here."));
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    const event = entries[index];
    const row = element("li");
    row.append(element("time", `T${event.data.tick}`), document.createTextNode(eventText(event)));
    fragment.append(row);
  }
  feed.replaceChildren(fragment);
  feed.scrollTop = 0;
}

function resultText(outcome) {
  if (!outcome) return "";
  if (outcome.type === "win") return `Win · ${outcome.players.map((id) => `Tank ${id}`).join(", ")}`;
  return outcome.reason === "round_limit" ? "Draw · round limit" : "Draw · mutual destruction";
}

function render() {
  const frame = frames[position];
  const players = frame?.players ?? [];
  const badge = byId("state");
  badge.className = `state ${status ?? ""}`;
  const liveLabel = inference
    ? `Tank ${inference.player} ${inference.retry ? "retrying" : "thinking"} · T${inference.tick}`
    : (info?.player_models ? "Ollama live" : "Demo live");
  badge.textContent = resultText(frame?.outcome)
    || (view === "replays" && info ? "Replay" : { running: liveLabel, complete: "Complete", failed: "Match error" }[status])
    || "Standby";
  byId("tick").textContent = frame?.tick ?? "—";
  byId("round").textContent = frame && info ? Math.floor(frame.tick / info.config.ap_interval) : "—";
  byId("standing").textContent = frame ? `${players.filter(({ health }) => health > 0).length} / ${players.length}` : "—";
  byId("empty").hidden = Boolean(frame);
  byId("empty-title").textContent = view === "live" ? "Awaiting a match" : "Choose a replay";
  byId("empty-detail").textContent = view === "live"
    ? "Configure a match to deploy players."
    : "Saved matches can be inspected frame by frame.";
  byId("position").max = Math.max(frames.length - 1, 0);
  byId("position").value = position;
  byId("position").disabled = frames.length < 2;
  byId("position").setAttribute("aria-valuetext", frame ? `Tick ${frame.tick}` : "No frames");
  byId("previous").disabled = position === 0;
  byId("next").disabled = position >= frames.length - 1;
  byId("play").disabled = frames.length < 2;
  byId("play").textContent = playback ? "Pause" : "Play";
  byId("follow").hidden = view !== "live";
  byId("follow").disabled = follow || !frames.length;
  byId("setup-button").hidden = view !== "live";
  byId("setup-button").disabled = !defaults || status === "running";
  renderBoard(frame);
  renderRoster(players);
  renderInspector(frame);
  renderActivity(frame);
}

function stopPlayback() {
  if (playback !== null) clearInterval(playback);
  playback = null;
}

function seek(index) {
  follow = false;
  position = Math.min(Math.max(index, 0), Math.max(frames.length - 1, 0));
  render();
}

byId("previous").onclick = () => { stopPlayback(); seek(position - 1); };
byId("next").onclick = () => { stopPlayback(); seek(position + 1); };
byId("position").oninput = (event) => { stopPlayback(); seek(Number(event.target.value)); };
byId("play").onclick = () => {
  if (playback) {
    stopPlayback();
  } else {
    follow = false;
    if (position === frames.length - 1) position = 0;
    playback = setInterval(() => {
      if (position >= frames.length - 1) stopPlayback();
      else position += 1;
      render();
    }, 240);
  }
  render();
};
byId("follow").onclick = () => {
  stopPlayback();
  follow = true;
  position = Math.max(frames.length - 1, 0);
  render();
};

async function poll() {
  if (view === "live" && pollGeneration !== generation) {
    const expected = generation;
    pollGeneration = expected;
    const query = liveId ? `?id=${encodeURIComponent(liveId)}&after=${frames.length}` : "";
    try {
      const data = await request("/api/live" + query);
      if (expected === generation && view === "live" && data) {
        const changed = liveId !== data.info.id || status !== data.status || data.frames.length > 0
          || inference?.player !== data.inference?.player || inference?.retry !== data.inference?.retry
          || inference?.tick !== data.inference?.tick;
        info = data.info;
        status = data.status;
        inference = data.inference;
        if (liveId !== data.info.id) {
          liveId = info.id;
          frames = [];
          position = 0;
          selected = null;
          activityKey = undefined;
        }
        if (data.frames.length || data.offset !== frames.length) {
          frames = frames.slice(0, data.offset).concat(data.frames);
        }
        if (follow) position = Math.max(frames.length - 1, 0);
        notice(data.error);
        if (changed) render();
      }
    } catch (error) {
      if (expected === generation) notice(`Could not refresh the match. ${error.message}`);
    } finally {
      if (pollGeneration === expected) pollGeneration = undefined;
    }
  }
}

async function openReplay(id) {
  const expected = ++generation;
  stopPlayback();
  try {
    const data = await request("/api/replays/" + encodeURIComponent(id));
    if (expected !== generation) return;
    info = data.info;
    status = data.status;
    frames = data.frames;
    position = 0;
    selected = null;
    activityKey = undefined;
    notice();
    render();
  } catch (error) {
    if (expected === generation) notice(error.message);
  }
}

byId("live-tab").onclick = () => { showView("live"); void poll(); };
byId("replays-tab").onclick = async () => {
  showView("replays");
  const expected = generation;
  try {
    const saved = await request("/api/replays");
    if (expected !== generation) return;
    const select = byId("archive-select");
    select.disabled = !saved.length;
    if (saved.length) {
      const date = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "medium" });
      for (const replay of saved) {
        const option = element("option", date.format(replay.created_ms));
        option.value = replay.id;
        select.append(option);
      }
      await openReplay(saved[0].id);
    } else {
      select.append(element("option", "No saved matches"));
    }
  } catch (error) {
    if (expected === generation) notice(error.message);
  }
};
byId("archive-select").onchange = (event) => { void openReplay(event.target.value); };

function resizePrompts() {
  const container = byId("prompts");
  const count = Number(field("players").value);
  if (!Number.isSafeInteger(count) || !field("players").validity.valid) return;
  container.replaceChildren();
  const local = field("player_source").value === "ollama";
  for (let index = 0; index < count; index += 1) {
    const row = element("div", undefined, "player-settings");
    if (local) {
      const modelLabel = element("label", `Tank ${index + 1} model`);
      const modelSelect = element("select");
      modelSelect.append(element("option", "Use default model"));
      modelSelect.firstChild.value = "";
      for (const model of installedModels) modelSelect.append(element("option", model));
      modelSelect.value = installedModels.includes(modelOverrides[index]) ? modelOverrides[index] : "";
      modelSelect.onchange = () => { modelOverrides[index] = modelSelect.value; };
      modelLabel.append(modelSelect);
      row.append(modelLabel);
    }
    const label = element("label", `Tank ${index + 1} starting prompt`);
    const prompt = element("textarea");
    prompt.rows = 2;
    prompt.value = promptDrafts[index] ?? "";
    prompt.oninput = () => { promptDrafts[index] = prompt.value; };
    label.append(prompt);
    row.append(label);
    container.append(row);
  }
}

function renderSource() {
  const local = field("player_source").value === "ollama";
  byId("default-model-field").hidden = !local;
  const picker = field("default_model");
  picker.replaceChildren(...installedModels.map((model) => element("option", model)));
  picker.value = defaultModel ?? "";
  byId("ollama-status").textContent = modelStatus;
  byId("ollama-status").hidden = !local || !modelStatus;
  updateLaunchButton();
  resizePrompts();
}

function updateLaunchButton() {
  const local = field("player_source").value === "ollama";
  byId("launch").disabled = launching || (local && !installedModels.includes(defaultModel));
}

async function discoverModels() {
  const expected = ++discovery;
  modelStatus = "Checking local Ollama…";
  installedModels = [];
  field("player_source").value = preferredSource ?? "ollama";
  renderSource();
  try {
    const found = await request("/api/models");
    if (expected !== discovery) return;
    installedModels = found;
    if (!found.includes(defaultModel)) defaultModel = found.find((name) => name === "qwen3:4b-instruct") ?? found[0];
    modelStatus = found.length ? "" : "Pull a model in Ollama, then reopen setup.";
  } catch (error) {
    if (expected !== discovery) return;
    defaultModel = undefined;
    modelStatus = error.message;
  }
  field("player_source").value = preferredSource ?? (defaultModel ? "ollama" : "demo");
  renderSource();
}

field("player_source").onchange = () => {
  preferredSource = field("player_source").value;
  renderSource();
};
field("default_model").onchange = () => { defaultModel = field("default_model").value; };
field("players").onchange = resizePrompts;
byId("setup-button").onclick = () => {
  byId("form-error").hidden = true;
  byId("setup").showModal();
  void discoverModels();
};
byId("close-setup").onclick = () => byId("setup").close();
form.onsubmit = async (event) => {
  event.preventDefault();
  const errorBox = byId("form-error");
  errorBox.hidden = true;
  launching = true;
  updateLaunchButton();
  try {
    const config = {
      seed: Number(field("seed").value),
      ap_interval: Number(field("ap_interval").value),
      ap_round_limit: Number(field("ap_round_limit").value),
      spawn_mode: field("spawn_mode").value,
      attacker_disclosure: field("attacker_disclosure").value,
      memory_limit_bytes: Number(field("memory_limit_bytes").value),
      players: Array.from({ length: Number(field("players").value) }, (_, index) => ({
        starting_prompt: promptDrafts[index] ?? "",
      })),
    };
    const player_models = field("player_source").value === "ollama" ? config.players.map((_, index) =>
      installedModels.includes(modelOverrides[index]) ? modelOverrides[index] : defaultModel
    ) : null;
    await request("/api/live", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ config, player_models }),
    });
    byId("setup").close();
    showView("live");
    await poll();
  } catch (error) {
    errorBox.textContent = error.message;
    errorBox.hidden = false;
  } finally {
    launching = false;
    updateLaunchButton();
  }
};

render();
try {
  defaults = await request("/api/config");
  for (const [key, value] of Object.entries(defaults)) {
    if (key !== "players" && field(key)) field(key).value = value;
  }
  field("players").value = defaults.players.length;
  render();
} catch (error) {
  notice(`Could not load match defaults. ${error.message}`);
}

async function refresh() {
  await poll();
  setTimeout(refresh, 320);
}
void refresh();
