import SwiftUI
import GoogleSignIn
import GoogleSignInSwift

struct ContentView: View {
    @State private var sessionCookie: String? = UserDefaults.standard.string(forKey: "sessionCookie")
    @State private var hasSeenOnboarding = UserDefaults.standard.bool(forKey: "hasSeenOnboarding")
    @State private var isLoggingIn = false
    @State private var errorMessage: String?
    @State private var tosAccepted = false
    
    let serverURL = "https://babel3.com"
    let dashboardURL = "https://babel3.com/account#agents"
    
    var body: some View {
        Group {
            if sessionCookie != nil {
                ZStack {
                    DashboardView(sessionCookie: $sessionCookie, dashboardURL: dashboardURL)
                        .ignoresSafeArea()
                    
                    // Demo mode overlay (record button + selfie)
                    DemoOverlay()
                    

                }
            } else {
                loginView
            }
        }
        .sheet(isPresented: .init(
            get: { !hasSeenOnboarding && sessionCookie != nil },
            set: { if !$0 { hasSeenOnboarding = true; UserDefaults.standard.set(true, forKey: "hasSeenOnboarding") } }
        )) {
            OnboardingView(hasSeenOnboarding: $hasSeenOnboarding)
        }
    }
    
    var loginView: some View {
        ZStack {
            Color(red: 0.05, green: 0.07, blue: 0.09)
                .ignoresSafeArea()
            
            VStack(spacing: 24) {
                Text("Babel3")
                    .font(.system(size: 32, weight: .bold))
                    .foregroundColor(.white)
                
                Text("Voice interface for Claude Code")
                    .font(.subheadline)
                    .foregroundColor(.gray)
                
                if let error = errorMessage {
                    Text(error)
                        .font(.caption)
                        .foregroundColor(.red)
                        .padding(.horizontal)
                        .multilineTextAlignment(.center)
                }
                
                HStack(alignment: .top, spacing: 8) {
                    Toggle("", isOn: $tosAccepted)
                        .labelsHidden()
                        .tint(.blue)
                    VStack(alignment: .leading, spacing: 2) {
                        Text("I agree to the")
                            .font(.caption)
                            .foregroundColor(.gray)
                        HStack(spacing: 4) {
                            Link("Terms of Service", destination: URL(string: "\(serverURL)/tos")!)
                                .font(.caption)
                            Text("and")
                                .font(.caption)
                                .foregroundColor(.gray)
                            Link("Privacy Policy", destination: URL(string: "\(serverURL)/privacy")!)
                                .font(.caption)
                        }
                    }
                }
                .padding(.horizontal, 40)

                GoogleSignInButton(style: .wide, action: signInWithGoogle)
                    .frame(height: 50)
                    .padding(.horizontal, 40)
                    .disabled(isLoggingIn || !tosAccepted)
                    .opacity(tosAccepted ? 1.0 : 0.5)

                if isLoggingIn {
                    ProgressView()
                        .tint(.white)
                }
            }
        }
    }

    func signInWithGoogle() {
        guard tosAccepted else {
            errorMessage = "Please accept the Terms of Service to continue."
            return
        }
        isLoggingIn = true
        errorMessage = nil
        
        guard let windowScene = UIApplication.shared.connectedScenes.first as? UIWindowScene,
              let rootVC = windowScene.windows.first?.rootViewController else {
            errorMessage = "Could not find root view controller"
            isLoggingIn = false
            return
        }
        
        GIDSignIn.sharedInstance.signIn(withPresenting: rootVC) { result, error in
            if let error = error {
                errorMessage = "Sign-in failed: \(error.localizedDescription)"
                isLoggingIn = false
                return
            }
            
            guard let idToken = result?.user.idToken?.tokenString else {
                errorMessage = "No ID token received from Google"
                isLoggingIn = false
                return
            }
            
            exchangeTokenWithServer(idToken: idToken)
        }
    }
    
    func exchangeTokenWithServer(idToken: String) {
        guard let url = URL(string: "\(serverURL)/api/auth/google-id-token") else { return }
        
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.addValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: [
            "id_token": idToken,
            "tos_accepted": tosAccepted,
        ])
        
        URLSession.shared.dataTask(with: request) { data, response, error in
            DispatchQueue.main.async {
                isLoggingIn = false
                
                if let error = error {
                    errorMessage = "Server error: \(error.localizedDescription)"
                    return
                }
                
                guard let httpResponse = response as? HTTPURLResponse else {
                    errorMessage = "Invalid server response"
                    return
                }
                
                if httpResponse.statusCode == 200,
                   let setCookie = httpResponse.allHeaderFields["Set-Cookie"] as? String {
                    let parts = setCookie.split(separator: ";")
                    if let sessionPart = parts.first,
                       let token = sessionPart.split(separator: "=", maxSplits: 1).last {
                        let cookieValue = String(token)
                        UserDefaults.standard.set(cookieValue, forKey: "sessionCookie")
                        sessionCookie = cookieValue
                        return
                    }
                }
                
                if let data = data,
                   let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                   let msg = body["message"] as? String {
                    errorMessage = msg
                } else {
                    errorMessage = "Login failed (HTTP \(httpResponse.statusCode))"
                }
            }
        }.resume()
    }
}
