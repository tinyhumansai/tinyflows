import { BrowserError } from './errors';

const STORAGE_KEY = 'tinyflows.sharedTabs.v1';
export const GROUP_TITLE = 'TinyFlows shared tabs';

export type BadgeState = 'connected' | 'reconnecting' | 'failed' | 'idle';
export interface SharedTab { tabId: number; groupId: number; windowId: number; attachedAt: number }
export interface SharedTabAnnouncement { id: number; window_id: number; url: string; title: string }

type ChromeApi = Pick<typeof chrome, 'tabs' | 'tabGroups' | 'debugger' | 'storage' | 'action'>;

export class TabManager {
  private shared = new Map<number, SharedTab>();

  constructor(private readonly api: ChromeApi = chrome) {}

  async rehydrate(): Promise<SharedTab[]> {
    const saved = (await this.api.storage.local.get(STORAGE_KEY))[STORAGE_KEY];
    if (Array.isArray(saved)) {
      for (const item of saved) {
        if (isSharedTab(item)) this.shared.set(item.tabId, item);
      }
    }
    for (const tabId of [...this.shared.keys()]) {
      try {
        await this.assertShared(tabId);
        const targets = await this.api.debugger.getTargets();
        if (!targets.some((target) => target.tabId === tabId && target.attached)) {
          await this.api.debugger.attach({ tabId }, '1.3');
        }
        await this.setBadge(tabId, 'connected');
      } catch {
        await this.revoke(tabId, false);
      }
    }
    return this.list();
  }

  async share(tabId: number): Promise<SharedTab> {
    const tab = await this.api.tabs.get(tabId);
    ensureSupported(tab.url);
    if (tab.windowId === undefined) throw new BrowserError('invalid_request', 'Tab has no window');

    let groupId: number;
    const groups = await this.api.tabGroups.query({ windowId: tab.windowId, title: GROUP_TITLE });
    const existing = groups[0];
    if (existing) {
      groupId = existing.id;
      await this.api.tabs.group({ groupId, tabIds: [tabId] });
    } else {
      groupId = await this.api.tabs.group({ tabIds: [tabId] });
      await this.api.tabGroups.update(groupId, { title: GROUP_TITLE, color: 'blue', collapsed: false });
    }
    try {
      await this.api.debugger.attach({ tabId }, '1.3');
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (!message.includes('already attached')) throw error;
    }
    const shared = { tabId, groupId, windowId: tab.windowId, attachedAt: Date.now() };
    this.shared.set(tabId, shared);
    await this.persist();
    await this.setBadge(tabId, 'connected');
    return shared;
  }

  async revoke(tabId: number, ungroup = true): Promise<void> {
    const existed = this.shared.delete(tabId);
    if (existed) await this.persist();
    try { await this.api.debugger.detach({ tabId }); } catch { /* already detached */ }
    if (ungroup) {
      try { await this.api.tabs.ungroup([tabId]); } catch { /* tab already closed */ }
    }
    await this.setBadge(tabId, 'idle');
  }

  async toggle(tabId: number): Promise<boolean> {
    if (this.shared.has(tabId)) {
      await this.revoke(tabId);
      return false;
    }
    await this.share(tabId);
    return true;
  }

  async assertShared(tabId: number): Promise<SharedTab> {
    const record = this.shared.get(tabId);
    if (!record) throw new BrowserError('tab_not_shared', 'The tab was not explicitly shared');
    let tab: chrome.tabs.Tab;
    try { tab = await this.api.tabs.get(tabId); }
    catch { throw new BrowserError('tab_revoked', 'The shared tab no longer exists'); }
    ensureSupported(tab.url);
    if (tab.groupId !== record.groupId) {
      await this.revoke(tabId, false);
      throw new BrowserError('tab_revoked', 'The tab was removed from the TinyFlows group');
    }
    const group = await this.api.tabGroups.get(record.groupId).catch(() => undefined);
    if (!group || group.title !== GROUP_TITLE) {
      await this.revoke(tabId, false);
      throw new BrowserError('tab_revoked', 'The TinyFlows group was removed or renamed');
    }
    return record;
  }

  list(): SharedTab[] { return [...this.shared.values()].sort((a, b) => a.tabId - b.tabId); }
  has(tabId: number): boolean { return this.shared.has(tabId); }

  async announcement(tabId: number): Promise<SharedTabAnnouncement> {
    const shared = await this.assertShared(tabId);
    const tab = await this.api.tabs.get(tabId);
    return {
      id: tabId,
      window_id: shared.windowId,
      url: tab.url!,
      title: tab.title ?? ''
    };
  }

  async markAll(state: BadgeState): Promise<void> {
    await Promise.all(this.list().map(({ tabId }) => this.setBadge(tabId, state)));
  }

  async setBadge(tabId: number, state: BadgeState): Promise<void> {
    const visual = {
      connected: { text: 'ON', color: '#16794f' },
      reconnecting: { text: '…', color: '#ad6b00' },
      failed: { text: '!', color: '#b42318' },
      idle: { text: '', color: '#59636e' }
    }[state];
    try {
      await this.api.action.setBadgeBackgroundColor({ tabId, color: visual.color });
      await this.api.action.setBadgeText({ tabId, text: visual.text });
    } catch { /* tab may have closed */ }
  }

  private async persist(): Promise<void> {
    await this.api.storage.local.set({ [STORAGE_KEY]: this.list() });
  }
}

function ensureSupported(url?: string): void {
  if (!url || /^(chrome|chrome-extension|edge|devtools|about|view-source):/i.test(url)) {
    throw new BrowserError('unsupported_page', 'Chrome does not permit automation on this page');
  }
}

function isSharedTab(value: unknown): value is SharedTab {
  if (typeof value !== 'object' || value === null) return false;
  const item = value as Record<string, unknown>;
  return Number.isInteger(item.tabId) && Number.isInteger(item.groupId) &&
    Number.isInteger(item.windowId) && typeof item.attachedAt === 'number';
}
