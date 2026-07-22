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

export type BrowserResponse =
  | { status: 'ok'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; result: { data: unknown } }
  | { status: 'error'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; error: BrowserErrorData };

export type BrowserEvent =
  | { event: 'action_started'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; run_id: string; tab_id: number }
  | { event: 'action_completed'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; result: { data: unknown } }
  | { event: 'action_failed'; protocol_version: typeof PROTOCOL_VERSION; request_id: string; error: BrowserErrorData }
  | { event: 'tab_revoked'; protocol_version: typeof PROTOCOL_VERSION; tab_id: number }
  | { event: 'relay_disconnected'; protocol_version: typeof PROTOCOL_VERSION };

export interface ControlRequest {
  protocol_version: typeof PROTOCOL_VERSION;
  type: 'control.request';
  request_id: string;
  method: 'workflow.list' | 'workflow.start' | 'workflow.cancel' | 'run.subscribe' | 'tab.list';
  params: Record<string, unknown>;
}
export interface ControlResponse {
  protocol_version: typeof PROTOCOL_VERSION;
  type: 'control.response';
  request_id: string;
  ok: boolean;
  result?: unknown;
  error?: { code: string; message: string };
}
export interface RunEvent {
  protocol_version: typeof PROTOCOL_VERSION;
  type: 'run.event';
  run_id: string;
  event: string;
  data: unknown;
}

export function isBrowserRequest(value: unknown): value is BrowserRequest {
  if (!isRecordWithKeys(value, ['protocol_version', 'request_id', 'run_id', 'tab_id', 'timeout_ms', 'action'])) return false;
  return value.protocol_version === PROTOCOL_VERSION && isId(value.request_id) && isId(value.run_id) &&
    Number.isSafeInteger(value.tab_id) && (value.tab_id as number) >= 0 &&
    Number.isSafeInteger(value.timeout_ms) && (value.timeout_ms as number) >= 100 &&
    (value.timeout_ms as number) <= 120_000 && isBrowserAction(value.action);
}

export function isBrowserAction(value: unknown): value is BrowserAction {
  if (!isRecord(value) || typeof value.action !== 'string') return false;
  switch (value.action) {
    case 'open': return exactStrings(value, ['action', 'url'], ['url']);
    case 'snapshot': case 'get_title': case 'get_url': case 'close': return hasExactKeys(value, ['action']);
    case 'click': case 'hover': case 'is_visible': return exactStrings(value, ['action', 'selector'], ['selector']);
    case 'fill': return exactStrings(value, ['action', 'selector', 'value'], ['selector', 'value']);
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
  if (!isRecord(value) || !hasOnlyKeys(value, ['protocol_version', 'type', 'request_id', 'ok', 'result', 'error'])) return false;
  return value.protocol_version === PROTOCOL_VERSION && value.type === 'control.response' && isId(value.request_id) && typeof value.ok === 'boolean';
}
export function isRunEvent(value: unknown): value is RunEvent {
  return isRecordWithKeys(value, ['protocol_version', 'type', 'run_id', 'event', 'data']) &&
    value.protocol_version === PROTOCOL_VERSION && value.type === 'run.event' && isId(value.run_id) && isId(value.event);
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
