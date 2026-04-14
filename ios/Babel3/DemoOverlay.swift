import SwiftUI
import ReplayKit
import AVFoundation

/// Demo overlay — gesture-only controls, no visible UI except the selfie bubble.
///
/// Gestures:
///   - Swipe right from left edge → start/stop recording
///   - Swipe left from right edge → toggle selfie camera
///   - Drag selfie bubble → reposition
struct DemoOverlay: View {
    @StateObject private var recorder = DemoRecorder()
    @State private var showSelfie = false
    @State private var selfiePosition: CGPoint = CGPoint(x: 90, y: 120)
    @State private var selfieBaseSize: CGSize = CGSize(width: 140, height: 180)
    @State private var selfieScale: CGFloat = 1.0
    @State private var selfieSteadyScale: CGFloat = 1.0  // committed scale between gestures
    @State private var selfieCamera: AVCaptureDevice.Position = .front

    private let selfieMinScale: CGFloat = 0.5   // ~70×90
    private let selfieMaxScale: CGFloat = 2.5   // ~350×450

    var body: some View {
        ZStack {
            // Selfie camera bubble (draggable, pinch-to-resize, long-press to flip)
            if showSelfie {
                let w = selfieBaseSize.width * selfieScale
                let h = selfieBaseSize.height * selfieScale
                SelfieBubble(cameraPosition: selfieCamera)
                    .frame(width: w, height: h)
                    .clipShape(RoundedRectangle(cornerRadius: 20))
                    .overlay(RoundedRectangle(cornerRadius: 20)
                        .stroke(Color.white.opacity(0.2), lineWidth: 1.5))
                    .shadow(radius: 8)
                    .position(selfiePosition)
                    .gesture(
                        DragGesture()
                            .onChanged { value in
                                selfiePosition = value.location
                            }
                        .simultaneously(with:
                            MagnificationGesture()
                                .onChanged { value in
                                    // value is cumulative since gesture start — multiply against
                                    // the committed steady scale, not the current selfieScale.
                                    let proposed = selfieSteadyScale * value
                                    selfieScale = min(selfieMaxScale, max(selfieMinScale, proposed))
                                }
                                .onEnded { _ in
                                    selfieSteadyScale = selfieScale
                                }
                        )
                        .simultaneously(with:
                            LongPressGesture(minimumDuration: 0.6)
                                .onEnded { _ in
                                    selfieCamera = (selfieCamera == .front) ? .back : .front
                                    UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                                }
                        )
                    )
            }
            
            // Recording indicator — subtle red dot only
            if recorder.isRecording {
                Circle()
                    .fill(Color.red)
                    .frame(width: 8, height: 8)
                    .position(x: 20, y: 54)
            }
            
            // Gesture areas (invisible, middle band avoids menu icon at top and mic at bottom)
            VStack {
                Spacer().frame(height: 100) // Below menu icon / status bar
                HStack(spacing: 0) {
                    // Left edge swipe right → toggle recording
                    Color.white.opacity(0.001)
                        .frame(width: 44)
                        .contentShape(Rectangle())
                        .gesture(
                            DragGesture(minimumDistance: 30)
                                .onEnded { value in
                                    if value.translation.width > 50 {
                                        recorder.toggle()
                                        UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                                    }
                                }
                        )
                    Spacer()
                    // Right edge swipe left → toggle selfie
                    Color.white.opacity(0.001)
                        .frame(width: 44)
                        .contentShape(Rectangle())
                        .gesture(
                            DragGesture(minimumDistance: 30)
                                .onEnded { value in
                                    if value.translation.width < -50 {
                                        showSelfie.toggle()
                                        UIImpactFeedbackGenerator(style: .light).impactOccurred()
                                    }
                                }
                        )
                }
                .frame(height: UIScreen.main.bounds.height * 0.35)
                Spacer()
            }
        }
    }
}

class DemoRecorder: NSObject, ObservableObject {
    @Published var isRecording = false
    private var recordingPendingTimeoutItem: DispatchWorkItem?  // clears isRecordingPending if startRecording callback never fires

    /// Log via NativeBridge so it appears in browser_console (not just Xcode)
    private func log(_ msg: String) {
        NativeBridge.shared.nativeLog(msg)
    }

    /// Log full engine + session + recorder state at a named transition point
    private func logState(_ label: String) {
        let nb = NativeBridge.shared
        let rec = RPScreenRecorder.shared()
        let session = AVAudioSession.sharedInstance()
        let outputs = session.currentRoute.outputs.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",")
        let engineAddr: String
        if let eng = nb.audioEngine {
            engineAddr = String(describing: Unmanaged.passUnretained(eng).toOpaque())
        } else {
            engineAddr = "nil"
        }

        log("[DEMO-DIAG] \(label): rec.isRecording=\(rec.isRecording) rec.isMicEnabled=\(rec.isMicrophoneEnabled) micActive=\(nb.isMicActive)")
        if let engine = nb.audioEngine {
            log("[DEMO-DIAG] \(label): engine=\(engineAddr) running=\(engine.isRunning) vp=\(engine.inputNode.isVoiceProcessingEnabled) outFmt=\(engine.outputNode.outputFormat(forBus: 0).sampleRate)/\(engine.outputNode.outputFormat(forBus: 0).channelCount)ch")
        } else {
            log("[DEMO-DIAG] \(label): NO ENGINE")
        }
        log("[DEMO-DIAG] \(label): session cat=\(session.category.rawValue) mode=\(session.mode.rawValue) route=\(outputs)")
    }

    func toggle() {
        if isRecording { stopRecording() }
        else { startRecording() }
    }

    func startRecording() {
        let rec = RPScreenRecorder.shared()
        rec.isMicrophoneEnabled = true

        let micWasActive = NativeBridge.shared.isMicActive
        log("[Demo] startRecording called (micWasActive=\(micWasActive))")
        logState("pre-recording-start")

        // When USB audio (e.g. Lark A1) is connected, skip AVAudioEngine entirely.
        // - Mic capture uses AVCaptureSession (not inputNode)
        // - TTS uses AVAudioPlayer (not playerNode)
        // - RPScreenRecorder handles its own recording pipeline
        // Creating the engine with VPIO would permanently de-enumerate the USB device
        // (process-scoped), breaking mic capture for the rest of the session.
        let usbConnected = NativeBridge.shared.isUSBAudioConnected()
        log("[Demo] startRecording: usbConnected=\(usbConnected)")

        if !usbConnected {
            // Ensure engine exists BEFORE starting recording.
            // The engine must go through VP-on → VP-off to initialize the mic hardware
            // connection. Without this, a later mic tap can't access the mic (0Hz format).
            NativeBridge.shared.ensureEngine()
        }

        // Set isRecordingPending BEFORE disabling VP. The VP disable triggers
        // AVAudioEngineConfigurationChange ~827ms later. If the flag isn't set first,
        // the configChange handler sees isRecording=false (startRecording callback hasn't
        // fired yet) and re-enables VPIO — causing the entire recording to run with VPIO
        // active and the asset writer to fail on longer recordings (error -5823).
        NativeBridge.shared.isRecordingPending = true
        log("[Demo] isRecordingPending = true")

        // Safety timeout: if startRecording callback never fires (e.g. app backgrounded
        // during setup, or RPScreenRecorder internal error), clear the flag after 10s so
        // VPIO isn't permanently suppressed.
        let timeoutItem = DispatchWorkItem { [weak self] in
            guard NativeBridge.shared.isRecordingPending else { return }
            NativeBridge.shared.isRecordingPending = false
            self?.log("[Demo] isRecordingPending timeout — cleared (startRecording callback never fired)")
        }
        recordingPendingTimeoutItem = timeoutItem
        DispatchQueue.main.asyncAfter(deadline: .now() + 10, execute: timeoutItem)

        // Disable voice processing — VP is incompatible with RPScreenRecorder
        // (recording captures no audio when VP is on).
        // Skip when USB connected: engine was never created, no VPIO to disable.
        if !usbConnected, let engine = NativeBridge.shared.audioEngine {
            engine.stop()
            log("[Demo] engine.stop() done")
            try? engine.inputNode.setVoiceProcessingEnabled(false)
            log("[Demo] VP disabled")
            try? engine.start()
            NativeBridge.shared.playerNode?.play()
            try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)
            log("[Demo] Engine restarted without VP")
            logState("post-vp-disable")
        }

        rec.startRecording { [weak self] error in
            DispatchQueue.main.async {
                // Clear pending flag (and cancel timeout) regardless of success or failure.
                self?.recordingPendingTimeoutItem?.cancel()
                self?.recordingPendingTimeoutItem = nil
                NativeBridge.shared.isRecordingPending = false
                self?.log("[Demo] isRecordingPending = false (startRecording callback fired)")

                if let error = error {
                    self?.log("[Demo] Recording failed: \(error.localizedDescription)")
                    return
                }
                self?.isRecording = true
                self?.log("[Demo] Recording started successfully (micWasActive=\(micWasActive))")
                self?.logState("post-recording-started")

                // Auto-restart mic if it was active before recording disrupted it.
                // The mic will restart on the non-VP engine, allowing transcription
                // and recording to coexist.
                if micWasActive {
                    self?.log("[Demo] Auto-restarting mic (was active before recording)")
                    NativeBridge.shared.stopMic()
                    NativeBridge.shared.startMic()
                }
            }
        }
    }

    func stopRecording() {
        log("[Demo] stopRecording called")
        logState("pre-recording-stop")

        RPScreenRecorder.shared().stopRecording { [weak self] preview, error in
            DispatchQueue.main.async {
                self?.isRecording = false

                if let error = error {
                    self?.log("[Demo] Stop failed: \(error.localizedDescription)")
                    self?.reEnableVoiceProcessing()
                    return
                }
                self?.log("[Demo] Recording stopped")
                self?.logState("post-recording-stopped")

                if let preview = preview {
                    preview.previewControllerDelegate = self
                    if let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene,
                       let root = scene.windows.first?.rootViewController {
                        root.present(preview, animated: true)
                    }
                } else {
                    self?.log("[Demo] No preview controller returned")
                }
                // Re-enable voice processing after preview is shown
                self?.reEnableVoiceProcessing()
            }
        }
    }

    private func reEnableVoiceProcessing() {
        // When USB connected, engine was never created — nothing to re-enable.
        guard !NativeBridge.shared.isUSBAudioConnected() else {
            log("[Demo] reEnableVoiceProcessing: skipped (USB connected, no engine)")
            return
        }
        guard let engine = NativeBridge.shared.audioEngine else { return }
        logState("pre-vp-reenable")
        engine.stop()
        try? engine.inputNode.setVoiceProcessingEnabled(true)
        try? engine.start()
        NativeBridge.shared.playerNode?.play()
        try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)
        log("[Demo] Voice processing re-enabled")
        logState("post-vp-reenable")
    }
}

extension DemoRecorder: RPPreviewViewControllerDelegate {
    func previewControllerDidFinish(_ previewController: RPPreviewViewController) {
        previewController.dismiss(animated: true)
    }
}

/// UIView whose root layer is AVCaptureVideoPreviewLayer.
/// Using layerClass means the preview layer IS the view's bounds layer —
/// it resizes automatically with the view, so pinch-to-resize always fills.
class PreviewView: UIView {
    override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }
    var previewLayer: AVCaptureVideoPreviewLayer { layer as! AVCaptureVideoPreviewLayer }
}

struct SelfieBubble: UIViewRepresentable {
    var cameraPosition: AVCaptureDevice.Position

    func makeUIView(context: Context) -> PreviewView {
        let view = PreviewView()
        view.backgroundColor = .black
        view.previewLayer.videoGravity = .resizeAspectFill
        context.coordinator.buildSession(in: view, position: cameraPosition)
        return view
    }

    func updateUIView(_ uiView: PreviewView, context: Context) {
        // Rebuild session only when camera position actually changed.
        // Frame tracking is automatic via layerClass — no manual frame sync needed.
        guard context.coordinator.currentPosition != cameraPosition else { return }
        context.coordinator.buildSession(in: uiView, position: cameraPosition)
    }

    static func dismantleUIView(_ uiView: PreviewView, coordinator: Coordinator) {
        coordinator.session?.stopRunning()
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    class Coordinator {
        var session: AVCaptureSession?
        var currentPosition: AVCaptureDevice.Position = .front

        func buildSession(in view: PreviewView, position: AVCaptureDevice.Position) {
            session?.stopRunning()
            session = nil
            currentPosition = position

            let newSession = AVCaptureSession()
            newSession.sessionPreset = .medium

            guard let camera = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: position),
                  let input = try? AVCaptureDeviceInput(device: camera) else { return }

            if newSession.canAddInput(input) { newSession.addInput(input) }

            view.previewLayer.session = newSession
            view.previewLayer.connection?.automaticallyAdjustsVideoMirroring = false
            // Mirror only front camera (selfie mode)
            view.previewLayer.connection?.isVideoMirrored = (position == .front)

            DispatchQueue.global(qos: .userInitiated).async { newSession.startRunning() }
            session = newSession
        }
    }
}
