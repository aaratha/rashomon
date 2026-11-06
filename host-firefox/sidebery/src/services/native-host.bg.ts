// Rashomon Firefox-extension Host pivot (see
// docs/FIREFOX_EXTENSION_SPIKE.md at the repo root). Stage 0 proved the
// native-messaging transport; this is Stage 1's extension-side half —
// relaying enough of each tab's state, plus Sidebery's own tree
// position for it, for the native host to mirror tabs into the graph
// and map Sidebery's tree to `contains` edges. Still taps tabs.* as
// independent listeners (tabs.bg.ts untouched) — WebExtensions allows
// multiple independent listeners on the same event.

import * as Logs from 'src/services/logs'
import * as Tabs from 'src/services/tabs.bg'
import { NOID } from 'src/defaults'

const APP_ID = 'dev.rashomon.kernel'

let port: browser.runtime.Port | undefined

function connect(): browser.runtime.Port {
  const p = browser.runtime.connectNative(APP_ID)
  p.onDisconnect.addListener(() => {
    Logs.warn('NativeHost.connect: disconnected', browser.runtime.lastError)
    port = undefined
  })
  p.onMessage.addListener((msg: unknown) => {
    Logs.info('NativeHost: ack', msg)
  })
  return p
}

interface RelayedTabEvent {
  type: string
  tabId: ID
  windowId: ID
  url?: string
  title?: string
  /**
   * Sidebery's own tree-parent for this tab. `Tabs.byId` here is the
   * *background* script's tab cache — its `parentId` is normally only
   * ever populated by `Snapshots`' call to `updateBgTabsTreeData`, not
   * kept live; the actual live tree lives in the sidebar page's own,
   * separate `tabs.fg.ts` module (a different JS context this
   * background-script file can't import). Forcing the same
   * `updateBgTabsTreeData` RPC (sidebar -> background, per window)
   * ourselves before reading it — only on `onCreated`/`onMoved`, the
   * two events that actually change tree shape, not the much more
   * frequent `onUpdated` — gets a real answer without duplicating this
   * relay into the sidebar context too. Still `NOID` if no sidebar is
   * open for that window; Sidebery has no tree opinion at all then.
   */
  parentId: ID
}

function send(event: RelayedTabEvent): void {
  if (!port) port = connect()
  try {
    port.postMessage(event)
  } catch (err) {
    Logs.warn('NativeHost.send: failed, will reconnect on next event', err)
    port = undefined
  }
}

function parentIdOf(tabId: ID): ID {
  return Tabs.byId[tabId]?.parentId ?? NOID
}

export function setupListeners(): void {
  browser.tabs.onCreated.addListener(tab => {
    void Tabs.updateBgTabsTreeData().finally(() => {
      send({
        type: 'tabs.onCreated',
        tabId: tab.id,
        windowId: tab.windowId,
        url: tab.url,
        title: tab.title,
        parentId: parentIdOf(tab.id),
      })
    })
  })

  browser.tabs.onUpdated.addListener((tabId, _change, tab) => {
    send({
      type: 'tabs.onUpdated',
      tabId,
      windowId: tab.windowId,
      url: tab.url,
      title: tab.title,
      parentId: parentIdOf(tabId),
    })
  })

  browser.tabs.onRemoved.addListener((tabId, removeInfo) => {
    send({
      type: 'tabs.onRemoved',
      tabId,
      windowId: removeInfo.windowId,
      parentId: parentIdOf(tabId),
    })
  })

  browser.tabs.onMoved.addListener((tabId, info) => {
    void Tabs.updateBgTabsTreeData().finally(() => {
      send({
        type: 'tabs.onMoved',
        tabId,
        windowId: info.windowId,
        parentId: parentIdOf(tabId),
      })
    })
  })
}
