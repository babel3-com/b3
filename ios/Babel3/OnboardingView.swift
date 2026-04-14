import SwiftUI

/// First-launch onboarding — shows gesture controls, then dismisses forever.
struct OnboardingView: View {
    @Binding var hasSeenOnboarding: Bool
    
    var body: some View {
        ZStack {
            Color.black.opacity(0.85)
                .ignoresSafeArea()
            
            VStack(spacing: 32) {
                Text("Babel3")
                    .font(.system(size: 36, weight: .bold))
                    .foregroundColor(.white)
                
                Text("Gesture Controls")
                    .font(.system(size: 18, weight: .medium))
                    .foregroundColor(.gray)
                
                VStack(alignment: .leading, spacing: 24) {
                    gestureRow(
                        icon: "arrow.right",
                        edge: "Left edge →",
                        action: "Start / Stop Recording",
                        detail: "Swipe right from the left edge"
                    )
                    
                    gestureRow(
                        icon: "arrow.left",
                        edge: "Right edge ←",
                        action: "Toggle Selfie Camera",
                        detail: "Swipe left from the right edge"
                    )
                    
                    gestureRow(
                        icon: "hand.draw",
                        edge: "Selfie bubble",
                        action: "Drag to Reposition",
                        detail: "Move the camera anywhere"
                    )
                }
                .padding(.horizontal, 24)
                
                Text("No buttons. No UI. Just gestures.")
                    .font(.system(size: 14))
                    .foregroundColor(.gray)
                    .padding(.top, 8)
                
                Button(action: {
                    hasSeenOnboarding = true
                    UserDefaults.standard.set(true, forKey: "hasSeenOnboarding")
                }) {
                    Text("Got it")
                        .font(.system(size: 18, weight: .semibold))
                        .frame(maxWidth: .infinity)
                        .padding()
                        .background(Color.blue)
                        .foregroundColor(.white)
                        .cornerRadius(12)
                }
                .padding(.horizontal, 40)
                .padding(.top, 16)
            }
        }
    }
    
    func gestureRow(icon: String, edge: String, action: String, detail: String) -> some View {
        HStack(spacing: 16) {
            Image(systemName: icon)
                .font(.system(size: 24))
                .foregroundColor(.blue)
                .frame(width: 40)
            
            VStack(alignment: .leading, spacing: 2) {
                Text(action)
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundColor(.white)
                Text(detail)
                    .font(.system(size: 14))
                    .foregroundColor(.gray)
            }
        }
    }
}
