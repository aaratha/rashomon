// Stage 0 of the Rashomon Firefox-extension Host pivot (see
// docs/FIREFOX_EXTENSION_SPIKE.md at the repo root). Proves the native-
// messaging transport by relaying the raw payloads of Sidebery's own
// tabs.* events to the rashomon-kernel native host, without touching
// tabs.bg.ts or anything else Sidebery does with those events —
// WebExtensions allows multiple independent listeners on the same
// event, so this just adds its own alongside Tabs.setupListeners().

import * as Logs from 'src/services/logs'

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

function send(type: string, args: unknown[]): void {
  if (!port) port = connect()
  try {
    port.postMessage({ type, args })
  } catch (err) {
    Logs.warn('NativeHost.send: failed, will reconnect on next event', err)
    port = undefined
  }
}

export function setupListeners(): void {
  browser.tabs.onCreated.addListener((...args) => send('tabs.onCreated', args))
  browser.tabs.onUpdated.addListener((...args) => send('tabs.onUpdated', args))
  browser.tabs.onRemoved.addListener((...args) => send('tabs.onRemoved', args))
  browser.tabs.onMoved.addListener((...args) => send('tabs.onMoved', args))
}
