const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const workflows = $('workflows');
const run = $('run');
const events = $<HTMLOListElement>('events');
const cancel = $<HTMLButtonElement>('cancel');
let activeRunId: string | undefined;

async function refresh(): Promise<void> {
  const state = await chrome.runtime.sendMessage({ type: 'state' });
  $('connection').textContent = `Companion: ${state.relayState} · ${state.tabs.length} shared tab(s)`;
  const response = await chrome.runtime.sendMessage({ type: 'workflow.list' });
  if (!response.ok) { workflows.innerHTML = `<p class="error"></p>`; workflows.querySelector('p')!.textContent = response.error; return; }
  const list = Array.isArray(response.result) ? response.result : [];
  workflows.replaceChildren(...list.map(workflowCard));
  if (list.length === 0) workflows.innerHTML = '<p class="muted">No workflows exposed by the companion.</p>';
}

function workflowCard(value: unknown): HTMLElement {
  const item = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  const id = String(item.id ?? item.workflow_id ?? '');
  const card = document.createElement('article'); card.className = 'card';
  const title = document.createElement('strong'); title.textContent = String(item.name ?? id);
  const description = document.createElement('p'); description.className = 'muted'; description.textContent = String(item.description ?? '');
  const button = document.createElement('button'); button.textContent = 'Run in this tab';
  button.addEventListener('click', () => void start(id));
  card.append(title, description, button); return card;
}

async function start(workflowId: string): Promise<void> {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (tab?.id === undefined) return showError('No active tab');
  const response = await chrome.runtime.sendMessage({ type: 'workflow.start', workflowId, tabId: tab.id });
  if (!response.ok) return showError(response.error);
  const value = response.result as Record<string, unknown> | undefined;
  activeRunId = String(value?.run_id ?? value?.id ?? '');
  run.textContent = `Run ${activeRunId || 'started'}`; cancel.hidden = !activeRunId;
}
function showError(message: string): void { run.innerHTML = '<p class="error"></p>'; run.querySelector('p')!.textContent = message; }
function addEvent(value: unknown): void {
  const li = document.createElement('li');
  const event = value as { event?: string; data?: unknown; run_id?: string };
  li.textContent = `${event.event ?? 'event'} · ${JSON.stringify(event.data ?? {})}`;
  events.prepend(li); while (events.children.length > 200) events.lastElementChild?.remove();
  if (event.run_id) activeRunId = event.run_id;
}
chrome.runtime.onMessage.addListener((message) => {
  if (message.type === 'relay.state') $('connection').textContent = `Companion: ${message.state}`;
  if (message.type === 'run.event') addEvent(message.event);
});
$('refresh').addEventListener('click', () => void refresh());
cancel.addEventListener('click', async () => {
  if (!activeRunId) return;
  const response = await chrome.runtime.sendMessage({ type: 'workflow.cancel', runId: activeRunId });
  if (!response.ok) return showError(response.error);
  addEvent({ event: 'cancel_requested', data: { run_id: activeRunId } }); activeRunId = undefined; cancel.hidden = true;
});
void refresh().catch((error) => showError(error instanceof Error ? error.message : String(error)));
