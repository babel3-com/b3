import SwiftUI
import WebKit
import GoogleSignIn

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
        let serverURL = "https://babel3.com"

        func userContentController(_ userContentController: WKUserContentController,
                                   didReceive message: WKScriptMessage) {
            guard let body = message.body as? [String: Any],
                  let action = body["action"] as? String else { return }
            NativeBridge.shared.handleMessage(action, body: body)
        }

        // Intercept any navigation to the web OAuth flow or accounts.google.com.
        // WKWebView is rejected by Google with 403 disallowed_useragent. Instead
        // of using a browser, kick off the native GIDSignIn flow — it handles auth
        // in a proper system sheet and exchanges the ID token with the server,
        // setting hc_session directly into the WKWebView cookie store.
        func webView(_ webView: WKWebView,
                     decidePolicyFor navigationAction: WKNavigationAction,
                     decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
            if let url = navigationAction.request.url,
               let host = url.host,
               (host == "accounts.google.com" || url.path.contains("/api/auth/oauth/google")) {
                decisionHandler(.cancel)
                DispatchQueue.main.async { self.triggerNativeSignIn(webView: webView) }
                return
            }
            decisionHandler(.allow)
        }

        private func triggerNativeSignIn(webView: WKWebView) {
            guard let windowScene = UIApplication.shared.connectedScenes.first as? UIWindowScene,
                  let rootVC = windowScene.windows.first?.rootViewController else { return }

            GIDSignIn.sharedInstance.signIn(withPresenting: rootVC) { [weak self] result, error in
                guard let self, error == nil, let idToken = result?.user.idToken?.tokenString else {
                    print("[WebView] Native re-auth failed: \(error?.localizedDescription ?? "no token")")
                    return
                }
                self.exchangeToken(idToken: idToken, webView: webView)
            }
        }

        private func exchangeToken(idToken: String, webView: WKWebView) {
            guard let endpointURL = URL(string: "\(serverURL)/api/auth/google-id-token") else { return }
            var request = URLRequest(url: endpointURL)
            request.httpMethod = "POST"
            request.addValue("application/json", forHTTPHeaderField: "Content-Type")
            // This code path is re-auth only — the user already accepted ToS at first
            // login via ContentView. New-user signup always goes through ContentView
            // which shows the ToS checkbox before calling this endpoint.
            request.httpBody = try? JSONSerialization.data(withJSONObject: [
                "id_token": idToken,
                "tos_accepted": true,
            ])

            URLSession.shared.dataTask(with: request) { [weak webView, serverURL] data, response, error in
                if let error = error {
                    print("[WebView] Token exchange network error: \(error)")
                    return
                }
                guard let httpResponse = response as? HTTPURLResponse else {
                    print("[WebView] Token exchange: no HTTP response")
                    return
                }
                print("[WebView] Token exchange status: \(httpResponse.statusCode)")
                guard httpResponse.statusCode == 200,
                      let baseURL = URL(string: serverURL) else {
                    if let data, let body = String(data: data, encoding: .utf8) {
                        print("[WebView] Token exchange failed: \(body)")
                    }
                    return
                }

                // allHeaderFields is [AnyHashable: Any] — convert safely, not with force cast.
                var stringHeaders: [String: String] = [:]
                for (key, value) in httpResponse.allHeaderFields {
                    if let k = key as? String, let v = value as? String {
                        stringHeaders[k] = v
                    }
                }
                print("[WebView] Response headers: \(stringHeaders.filter { $0.key.lowercased().contains("cookie") })")

                // Parse Set-Cookie headers properly and find hc_session by name.
                let cookies = HTTPCookie.cookies(withResponseHeaderFields: stringHeaders, for: baseURL)
                print("[WebView] Parsed cookies: \(cookies.map { "\($0.name)=\($0.value.prefix(8))..." })")
                guard let sessionCookie = cookies.first(where: { $0.name == "hc_session" }) else {
                    print("[WebView] Token exchange: hc_session not found in response")
                    return
                }

                let cookieValue = sessionCookie.value
                UserDefaults.standard.set(cookieValue, forKey: "sessionCookie")

                // Inject the new session cookie into the WKWebView store, then reload.
                guard let host = baseURL.host,
                      let httpCookie = HTTPCookie(properties: [
                          .name: "hc_session", .value: cookieValue,
                          .domain: host, .path: "/",
                      ]) else { return }

                DispatchQueue.main.async {
                    webView?.configuration.websiteDataStore.httpCookieStore.setCookie(httpCookie) {
                        print("[WebView] Re-auth cookie set, reloading")
                        webView?.reload()
                    }
                }
            }.resume()
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
