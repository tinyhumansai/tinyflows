// src/sidepanel.ts
var $ = (id) => document.getElementById(id);
var workflows = $("workflows");
var run = $("run");
var events = $("events");
var cancel = $("cancel");
var activeRunId;
async function refresh() {
  const state = await chrome.runtime.sendMessage({ type: "state" });
  $("connection").textContent = `Companion: ${state.relayState} \xB7 ${state.tabs.length} shared tab(s)`;
  const response = await chrome.runtime.sendMessage({ type: "workflow.list" });
  if (!response.ok) {
    workflows.innerHTML = `<p class="error"></p>`;
    workflows.querySelector("p").textContent = response.error;
    return;
  }
  const list = Array.isArray(response.result) ? response.result : [];
  workflows.replaceChildren(...list.map(workflowCard));
  if (list.length === 0) workflows.innerHTML = '<p class="muted">No workflows exposed by the companion.</p>';
}
function workflowCard(value) {
  const item = value && typeof value === "object" ? value : {};
  const id = String(item.id ?? item.workflow_id ?? "");
  const card = document.createElement("article");
  card.className = "card";
  const title = document.createElement("strong");
  title.textContent = String(item.name ?? id);
  const description = document.createElement("p");
  description.className = "muted";
  description.textContent = String(item.description ?? "");
  const button = document.createElement("button");
  button.textContent = "Run in this tab";
  button.addEventListener("click", () => void start(id));
  card.append(title, description, button);
  return card;
}
async function start(workflowId) {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab?.id === void 0) return showError("No active tab");
  const response = await chrome.runtime.sendMessage({ type: "workflow.start", workflowId, tabId: tab.id });
  if (!response.ok) return showError(response.error);
  const value = response.result;
  activeRunId = String(value?.run_id ?? value?.id ?? "");
  run.textContent = `Run ${activeRunId || "started"}`;
  cancel.hidden = !activeRunId;
  if (activeRunId) addEvent({ event: "started", protocol_version: 1, run_id: activeRunId, tab_id: tab.id });
}
function showError(message) {
  run.innerHTML = '<p class="error"></p>';
  run.querySelector("p").textContent = message;
}
function addEvent(value) {
  const event = value;
  const runId = typeof event.run_id === "string" ? event.run_id : void 0;
  if (!activeRunId || runId !== activeRunId) return;
  const li = document.createElement("li");
  const details = { ...event };
  delete details.event;
  delete details.protocol_version;
  delete details.run_id;
  li.textContent = `${String(event.event ?? "event")} \xB7 ${JSON.stringify(details)}`;
  events.prepend(li);
  while (events.children.length > 200) events.lastElementChild?.remove();
}
chrome.runtime.onMessage.addListener((message) => {
  if (message.type === "relay.state") $("connection").textContent = `Companion: ${message.state}`;
  if (message.type === "run.event") addEvent(message.event);
});
$("refresh").addEventListener("click", () => void refresh());
cancel.addEventListener("click", async () => {
  if (!activeRunId) return;
  const response = await chrome.runtime.sendMessage({ type: "workflow.cancel", runId: activeRunId });
  if (!response.ok) return showError(response.error);
  addEvent({ event: "cancel_requested", data: { run_id: activeRunId } });
  activeRunId = void 0;
  cancel.hidden = true;
});
void refresh().catch((error) => showError(error instanceof Error ? error.message : String(error)));
