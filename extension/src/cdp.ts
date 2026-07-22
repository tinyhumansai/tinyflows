import { BrowserError } from './errors';
import type { BrowserAction, BrowserRequest } from './protocol';

type DebuggerApi = Pick<typeof chrome.debugger, 'sendCommand'>;
type TabsApi = Pick<typeof chrome.tabs, 'remove'>;

export class CdpExecutor {
  constructor(
    private readonly debuggerApi: DebuggerApi = chrome.debugger,
    private readonly tabsApi: TabsApi = chrome.tabs
  ) {}

  async execute(request: BrowserRequest): Promise<unknown> {
    const operation = this.run(request.tab_id, request.action);
    return withTimeout(operation, request.timeout_ms);
  }

  private async run(tabId: number, action: BrowserAction): Promise<unknown> {
    const target = { tabId };
    switch (action.action) {
      case 'open': {
        const url = action.url;
        if (!/^https?:\/\//i.test(url)) throw new BrowserError('unsupported_page', 'Only HTTP(S) URLs can be opened');
        await this.debuggerApi.sendCommand(target, 'Page.navigate', { url });
        return { url };
      }
      case 'snapshot':
        return this.debuggerApi.sendCommand(target, 'Accessibility.getFullAXTree', {});
      case 'get_title': return evaluate(this.debuggerApi, target, 'document.title');
      case 'get_url': return evaluate(this.debuggerApi, target, 'location.href');
      case 'get_text': return evaluate(this.debuggerApi, target, textExpression(action.selector));
      case 'is_visible': return evaluate(this.debuggerApi, target, visibleExpression(action.selector));
      case 'find': return evaluate(this.debuggerApi, target, findExpression(action.query));
      case 'screenshot': {
        const result = await this.debuggerApi.sendCommand(target, 'Page.captureScreenshot', { format: 'png', fromSurface: true });
        return result;
      }
      case 'click': {
        const point = await elementPoint(this.debuggerApi, target, action.selector);
        await mouse(this.debuggerApi, target, 'mousePressed', point, 1);
        await mouse(this.debuggerApi, target, 'mouseReleased', point, 1);
        return { clicked: true };
      }
      case 'hover': {
        const point = await elementPoint(this.debuggerApi, target, action.selector);
        await mouse(this.debuggerApi, target, 'mouseMoved', point, 0);
        return { hovered: true };
      }
      case 'fill': {
        const { selector, value } = action;
        const ok = await evaluate(this.debuggerApi, target, fillExpression(selector, value));
        if (!ok) throw new BrowserError('element_not_found', `No element matches ${selector}`);
        return { filled: true };
      }
      case 'type': {
        if (action.selector) {
          const ok = await evaluate(this.debuggerApi, target, `document.querySelector(${JSON.stringify(action.selector)})?.focus() ?? false`);
          if (ok === null) throw new BrowserError('element_not_found', `No element matches ${action.selector}`);
        }
        await this.debuggerApi.sendCommand(target, 'Input.insertText', { text: action.text });
        return { typed: true };
      }
      case 'press': {
        const key = action.key;
        await this.debuggerApi.sendCommand(target, 'Input.dispatchKeyEvent', { type: 'keyDown', key });
        await this.debuggerApi.sendCommand(target, 'Input.dispatchKeyEvent', { type: 'keyUp', key });
        return { pressed: key };
      }
      case 'scroll': {
        const x = action.x ?? 0; const y = action.y ?? 0;
        await this.debuggerApi.sendCommand(target, 'Runtime.evaluate', {
          expression: `window.scrollBy(${JSON.stringify(x)},${JSON.stringify(y)})`, returnByValue: true
        });
        return { x, y };
      }
      case 'wait': {
        const selector = action.selector;
        const ms = action.duration_ms ?? (selector ? 15_000 : 1000);
        if (!selector) { await delay(ms); return { waitedMs: ms }; }
        const deadline = Date.now() + ms;
        while (Date.now() < deadline) {
          if (await evaluate(this.debuggerApi, target, visibleExpression(selector))) return { visible: true };
          await delay(Math.min(100, Math.max(1, deadline - Date.now())));
        }
        throw new BrowserError('element_not_found', `Element did not appear: ${selector}`);
      }
      case 'close':
        await this.tabsApi.remove(tabId);
        return { closed: true };
    }
  }
}

async function evaluate(api: DebuggerApi, target: chrome.debugger.Debuggee, expression: string): Promise<unknown> {
  const response = await api.sendCommand(target, 'Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  const result = response as { result?: { value?: unknown }; exceptionDetails?: unknown };
  if (result.exceptionDetails) throw new BrowserError('browser_failure', 'Page evaluation failed');
  return result.result?.value;
}

async function elementPoint(api: DebuggerApi, target: chrome.debugger.Debuggee, selector: string) {
  const expression = `(() => { const e=document.querySelector(${JSON.stringify(selector)}); if(!e)return null; const r=e.getBoundingClientRect(); return {x:r.left+r.width/2,y:r.top+r.height/2}; })()`;
  const point = await evaluate(api, target, expression) as { x?: unknown; y?: unknown } | null;
  if (!point || typeof point.x !== 'number' || typeof point.y !== 'number') {
    throw new BrowserError('element_not_found', `No element matches ${selector}`);
  }
  return { x: point.x, y: point.y };
}

async function mouse(api: DebuggerApi, target: chrome.debugger.Debuggee, type: string, point: {x: number; y: number}, clickCount: number) {
  await api.sendCommand(target, 'Input.dispatchMouseEvent', { type, x: point.x, y: point.y, button: 'left', clickCount });
}

function textExpression(selector?: string): string {
  return selector
    ? `document.querySelector(${JSON.stringify(selector)})?.textContent ?? null`
    : 'document.body?.innerText ?? ""';
}
function visibleExpression(selector: string): string {
  return `(() => { const e=document.querySelector(${JSON.stringify(selector)}); if(!e)return false; const r=e.getBoundingClientRect(); const s=getComputedStyle(e); return r.width>0&&r.height>0&&s.visibility!=="hidden"&&s.display!=="none"; })()`;
}
function fillExpression(selector: string, value: string): string {
  return `(() => { const e=document.querySelector(${JSON.stringify(selector)}); if(!(e instanceof HTMLInputElement||e instanceof HTMLTextAreaElement))return false; e.focus(); const set=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(e),"value")?.set; set?.call(e,${JSON.stringify(value)}); e.dispatchEvent(new Event("input",{bubbles:true})); e.dispatchEvent(new Event("change",{bubbles:true})); return true; })()`;
}
function findExpression(text: string): string {
  return `(() => { const q=${JSON.stringify(text)}.toLowerCase(); return [...document.querySelectorAll("a,button,input,textarea,select,[role],[aria-label]")].filter(e => ((e.textContent||"")+" "+(e.getAttribute("aria-label")||"")).toLowerCase().includes(q)).slice(0,50).map(e => ({tag:e.tagName.toLowerCase(),text:(e.textContent||"").trim().slice(0,500),role:e.getAttribute("role"),ariaLabel:e.getAttribute("aria-label")})); })()`;
}
function delay(ms: number) { return new Promise<void>((resolve) => setTimeout(resolve, ms)); }
async function withTimeout<T>(operation: Promise<T>, ms: number): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new BrowserError('action_timeout', `Browser action timed out after ${ms}ms`, true)), ms);
  });
  try { return await Promise.race([operation, timeout]); }
  finally { if (timer) clearTimeout(timer); }
}
