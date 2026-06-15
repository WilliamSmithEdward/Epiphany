// Bridge to a native host that embeds this web app (ADR-0022: the Excel add-in's
// WebView2 configurator). In a normal browser these are no-ops.

interface WebView2Host {
  postMessage: (message: unknown) => void
}

interface ChromeWithWebView {
  webview?: WebView2Host
}

/**
 * When signed in inside the Excel add-in's WebView2, hand the session token to
 * the host so its worksheet functions can call the API as this user. The host
 * validates the message origin before accepting the token. A normal browser has
 * no `chrome.webview`, so this does nothing.
 */
export function notifyExcelHost(token: string): void {
  const chrome = (window as unknown as { chrome?: ChromeWithWebView }).chrome
  if (chrome?.webview && typeof chrome.webview.postMessage === 'function') {
    chrome.webview.postMessage({ type: 'epiphany-auth', token })
  }
}
