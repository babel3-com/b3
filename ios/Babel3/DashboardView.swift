import SwiftUI
import WebKit

struct DashboardView: UIViewRepresentable {
    @Binding var sessionCookie: String?
    let dashboardURL: String
    
    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.allowsInlineMediaPlayback = true
        config.mediaTypesRequiringUserActionForPlayback = []
        config.websiteDataStore = .default()
        
        let userContentController = WKUserContentController()
        userContentController.add(context.coordinator, name: "nativeBridge")
        
        let bridgeScript = WKUserScript(source: NativeBridge.bridgeJS, injectionTime: .atDocumentStart, forMainFrameOnly: true)
        userContentController.addUserScript(bridgeScript)
        
        config.userContentController = userContentController
        
        let prefs = WKWebpagePreferences()
        prefs.allowsContentJavaScript = true
        config.defaultWebpagePreferences = prefs
        
        let webView = WKWebView(frame: .zero, configuration: config)
        webView.isOpaque = true
        webView.backgroundColor = .black
        webView.navigationDelegate = context.coordinator
        
        NativeBridge.shared.webView = webView
        NativeBridge.shared.configureAudioSession()
        
        // Clear JS/CSS cache first, THEN set cookie and load page.
        // This prevents the cache clear from racing with setCookie.
        // CRITICAL: use webView's own data store, NOT .default() — they can differ
        // if the WKWebView was configured with a non-default store. Using .default()
        // clears the wrong cache and stale JS persists indefinitely.
        let cookie = sessionCookie
        let url = dashboardURL
        webView.configuration.websiteDataStore.removeData(
            ofTypes: [WKWebsiteDataTypeMemoryCache, WKWebsiteDataTypeDiskCache],
            modifiedSince: Date.distantPast
        ) {
            print("[WebView] Cache cleared")
            
            if let cookieValue = cookie,
               let pageURL = URL(string: url),
               let host = pageURL.host {
                let props: [HTTPCookiePropertyKey: Any] = [
                    .name: "hc_session",
                    .value: cookieValue,
                    .domain: host,
                    .path: "/",
                ]
                if let httpCookie = HTTPCookie(properties: props) {
                    webView.configuration.websiteDataStore.httpCookieStore.setCookie(httpCookie) {
                        print("[WebView] Cookie set, loading dashboard")
                        webView.load(URLRequest(url: pageURL))
                    }
                    return
                }
            }
            
            // No cookie — load login page
            if let pageURL = URL(string: url) {
                webView.load(URLRequest(url: pageURL))
            }
        }
        
        context.coordinator.webView = webView
        return webView
    }
    
    func updateUIView(_ uiView: WKWebView, context: Context) {}
    
    func makeCoordinator() -> Coordinator { Coordinator() }
    
    class Coordinator: NSObject, WKScriptMessageHandler, WKNavigationDelegate {
        weak var webView: WKWebView?
        
        func userContentController(_ userContentController: WKUserContentController,
                                   didReceive message: WKScriptMessage) {
            guard let body = message.body as? [String: Any],
                  let action = body["action"] as? String else { return }
            NativeBridge.shared.handleMessage(action, body: body)
        }
        
        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            print("[WebView] Loaded: \(webView.url?.absoluteString.prefix(80) ?? "?")")
            webView.evaluateJavaScript("typeof window.NativeBridge") { result, _ in
                if let type = result as? String, type == "undefined" {
                    print("[WebView] NativeBridge missing — injecting fallback")
                    webView.evaluateJavaScript(NativeBridge.bridgeJS) { _, error in
                        if let error = error {
                            print("[WebView] Bridge injection error: \(error)")
                        } else {
                            print("[WebView] NativeBridge injected OK")
                        }
                    }
                } else {
                    print("[WebView] NativeBridge already present")
                }
            }
        }
    }
}
