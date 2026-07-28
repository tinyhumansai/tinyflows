export const PROTOCOL_VERSION = 1 as const;

export type BrowserAction =
  | { action: 'open'; url: string }
  | { action: 'snapshot' }
  | { action: 'click'; selector: string }
  | { action: 'fill'; selector: string; value: string }
  | { action: 'type'; selector?: string; text: string }
  | { action: 'get_text'; selector?: string }
  | { action: 'get_title' }
  | { action: 'get_url' }
  | { action: 'screenshot'; selector?: string; full_page?: boolean }
  | { action: 'wait'; duration_ms?: number; selector?: string }
  | { action: 'press'; key: string }
  | { action: 'hover'; selector: string }
  | { action: 'scroll'; x?: number; y?: number }
  | { action: 'is_visible'; selector: string }
  | { action: 'close' }
  | { action: 'find'; query: string };

export const BROWSER_ERROR_CODES = [
  'tab_not_shared', 'tab_revoked', 'relay_disconnected', 'unsupported_page',
  'action_timeout', 'element_not_found', 'invalid_request', 'protocol_mismatch',
  'cancelled', 'browser_failure'
] as const;
export type BrowserErrorCode = typeof BROWSER_ERROR_CODES[number];
export interface BrowserErrorData { code: BrowserErrorCode; message: string; details?: unknown }

export interface BrowserRequest {
  protocol_version: typeof PROTOCOL_VERSION;
  request_id: string;
  run_id: string;
  tab_id: number;
  timeout_ms: number;
  action: BrowserAction;
}

export interface BrowserCancel {
  protocol_version: typeof PROTOCOL_VERSION;
  type: 'browser.cancel';
  request_id: string;
}

export type BrowserResponse =
  | { status: 'ok'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; result: { data: unknown } }
  | { status: 'error'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; error: BrowserErrorData };

export type BrowserEvent =
  | { event: 'action_started'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; run_id: string; tab_id: number }
  | { event: 'action_completed'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; result: { data: unknown } }
  | { event: 'action_failed'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; error: BrowserErrorData }
  | { event: 'tab_revoked'; protocol_version: typeof PROTOCOL_VERSION; tab_id: number }
  | { event: 'relay_disconnected'; protocol_version: typeof PROTOCOL_VERSION };

export interface TabSharedEvent {
  event: 'tab_shared';
  protocol_version: typeof PROTOCOL_VERSION;
  tab: { id: number; window_id: number; url: string; title: string };
}

export function tabSharedEvent(tab: TabSharedEvent['tab']): TabSharedEvent {
  return { event: 'tab_shared', protocol_version: PROTOCOL_VERSION, tab };
}

export type ControlRequest =
  | { method: 'workflow.list'; protocol_version: typeof PROTOCOL_VERSION; request_id: string }
  | { method: 'workflow.start'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; workflow_id: string; tab_id: number; input: unknown }
  | { method: 'workflow.cancel'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; run_id: string }
  | { method: 'run.subscribe'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; run_id: string }
  | { method: 'tab.list'; protocol_version: typeof PROTOCOL_VERSION; request_id: string }
  | { method: 'connection.status'; protocol_version: typeof PROTOCOL_VERSION; request_id: string };
export type ControlResponse =
  | { status: 'ok'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; result: unknown }
  | { status: 'error'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; code: string; message: string }
  | { status: 'workflows'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; workflows: Array<{id:string; name:string}> }
  | { status: 'tabs'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; tabs: unknown[] }
  | { status: 'connection'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; connected: boolean };
export type RunEvent =
  | { event: 'started'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; tab_id: number }
  | { event: 'step_started'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; node_id: string; node_kind: string }
  | { event: 'step_completed'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; node_id: string; node_kind: string; status: 'success' | 'error'; duration_ms: number }
  | { event: 'awaiting_approval'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; pending_approvals: string[] }
  | { event: 'browser_action_started'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; request_id: string; tab_id: number; action: string }
  | { event: 'browser_action_completed'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; request_id: string; output: unknown }
  | { event: 'browser_action_failed'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; request_id: string; code: string; message: string }
  | { event: 'completed'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; status: 'success' }
  | { event: 'failed'; protocol_version: typeof PROTOCOL_VERSION; run_id: string; code: string; message: string }
  | { event: 'cancelled'; protocol_version: typeof PROTOCOL_VERSION; run_id: string };

export function isBrowserRequest(value: unknown): value is BrowserRequest {
  if (!isRecordWithKeys(value, ['protocol_version', 'request_id', 'run_id', 'tab_id', 'timeout_ms', 'action'])) return false;
  return value.protocol_version === PROTOCOL_VERSION && isId(value.request_id) && isId(value.run_id) &&
    Number.isSafeInteger(value.tab_id) && (value.tab_id as number) >= 0 &&
    Number.isSafeInteger(value.timeout_ms) && (value.timeout_ms as number) >= 1 &&
    (value.timeout_ms as number) <= 60_000 && isBrowserAction(value.action);
}

export function isBrowserCancel(value: unknown): value is BrowserCancel {
  return isRecordWithKeys(value, ['protocol_version', 'type', 'request_id']) &&
    value.protocol_version === PROTOCOL_VERSION && value.type === 'browser.cancel' && isId(value.request_id);
}

export function isBrowserAction(value: unknown): value is BrowserAction {
  if (!isRecord(value) || typeof value.action !== 'string') return false;
  switch (value.action) {
    case 'open': return exactStrings(value, ['action', 'url'], ['url']);
    case 'snapshot': case 'get_title': case 'get_url': case 'close': return hasExactKeys(value, ['action']);
    case 'click': case 'hover': case 'is_visible': return exactStrings(value, ['action', 'selector'], ['selector']);
    case 'fill': return hasExactKeys(value, ['action', 'selector', 'value']) &&
      typeof value.selector === 'string' && value.selector.length > 0 && typeof value.value === 'string';
    case 'type': return exactStrings(value, ['action', 'text'], ['text'], ['selector']);
    case 'get_text': return exactStrings(value, ['action'], [], ['selector']);
    case 'screenshot': return hasOnlyKeys(value, ['action', 'selector', 'full_page']) &&
      optionalString(value.selector) && (value.full_page === undefined || typeof value.full_page === 'boolean');
    case 'wait': return hasOnlyKeys(value, ['action', 'duration_ms', 'selector']) && optionalString(value.selector) &&
      (value.duration_ms === undefined || (Number.isSafeInteger(value.duration_ms) && (value.duration_ms as number) >= 0));
    case 'press': return exactStrings(value, ['action', 'key'], ['key']);
    case 'scroll': return hasOnlyKeys(value, ['action', 'x', 'y']) && optionalInteger(value.x) && optionalInteger(value.y);
    case 'find': return exactStrings(value, ['action', 'query'], ['query']);
    default: return false;
  }
}

export function isControlResponse(value: unknown): value is ControlResponse {
  if (!isRecord(value) || value.protocol_version !== PROTOCOL_VERSION || !isId(value.request_id)) return false;
  switch (value.status) {
    case 'ok': return hasExactKeys(value, ['status', 'protocol_version', 'request_id', 'result']);
    case 'error': return hasExactKeys(value, ['status', 'protocol_version', 'request_id', 'code', 'message']) && isId(value.code) && typeof value.message === 'string';
    case 'workflows': return hasExactKeys(value, ['status', 'protocol_version', 'request_id', 'workflows']) && Array.isArray(value.workflows);
    case 'tabs': return hasExactKeys(value, ['status', 'protocol_version', 'request_id', 'tabs']) && Array.isArray(value.tabs);
    case 'connection': return hasExactKeys(value, ['status', 'protocol_version', 'request_id', 'connected']) && typeof value.connected === 'boolean';
    default: return false;
  }
}
export function isRunEvent(value: unknown): value is RunEvent {
  if (!isRecord(value) || value.protocol_version !== PROTOCOL_VERSION || !isId(value.run_id)) return false;
  switch (value.event) {
    case 'started': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'tab_id']) && Number.isSafeInteger(value.tab_id);
    case 'step_started': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'node_id', 'node_kind']) && isId(value.node_id) && isId(value.node_kind);
    case 'step_completed': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'node_id', 'node_kind', 'status', 'duration_ms']) &&
      isId(value.node_id) && isId(value.node_kind) && (value.status === 'success' || value.status === 'error') &&
      Number.isSafeInteger(value.duration_ms) && (value.duration_ms as number) >= 0;
    case 'awaiting_approval': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'pending_approvals']) &&
      Array.isArray(value.pending_approvals) && value.pending_approvals.every(isId);
    case 'browser_action_started': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'request_id', 'tab_id', 'action']) &&
      isId(value.request_id) && Number.isSafeInteger(value.tab_id) && isId(value.action);
    case 'browser_action_completed': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'request_id', 'output']) && isId(value.request_id);
    case 'browser_action_failed': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'request_id', 'code', 'message']) &&
      isId(value.request_id) && isId(value.code) && typeof value.message === 'string';
    case 'completed': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'status']) && value.status === 'success';
    case 'failed': return hasExactKeys(value, ['event', 'protocol_version', 'run_id', 'code', 'message']) && isId(value.code) && typeof value.message === 'string';
    case 'cancelled': return hasExactKeys(value, ['event', 'protocol_version', 'run_id']);
    default: return false;
  }
}
export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
function isRecordWithKeys(value: unknown, keys: string[]): value is Record<string, unknown> {
  return isRecord(value) && hasExactKeys(value, keys);
}
function hasExactKeys(value: Record<string, unknown>, keys: string[]): boolean {
  return Object.keys(value).length === keys.length && hasOnlyKeys(value, keys) && keys.every((key) => key in value);
}
function hasOnlyKeys(value: Record<string, unknown>, keys: string[]): boolean {
  return Object.keys(value).every((key) => keys.includes(key));
}
function exactStrings(value: Record<string, unknown>, requiredKeys: string[], requiredStrings: string[], optionalStrings: string[] = []): boolean {
  if (!requiredKeys.every((key) => key in value) || !hasOnlyKeys(value, [...requiredKeys, ...optionalStrings])) return false;
  return requiredStrings.every((key) => typeof value[key] === 'string' && (value[key] as string).length > 0) &&
    optionalStrings.every((key) => optionalString(value[key]));
}
function optionalString(value: unknown): boolean { return value === undefined || typeof value === 'string'; }
function optionalInteger(value: unknown): boolean { return value === undefined || Number.isSafeInteger(value); }
function isId(value: unknown): value is string { return typeof value === 'string' && value.length > 0 && value.length <= 256; }
