import Foundation
import AVFoundation
import AVKit
import WebKit
import ReplayKit

/// NativeBridge — owns both the microphone and speaker through AVAudioEngine.
/// Running mic + speaker on the same audio engine enables hardware echo cancellation:
/// iOS knows what's being played and subtracts it from the mic input.
class NativeBridge: NSObject, ObservableObject {
    static let shared = NativeBridge()

    @Published var isMicActive = false

    var audioEngine: AVAudioEngine?
    var playerNode: AVAudioPlayerNode?
    weak var webView: WKWebView?
    private var isConfigured = false
    private var routePicker: AVRoutePickerView?  // Retained so the picker sheet stays alive

    static let version = APP_VERSION

    private let sampleRate: Double = 16000
    private let bufferSize: AVAudioFrameCount = 4096
    private var audioChunkCount: Int = 0
    private var playCount: Int = 0
    private var engineCreatedAt: Date?  // Timestamp when ensureEngine creates engine
    private var isEngineReady = false   // True after config change handler completes rebuild
    private var pendingAudio: [String] = []  // Base64 audio queued while engine not ready
    private var deviceUpdateWorkItem: DispatchWorkItem?  // Debounce token for pushDeviceUpdateToWebApp

    // AVCaptureSession path — used for USBAudio inputs (e.g. Hollyland Lark A1).
    // VPIO (AVAudioEngine's VoiceProcessingIO) permanently de-enumerates USBAudio devices
    // from availableInputs when engine.start() is called. AVCaptureSession bypasses VPIO
    // entirely and can capture from any input including USBAudio.
    // When USB is connected, TTS playback also avoids AVAudioEngine — uses AVAudioPlayer
    // instead (writes WAV to temp file, plays through session output route) so VPIO never
    // runs and the Lark stays enumerated for the next mic press.
    private var captureSession: AVCaptureSession?
    private var captureOutput: AVCaptureAudioDataOutput?
    private var captureConverter: AVAudioConverter?
    private var captureTargetFormat: AVAudioFormat?
    private var isCaptureMicActive = false  // true when AVCaptureSession path is active
    private var audioPlayers: [AVAudioPlayer] = []  // Retained for TTS when USBAudio connected (no VPIO); multiple may overlap
    private var captureWasPausedForTTS = false  // true when capture was stopped to prevent TTS feedback; restarted after last chunk
    private var recMicWasMutedForTTS = false    // true when RPScreenRecorder mic muted during TTS; unmuted after last chunk
    var isRecordingPending = false  // set before rec.startRecording, cleared in callback; guards configChange from re-enabling VPIO in the race window
    private var lastAppliedUSBState: Bool?  // nil = never applied; guards applySessionCategory against infinite loop (setCategory → categoryChange → applySessionCategory → setCategory…)

    /// Log to both Xcode console AND browser console (accessible via browser_console MCP tool)
    /// Uses HC.log() which feeds into the dashboard's log ring buffer,
    /// not console.log() which isn't captured by browser_console.
    func nativeLog(_ msg: String) {
        print(msg)
        let escaped = msg.replacingOccurrences(of: "'", with: "\\'").replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\n", with: " ")
        DispatchQueue.main.async { [weak self] in
            self?.webView?.evaluateJavaScript("if(typeof HC!=='undefined'&&HC.log)HC.log('\(escaped)');else console.log('\(escaped)');")
        }
    }

    func attach(to webView: WKWebView) {
        self.webView = webView
        let script = WKUserScript(source: Self.bridgeJS, injectionTime: .atDocumentStart, forMainFrameOnly: true)
        webView.configuration.userContentController.addUserScript(script)
    }

    /// D1+D2: Log comprehensive engine and audio session state for diagnostics.
    /// Call at key transition points to compare broken vs working states.
    func logEngineState(_ label: String) {
        let session = AVAudioSession.sharedInstance()
        let outputs = session.currentRoute.outputs.map { "\($0.portName)(\($0.portType.rawValue) ch=\($0.channels?.count ?? 0))" }.joined(separator: ",")
        let inputs = session.currentRoute.inputs.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",")
        let cat = session.category.rawValue
        let mode = session.mode.rawValue
        nativeLog("[DIAG] \(label): route=\(outputs) input=\(inputs) cat=\(cat) mode=\(mode)")
        // D1: availableInputs — tells us if device is de-enumerated (missing here) vs de-routed (missing from currentRoute.inputs but present here)
        let available = session.availableInputs?.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",") ?? "none"
        nativeLog("[DIAG] \(label): availableInputs=\(available)")

        if let engine = audioEngine {
            let outFmt = engine.outputNode.outputFormat(forBus: 0)
            let outInFmt = engine.outputNode.inputFormat(forBus: 0)
            let mixFmt = engine.mainMixerNode.outputFormat(forBus: 0)
            let addr = String(describing: Unmanaged.passUnretained(engine).toOpaque())
            nativeLog("[DIAG] \(label): engine=\(addr) running=\(engine.isRunning) outNode.outFmt=\(outFmt.sampleRate)/\(outFmt.channelCount)ch outNode.inFmt=\(outInFmt.sampleRate)/\(outInFmt.channelCount)ch mixer=\(mixFmt.sampleRate)/\(mixFmt.channelCount)ch vp=\(engine.inputNode.isVoiceProcessingEnabled)")
            if let player = playerNode {
                let playerFmt = player.outputFormat(forBus: 0)
                nativeLog("[DIAG] \(label): player playing=\(player.isPlaying) fmt=\(playerFmt.sampleRate)/\(playerFmt.channelCount)ch")
            }
        } else {
            nativeLog("[DIAG] \(label): NO ENGINE")
        }
    }

    func configureAudioSession() {
        guard !isConfigured else { return }
        nativeLog("[NativeBridge] v\(Self.version) starting (configureAudioSession)")

        // Monitor engine config changes (Bluetooth, route changes, VP toggled by external code)
        // RPScreenRecorder and WKWebView can reconfigure the audio session behind our back,
        // disabling voice processing and changing sample rates. This causes format mismatches
        // (chipmunk playback) and crashes. Full rebuild on any config change.
        NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self = self, let engine = self.audioEngine else { return }
            let sinceCreate = self.engineCreatedAt.map { String(format: "%.0f", Date().timeIntervalSince($0) * 1000) } ?? "?"
            self.nativeLog("[NativeBridge] Engine config changed — full rebuild (vp=\(engine.inputNode.isVoiceProcessingEnabled) running=\(engine.isRunning))")
            self.nativeLog("[DIAG] configChange: \(sinceCreate)ms after engine creation, thread=\(Thread.isMainThread ? "main" : "background")")

            engine.stop()

            // Rebuild player node (prevents zombie state)
            if let oldPlayer = self.playerNode {
                engine.detach(oldPlayer)
            }
            let player = AVAudioPlayerNode()
            engine.attach(player)
            let mixerFormat = engine.mainMixerNode.outputFormat(forBus: 0)
            engine.connect(player, to: engine.mainMixerNode, format: mixerFormat)
            self.playerNode = player

            // Re-enable voice processing — but NOT during screen recording, and NOT
            // while AVCaptureSession is actively capturing (VPIO + engine.start() would
            // de-enumerate USBAudio from availableInputs, potentially breaking live capture).
            // When capture is NOT active, VPIO is allowed — AVAudioEngine requires VPIO
            // to run even for output-only (TTS). After VPIO de-enumerates USBAudio from
            // availableInputs, currentRoute.inputs may still show USBAudio — startMic()
            // checks currentRoute (via isUSBAudioActive) not availableInputs, so the
            // AVCaptureSession path remains reachable.
            // Guard includes isRecordingPending: the configChange triggered by VP disable
            // (~827ms delay) fires before rec.startRecording's async callback sets
            // isRecording=true. Without this flag, the handler would re-enable VP in that
            // window and the entire recording would run with VPIO active.
            if !RPScreenRecorder.shared().isRecording && !self.isRecordingPending && !self.isCaptureMicActive {
                try? engine.inputNode.setVoiceProcessingEnabled(true)
                engine.inputNode.isVoiceProcessingAGCEnabled = false
            } else if self.isCaptureMicActive {
                self.nativeLog("[NativeBridge] Config change: skipping VP re-enable — AVCaptureSession capturing")
            } else if self.isRecordingPending || RPScreenRecorder.shared().isRecording {
                self.nativeLog("[NativeBridge] Config change: skipping VP re-enable — recording active (pending=\(self.isRecordingPending))")
            }

            // Assert persisted preferred input after VP re-enable, before engine.start().
            // Same pre-start window as ensureEngine() — Lark still in availableInputs here.
            if let uid = UserDefaults.standard.string(forKey: "NativeBridge.preferredInputUid"),
               let port = AVAudioSession.sharedInstance().availableInputs?.first(where: { $0.uid == uid }) {
                do {
                    try self.setPreferredInputPreservingOutput(port)
                    self.nativeLog("[NativeBridge] configChange: pre-start preferredInput set to '\(port.portName)' (\(port.portType.rawValue))")
                } catch {
                    self.nativeLog("[NativeBridge] configChange: pre-start setPreferredInput failed: \(error)")
                }
            }

            // Reinstall mic tap if was active
            if self.isMicActive {
                // Tap was lost — mic will need to be restarted by JS
                self.isMicActive = false
                DispatchQueue.main.async {
                    self.webView?.evaluateJavaScript("window.NativeBridge._micStopped()")
                }
                self.nativeLog("[NativeBridge] Mic tap lost — JS notified")
            }

            do {
                try engine.start()
                // D5: Which engine.start() resets preferredInput to nil?
                let prefInput1 = AVAudioSession.sharedInstance().preferredInput?.portName ?? "nil"
                self.nativeLog("[DIAG] post-engine-start(configChange-rebuild): preferredInput=\(prefInput1)")
                player.play()
                try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)
                self.nativeLog("[NativeBridge] Engine rebuilt (sr=\(mixerFormat.sampleRate) vp=\(engine.inputNode.isVoiceProcessingEnabled))")
                self.logEngineState("after-configChange-rebuild")

                // Engine is now stable after VP config change settled.
                // Set ready flag and drain any audio that was queued while waiting.
                self.isEngineReady = true
                self.nativeLog("[DIAG] isEngineReady: false→true (source=configChange, pending=\(self.pendingAudio.count))")
                if !self.pendingAudio.isEmpty {
                    let queued = self.pendingAudio
                    self.pendingAudio = []
                    self.nativeLog("[DIAG] draining \(queued.count) pending audio items")
                    for b64 in queued {
                        self.playAudio(base64: b64)
                    }
                }
            } catch {
                self.nativeLog("[NativeBridge] Engine rebuild failed: \(error)")
            }
        }

        // D3: Monitor route changes to see if something changes output between ensureEngine and playAudio
        NotificationCenter.default.addObserver(
            forName: AVAudioSession.routeChangeNotification, object: nil, queue: nil
        ) { [weak self] notification in
            guard let self = self else { return }
            let reason = (notification.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt).flatMap { AVAudioSession.RouteChangeReason(rawValue: $0) }
            let reasonStr: String
            switch reason {
            case .newDeviceAvailable: reasonStr = "newDevice"
            case .oldDeviceUnavailable: reasonStr = "oldDeviceGone"
            case .categoryChange: reasonStr = "categoryChange"
            case .override: reasonStr = "override"
            case .wakeFromSleep: reasonStr = "wake"
            case .routeConfigurationChange: reasonStr = "routeConfig"
            default: reasonStr = "other(\(reason?.rawValue ?? 999))"
            }
            let prev = (notification.userInfo?[AVAudioSessionRouteChangePreviousRouteKey] as? AVAudioSessionRouteDescription)?
                .outputs.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",") ?? "?"
            let curr = AVAudioSession.sharedInstance().currentRoute.outputs.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",")
            self.nativeLog("[DIAG] RouteChange: reason=\(reasonStr) prev=\(prev) curr=\(curr)")

            // Push updated device list to web app so the picker UI stays current
            self.pushDeviceUpdateToWebApp()
            // No auto-reapply of setPreferredInput on newDeviceAvailable — it causes iOS to
            // reset the output route to Speaker. The Lark is selected on the next startMic().

            // Re-apply session category on every route change — USB may appear without a
            // newDeviceAvailable event if it was connected before app launch and enumerated
            // asynchronously. Cheap call: just sets a category option, no pipeline rebuild.
            try? self.applySessionCategory()
            self.nativeLog("[NativeBridge] RouteChange: re-applied session category (reason=\(reasonStr))")
        }

        do {
            let session = AVAudioSession.sharedInstance()
            try applySessionCategory()
            try session.setActive(true)
            isConfigured = true
            // D4: Ground truth snapshot before any engine work touches the audio graph.
            let startupInputs = session.availableInputs?.map { "\($0.portName)(\($0.portType.rawValue) uid=\($0.uid))" }.joined(separator: ",") ?? "none"
            let startupActive = session.currentRoute.inputs.first?.portName ?? "none"
            nativeLog("[DIAG] post-setActive: availableInputs=\(startupInputs) activeInput=\(startupActive)")
            // Defer applyPersistedInput to the first routeChangeNotification after setActive.
            // availableInputs may be empty or stale immediately after setActive — iOS signals
            // the settled hardware state via the first route change. One-shot observer handles it.
            var startupObserver: NSObjectProtocol?
            startupObserver = NotificationCenter.default.addObserver(
                forName: AVAudioSession.routeChangeNotification, object: nil, queue: .main
            ) { [weak self] _ in
                guard let self = self else { return }
                if let obs = startupObserver { NotificationCenter.default.removeObserver(obs) }
                startupObserver = nil
                // Bug A fix: if no explicit selection has been persisted and a non-default
                // input is active (e.g. Lark plugged in at launch), save it now so the
                // pre-start preferred-input assert in ensureEngine() has a uid to act on.
                if let activePort = AVAudioSession.sharedInstance().currentRoute.inputs.first {
                    let uidKey = "NativeBridge.preferredInputUid"
                    let typeKey = "NativeBridge.preferredInputType"
                    if UserDefaults.standard.string(forKey: uidKey) == nil {
                        // No prior selection: persist both uid and type from current active input.
                        UserDefaults.standard.set(activePort.uid, forKey: uidKey)
                        UserDefaults.standard.set(activePort.portType.rawValue, forKey: typeKey)
                        self.nativeLog("[NativeBridge] startup: auto-persisted '\(activePort.portName)' (\(activePort.portType.rawValue))")
                    } else if UserDefaults.standard.string(forKey: typeKey) == nil {
                        // uid exists (older build) but type is missing — backfill type from
                        // current route only if the uid still matches, to avoid overwriting a
                        // USB selection with built-in when the Lark is not connected at launch.
                        let persistedUid = UserDefaults.standard.string(forKey: uidKey)
                        if persistedUid == activePort.uid {
                            UserDefaults.standard.set(activePort.portType.rawValue, forKey: typeKey)
                            self.nativeLog("[NativeBridge] startup: backfilled portType '\(activePort.portType.rawValue)' for existing uid")
                        } else {
                            self.nativeLog("[NativeBridge] startup: uid mismatch (persisted=\(persistedUid ?? "nil") active=\(activePort.uid)) — skipping type backfill")
                        }
                    }
                }
                // Re-apply session category now that USB may be enumerated (Lark connected
                // before launch appears in availableInputs after first route change settles).
                try? self.applySessionCategory()
                self.applyPersistedInput()
            }
        } catch {
            nativeLog("[NativeBridge] Audio session error: \(error)")
        }
    }

    /// Ensure the AVAudioEngine and player node exist.
    /// Called by both startMic() and playAudio() — the engine is shared
    /// so echo cancellation works between mic and speaker.
    @discardableResult
    func ensureEngine() -> Bool {
        if audioEngine != nil && playerNode != nil { return true }

        configureAudioSession()

        engineCreatedAt = Date()
        isEngineReady = false
        nativeLog("[DIAG] ensureEngine: creating new engine (isEngineReady=false)")
        let engine = AVAudioEngine()

        // Enable hardware echo cancellation BEFORE starting the engine — but NOT
        // during screen recording (VP is incompatible with RPScreenRecorder),
        // and NOT while AVCaptureSession is actively capturing from a USBAudio device.
        // AVAudioEngine REQUIRES VPIO to run, even for output-only (TTS). Blocking VPIO
        // when capture is not active prevents TTS from working. When capture IS active,
        // VPIO is blocked to protect the live AVCaptureSession pipeline.
        // After VPIO de-enumerates USBAudio from availableInputs, currentRoute.inputs
        // may still show USBAudio — isUSBAudioActive() checks currentRoute, so
        // startMic() still routes to AVCaptureSession on the next mic press.
        if !RPScreenRecorder.shared().isRecording && !isRecordingPending && !isCaptureMicActive {
            do {
                try engine.inputNode.setVoiceProcessingEnabled(true)
                engine.inputNode.isVoiceProcessingAGCEnabled = false
                nativeLog("[NativeBridge] Voice processing (AEC) enabled, AGC disabled")
                // D2: Capture availableInputs immediately after VP enable — if Lark disappears here,
                // VPIO itself (not engine.start) is the direct cause of de-enumeration.
                let postVpInputs = AVAudioSession.sharedInstance().availableInputs?.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",") ?? "none"
                nativeLog("[DIAG] post-VP-enable: availableInputs=\(postVpInputs)")
            } catch {
                nativeLog("[NativeBridge] Voice processing failed: \(error)")
            }
        } else if isCaptureMicActive {
            nativeLog("[NativeBridge] Skipping VP — AVCaptureSession capturing")
        } else {
            nativeLog("[NativeBridge] Skipping VP — recording active (pending=\(isRecordingPending))")
        }

        // Assert persisted preferred input BEFORE engine.start() while USBAudio devices
        // are still in availableInputs. D2 confirmed the Lark survives VP enable but is
        // de-enumerated by engine.start(). Locking preferred input here gives iOS the
        // chance to route VPIO through USB before the audio unit pipeline starts.
        if let uid = UserDefaults.standard.string(forKey: "NativeBridge.preferredInputUid"),
           let port = AVAudioSession.sharedInstance().availableInputs?.first(where: { $0.uid == uid }) {
            do {
                try setPreferredInputPreservingOutput(port)
                nativeLog("[NativeBridge] ensureEngine: pre-start preferredInput set to '\(port.portName)' (\(port.portType.rawValue))")
            } catch {
                nativeLog("[NativeBridge] ensureEngine: pre-start setPreferredInput failed: \(error)")
            }
        }

        // Attach player node for TTS playback
        let player = AVAudioPlayerNode()
        engine.attach(player)
        let mixerFormat = engine.mainMixerNode.outputFormat(forBus: 0)
        engine.connect(player, to: engine.mainMixerNode, format: mixerFormat)

        do {
            try engine.start()
            // D5: preferredInput after ensureEngine engine.start()
            let prefInput2 = AVAudioSession.sharedInstance().preferredInput?.portName ?? "nil"
            nativeLog("[DIAG] post-engine-start(ensureEngine): preferredInput=\(prefInput2)")
            player.play()
            try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)
            audioEngine = engine
            playerNode = player
            nativeLog("[NativeBridge] Engine started (sr=\(mixerFormat.sampleRate) ch=\(mixerFormat.channelCount) voiceProcessing=\(engine.inputNode.isVoiceProcessingEnabled))")
            logEngineState("after-ensureEngine")
            // KEY EXPERIMENT: after VPIO + engine.start() de-enumerates USBAudio from
            // availableInputs, does currentRoute.inputs still show USBAudio? If yes,
            // isUSBAudioActive() returns true and startMic() routes to AVCaptureSession.
            let postStartRoute = AVAudioSession.sharedInstance().currentRoute.inputs.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",")
            nativeLog("[DIAG] post-engine-start(ensureEngine): currentRoute.inputs=\(postStartRoute.isEmpty ? "none" : postStartRoute)")

            // D5: Schedule a tiny silent probe buffer to test if engine can actually play
            let probeFormat = engine.mainMixerNode.outputFormat(forBus: 0)
            if let probeBuf = AVAudioPCMBuffer(pcmFormat: probeFormat, frameCapacity: 4800) {
                probeBuf.frameLength = 4800 // 100ms at 48kHz
                // Buffer is zero-filled (silence)
                let probeStart = Date()
                player.scheduleBuffer(probeBuf) { [weak self] in
                    let elapsed = Date().timeIntervalSince(probeStart)
                    self?.nativeLog("[DIAG] probe-buffer completed in \(String(format: "%.1f", elapsed * 1000))ms (expected ~100ms)")
                }
            }

            return true
        } catch {
            nativeLog("[NativeBridge] Engine start failed: \(error)")
            return false
        }
    }

    func startMic() {
        nativeLog("[NativeBridge] startMic called (isMicActive=\(isMicActive) isCaptureMicActive=\(isCaptureMicActive))")
        logEngineState("startMic-entry")
        guard !isMicActive && !isCaptureMicActive else {
            nativeLog("[NativeBridge] startMic: already active, skipping")
            return
        }

        // USBAudio inputs (e.g. Hollyland Lark A1) cannot be captured via AVAudioEngine+VPIO.
        // Two-step check: currentRoute first (fast path), then persisted type (fallback for
        // when VPIO has already de-enumerated USBAudio from availableInputs AND currentRoute).
        if isUSBAudioActive() {
            nativeLog("[NativeBridge] startMic: USBAudio active (currentRoute) — using AVCaptureSession path")
            startMicViaCapture()
            return
        }
        // Fallback: if currentRoute lost USBAudio due to prior VPIO engine run, check the
        // persisted portType. If user's last explicit selection was USBAudio, tear down the
        // VPIO engine so the device re-enumerates, then start AVCaptureSession.
        let persistedType = UserDefaults.standard.string(forKey: "NativeBridge.preferredInputType")
        if persistedType == AVAudioSession.Port.usbAudio.rawValue {
            nativeLog("[NativeBridge] startMic: persisted input is USBAudio — deactivating session for USB re-enumeration")
            audioEngine?.stop()
            if let p = playerNode { audioEngine?.detach(p) }
            audioEngine = nil
            playerNode = nil
            isEngineReady = false
            do {
                try AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
                nativeLog("[NativeBridge] startMic: session deactivated — waiting for iOS to release hardware")
            } catch {
                nativeLog("[NativeBridge] startMic: session deactivation failed: \(error)")
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
                guard let self = self else { return }
                // Reactivate directly — do NOT call configureAudioSession() here.
                // configureAudioSession() registers permanent observers (configChange,
                // routeChange) and is guarded by isConfigured. Calling it again would
                // re-register a second set of permanent listeners. Just reactivate the
                // session and let the existing observers handle the new route state.
                do {
                    try AVAudioSession.sharedInstance().setActive(true)
                    self.nativeLog("[NativeBridge] startMic: session reactivated")
                } catch {
                    self.nativeLog("[NativeBridge] startMic: session reactivation failed: \(error)")
                }
                let available = AVAudioSession.sharedInstance().availableInputs?.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",") ?? "none"
                self.nativeLog("[DIAG] post-reactivation: availableInputs=\(available)")
                self.startMicViaCapture()
            }
            return
        }

        guard ensureEngine(), let engine = audioEngine else { nativeLog("[NativeBridge] startMic: ensureEngine failed"); return }

        // Must stop engine and fully rebuild player node before installing mic tap.
        // After engine.stop(), AVAudioPlayerNode can enter a zombie state where
        // isPlaying=true but audio doesn't flow. Detach + reattach fixes this.
        engine.stop()
        if let oldPlayer = playerNode {
            engine.detach(oldPlayer)
            playerNode = nil
        }
        let player = AVAudioPlayerNode()
        engine.attach(player)
        let mixerFormat = engine.mainMixerNode.outputFormat(forBus: 0)
        engine.connect(player, to: engine.mainMixerNode, format: mixerFormat)
        playerNode = player

        let inputNode = engine.inputNode

        // Read input format AFTER restarting the engine — on a stopped engine
        // (especially without VP), inputNode may report invalid formats (0ch, 0Hz).
        // Restart first, then read, then install tap.
        do {
            try engine.start()
            // D5: preferredInput after startMic pre-start (format-read engine.start())
            let prefInput3 = AVAudioSession.sharedInstance().preferredInput?.portName ?? "nil"
            nativeLog("[DIAG] post-engine-start(startMic-prestart): preferredInput=\(prefInput3)")
            nativeLog("[NativeBridge] startMic: engine pre-started for format read")
        } catch {
            nativeLog("[NativeBridge] startMic: pre-start failed: \(error)")
            return
        }

        let inputFormat = inputNode.outputFormat(forBus: 0)
        nativeLog("[NativeBridge] startMic: inputFormat=\(inputFormat.sampleRate)/\(inputFormat.channelCount)ch")

        // Validate format before installing tap
        guard inputFormat.sampleRate > 0 && inputFormat.channelCount > 0 else {
            nativeLog("[NativeBridge] startMic: invalid input format, aborting")
            return
        }

        engine.stop()  // Stop again to install tap cleanly

        guard let targetFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                                sampleRate: sampleRate,
                                                channels: 1,
                                                interleaved: false) else { return }

        guard let converter = AVAudioConverter(from: inputFormat, to: targetFormat) else {
            nativeLog("[NativeBridge] startMic: converter creation failed (input: \(inputFormat))")
            return
        }

        inputNode.installTap(onBus: 0, bufferSize: bufferSize, format: inputFormat) { [weak self] buffer, time in
            guard let self = self else { return }

            let frameCount = AVAudioFrameCount(Double(buffer.frameLength) * self.sampleRate / inputFormat.sampleRate)
            guard let convertedBuffer = AVAudioPCMBuffer(pcmFormat: targetFormat, frameCapacity: frameCount) else { return }

            var error: NSError?
            let status = converter.convert(to: convertedBuffer, error: &error) { inNumPackets, outStatus in
                outStatus.pointee = .haveData
                return buffer
            }

            guard status != .error, error == nil else { return }

            if let channelData = convertedBuffer.floatChannelData?[0] {
                let data = Data(bytes: channelData, count: Int(convertedBuffer.frameLength) * MemoryLayout<Float>.size)
                let base64 = data.base64EncodedString()

                self.audioChunkCount += 1
                DispatchQueue.main.async {
                    self.webView?.evaluateJavaScript(
                        "window.NativeBridge._receiveAudio('\(base64)')"
                    )
                    // Log every 100th chunk to avoid flooding
                    if self.audioChunkCount % 100 == 0 {
                        self.nativeLog("[NativeBridge] mic chunk #\(self.audioChunkCount)")
                    }
                }
            }
        }

        // Restart engine with tap installed
        do {
            try engine.start()
            // D5: preferredInput after final startMic engine.start() (with tap)
            let prefInput4 = AVAudioSession.sharedInstance().preferredInput?.portName ?? "nil"
            nativeLog("[DIAG] post-engine-start(startMic-final): preferredInput=\(prefInput4)")
            playerNode?.play()
            // Re-apply speaker override — engine restart resets audio routing
            try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)
            // Re-assert the persisted preferred input AFTER the final engine.start().
            // setVoiceProcessingEnabled(true) and each engine.start() with VPIO active
            // silently resets setPreferredInput — external/USB inputs (e.g. Lark A1,
            // portType USBAudio) are overridden back to built-in mic on every restart.
            // Calling applyPersistedInput() here, after the last engine start but before
            // the tap begins delivering audio, re-routes iOS to the correct device.
            applyPersistedInput()
            nativeLog("[NativeBridge] Engine restarted with mic tap (input: \(inputFormat) voiceProcessing=\(engine.inputNode.isVoiceProcessingEnabled) output=speaker)")
            logEngineState("after-startMic-restart")
            isEngineReady = true
            nativeLog("[DIAG] isEngineReady: →true (source=startMic)")
        } catch {
            nativeLog("[NativeBridge] Engine restart failed: \(error)")
            return
        }

        isMicActive = true
        DispatchQueue.main.async {
            self.webView?.evaluateJavaScript("window.NativeBridge._micStarted()")
        }
    }

    func stopMic() {
        nativeLog("[NativeBridge] stopMic called (isMicActive=\(isMicActive) isCaptureMicActive=\(isCaptureMicActive) chunks=\(audioChunkCount) engineRunning=\(audioEngine?.isRunning ?? false))")
        logEngineState("stopMic-entry")
        if isCaptureMicActive {
            stopMicCapture()
            return
        }
        audioChunkCount = 0
        audioEngine?.inputNode.removeTap(onBus: 0)
        isMicActive = false
        // Don't stop the engine — player may still need it for TTS
        DispatchQueue.main.async {
            self.webView?.evaluateJavaScript("window.NativeBridge._micStopped()")
        }
    }

    // MARK: - Audio Device Selection

    /// Returns available audio inputs as a JSON-serializable dict.
    func listAudioDevices() -> [String: Any] {
        let session = AVAudioSession.sharedInstance()
        let inputs = (session.availableInputs ?? []).map { port -> [String: String] in
            [
                "portName": port.portName,
                "portType": port.portType.rawValue,
                "uid": port.uid,
            ]
        }
        return ["inputs": inputs]
    }

    /// Selects the audio input with the given uid. Returns true on success.
    /// If the mic is active, waits for the route change notification before reinstalling
    /// the tap — avoids the race where inputNode.outputFormat reads 0Hz/0ch immediately
    /// after setPreferredInput before iOS has committed the route change.
    @discardableResult
    func selectAudioInput(uid: String) -> Bool {
        let session = AVAudioSession.sharedInstance()
        guard let port = session.availableInputs?.first(where: { $0.uid == uid }) else {
            nativeLog("[NativeBridge] selectAudioInput: uid '\(uid)' not found in availableInputs")
            return false
        }
        do {
            // Switch to A2DP before selecting a USB input — USB is now in availableInputs
            // so applySessionCategory() will see it and pick .allowBluetoothA2DP, preventing
            // HFP input/output coupling from resetting Bluetooth output to Speaker.
            if port.portType == .usbAudio {
                try? applySessionCategory()
            }
            try setPreferredInputPreservingOutput(port)
            nativeLog("[NativeBridge] selectAudioInput: set preferred input to '\(port.portName)' (\(port.portType.rawValue))")
            UserDefaults.standard.set(uid, forKey: "NativeBridge.preferredInputUid")
            UserDefaults.standard.set(port.portType.rawValue, forKey: "NativeBridge.preferredInputType")

            if isMicActive || isCaptureMicActive {
                nativeLog("[NativeBridge] selectAudioInput: mic active (engine=\(isMicActive) capture=\(isCaptureMicActive)) — stopping, waiting for route change before reinstall")
                stopMic()
                // Wait for iOS to commit the route change (fires .override reason) before
                // reinstalling the tap. Without this wait, inputNode.outputFormat may return
                // 0Hz/0ch immediately after setPreferredInput (Apple dev forum #737904).
                var observer: NSObjectProtocol?
                let timeoutItem = DispatchWorkItem { [weak self] in
                    guard let self = self else { return }
                    if let obs = observer { NotificationCenter.default.removeObserver(obs) }
                    self.nativeLog("[NativeBridge] selectAudioInput: route change timeout — attempting startMic anyway")
                    self.startMicWithErrorReporting()
                }
                observer = NotificationCenter.default.addObserver(
                    forName: AVAudioSession.routeChangeNotification, object: nil, queue: .main
                ) { [weak self] _ in
                    guard let self = self else { return }
                    timeoutItem.cancel()
                    if let obs = observer { NotificationCenter.default.removeObserver(obs) }
                    self.nativeLog("[NativeBridge] selectAudioInput: route change confirmed — reinstalling tap")
                    self.startMicWithErrorReporting()
                }
                DispatchQueue.main.asyncAfter(deadline: .now() + 2.0, execute: timeoutItem)
            }
            return true
        } catch {
            nativeLog("[NativeBridge] selectAudioInput: setPreferredInput failed: \(error)")
            return false
        }
    }

    // MARK: - USBAudio Capture (AVCaptureSession path)

    /// True when the currently active input is a USBAudio device (e.g. Hollyland Lark A1).
    private func isUSBAudioActive() -> Bool {
        return AVAudioSession.sharedInstance().currentRoute.inputs.first?.portType == .usbAudio
    }

    /// True when a USBAudio device is present — checks both currentRoute (fast path) and
    /// availableInputs (catches devices that VPIO de-enumerated from currentRoute but are
    /// still physically connected). Used to decide whether to use AVAudioPlayer for TTS
    /// and whether DemoRecorder should skip the ensureEngine + VPIO dance.
    func isUSBAudioConnected() -> Bool {
        let session = AVAudioSession.sharedInstance()
        if session.currentRoute.inputs.first?.portType == .usbAudio { return true }
        if session.availableInputs?.contains(where: { $0.portType == .usbAudio }) == true { return true }
        // Fallback: persisted type — covers case where VPIO already ran and de-enumerated
        // the device from both currentRoute and availableInputs (process-scoped).
        return UserDefaults.standard.string(forKey: "NativeBridge.preferredInputType") == AVAudioSession.Port.usbAudio.rawValue
    }

    /// Set `.playAndRecord` session category with the appropriate Bluetooth option.
    /// When USB audio is connected: use `.allowBluetoothA2DP` (output only, no input coupling).
    ///   A2DP has no input channel, so `setPreferredInput(Lark)` cannot cause HFP coupling
    ///   that would reset the Bluetooth output to Speaker.
    /// When no USB: use `.allowBluetoothHFP` (input+output) so AirPods can serve as mic+speaker.
    /// Re-called on USB connect/disconnect (via routeChangeNotification) so the option tracks
    /// live hardware state.
    private func applySessionCategory() throws {
        let session = AVAudioSession.sharedInstance()
        let usbConnected = session.availableInputs?.contains(where: { $0.portType == .usbAudio }) ?? false

        // Guard against infinite loop: setCategory triggers a categoryChange route notification,
        // which calls applySessionCategory again. If the USB state hasn't changed, skip the call.
        guard usbConnected != lastAppliedUSBState else { return }
        lastAppliedUSBState = usbConnected

        #if compiler(>=6.2)
        try session.setCategory(.playAndRecord, options: [
            .mixWithOthers,
            .defaultToSpeaker,
            usbConnected ? .allowBluetoothA2DP : .allowBluetoothHFP,
        ])
        #else
        try session.setCategory(.playAndRecord, options: [
            .mixWithOthers,
            .defaultToSpeaker,
            usbConnected ? .allowBluetoothA2DP : .allowBluetooth,
        ])
        #endif
        nativeLog("[NativeBridge] applySessionCategory: usbConnected=\(usbConnected) bt=\(usbConnected ? "A2DP" : "HFP")")
    }

    /// Set the preferred input while preserving the current output route.
    /// `setPreferredInput` for a USB device causes iOS to recalculate the full route.
    /// With `.playAndRecord + .defaultToSpeaker`, iOS picks speaker over Bluetooth when
    /// the input changes. After the call, if the output was Bluetooth and got reset to
    /// speaker, remove the speaker override so iOS re-routes to the Bluetooth device.
    @discardableResult
    func setPreferredInputPreservingOutput(_ port: AVAudioSessionPortDescription) throws -> Bool {
        let session = AVAudioSession.sharedInstance()
        let outputBefore = session.currentRoute.outputs.first
        try session.setPreferredInput(port)
        // Check if a Bluetooth output was knocked off by the input change.
        let btTypes: Set<AVAudioSession.Port> = [.bluetoothA2DP, .bluetoothHFP, .bluetoothLE]
        if let prev = outputBefore, btTypes.contains(prev.portType),
           session.currentRoute.outputs.first?.portType == .builtInSpeaker {
            try? session.overrideOutputAudioPort(.none)
            nativeLog("[NativeBridge] setPreferredInput: Bluetooth output was reset to speaker — removed override, iOS will re-route")
            return true  // output was restored
        }
        return false  // no correction needed
    }

    /// Start mic capture via AVCaptureSession for USBAudio inputs.
    /// AVCaptureSession can capture from any input without VPIO — bypasses the
    /// audio unit constraint that de-enumerates USBAudio from availableInputs.
    private func startMicViaCapture() {
        guard !isCaptureMicActive else {
            nativeLog("[NativeBridge] startMicViaCapture: already active, skipping")
            return
        }

        let session = AVAudioSession.sharedInstance()
        guard let activePort = session.currentRoute.inputs.first else {
            nativeLog("[NativeBridge] startMicViaCapture: no active input — aborting")
            return
        }

        guard let device = AVCaptureDevice.default(for: .audio) else {
            nativeLog("[NativeBridge] startMicViaCapture: no AVCaptureDevice for audio — aborting")
            return
        }

        let capture = AVCaptureSession()
        // Prevent AVCaptureSession from auto-reconfiguring the audio session — doing so
        // would break AVAudioEngine's playerNode output (Apple Forums #61594, #764894).
        capture.automaticallyConfiguresApplicationAudioSession = false
        do {
            let input = try AVCaptureDeviceInput(device: device)
            guard capture.canAddInput(input) else {
                nativeLog("[NativeBridge] startMicViaCapture: canAddInput failed")
                return
            }
            capture.addInput(input)
        } catch {
            nativeLog("[NativeBridge] startMicViaCapture: input creation failed: \(error)")
            return
        }

        let output = AVCaptureAudioDataOutput()
        guard capture.canAddOutput(output) else {
            nativeLog("[NativeBridge] startMicViaCapture: canAddOutput failed")
            return
        }
        capture.addOutput(output)

        guard let targetFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                               sampleRate: sampleRate,
                                               channels: 1,
                                               interleaved: false) else {
            nativeLog("[NativeBridge] startMicViaCapture: targetFormat creation failed")
            return
        }

        output.setSampleBufferDelegate(self, queue: DispatchQueue(label: "com.babel3.capture.audio"))
        captureOutput = output
        captureTargetFormat = targetFormat
        captureConverter = nil  // built lazily on first sample buffer (format known then)

        // startRunning() is blocking — dispatch to background to avoid stalling the main thread.
        // Set isCaptureMicActive and store session before dispatching so any concurrent stopMic()
        // call sees consistent state. _micStarted() fires after startRunning() confirms capture is live.
        captureSession = capture
        isCaptureMicActive = true
        nativeLog("[NativeBridge] startMicViaCapture: starting for '\(activePort.portName)' (\(activePort.portType.rawValue))")

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self = self else { return }
            capture.startRunning()
            self.nativeLog("[NativeBridge] startMicViaCapture: capture running (isRunning=\(capture.isRunning))")
            DispatchQueue.main.async {
                self.webView?.evaluateJavaScript("window.NativeBridge._micStarted()")
            }
        }
    }

    /// Stop AVCaptureSession mic capture.
    private func stopMicCapture() {
        captureSession?.stopRunning()
        captureSession = nil
        captureOutput = nil
        captureConverter = nil
        captureTargetFormat = nil
        isCaptureMicActive = false
        audioChunkCount = 0
        nativeLog("[NativeBridge] stopMicCapture: stopped")
        DispatchQueue.main.async {
            self.webView?.evaluateJavaScript("window.NativeBridge._micStopped()")
        }
    }

    /// startMic() variant that notifies the web app on abort (silent failure prevention).
    /// The existing startMic() aborts with a log entry only when inputFormat is invalid.
    private func startMicWithErrorReporting() {
        let inputFormat = audioEngine?.inputNode.outputFormat(forBus: 0)
        if let fmt = inputFormat, fmt.sampleRate <= 0 || fmt.channelCount == 0 {
            nativeLog("[NativeBridge] selectAudioInput: inputFormat still invalid after route change (\(fmt)) — mic restart aborted")
            DispatchQueue.main.async { [weak self] in
                self?.webView?.evaluateJavaScript("if(window.NativeBridge&&window.NativeBridge._onMicError)window.NativeBridge._onMicError('input_format_invalid')")
            }
            return
        }
        startMic()
    }

    /// Returns the current active route as a JSON-serializable dict.
    func getCurrentRoute() -> [String: Any] {
        let route = AVAudioSession.sharedInstance().currentRoute
        return [
            "inputs": route.inputs.map { [
                "portName": $0.portName,
                "portType": $0.portType.rawValue,
                "uid": $0.uid,
            ]},
            "outputs": route.outputs.map { [
                "portName": $0.portName,
                "portType": $0.portType.rawValue,
                "uid": $0.uid,
            ]},
        ]
    }

    /// Overrides output to built-in speaker.
    func selectBuiltInSpeaker() {
        try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)
        nativeLog("[NativeBridge] selectBuiltInSpeaker: output overridden to speaker")
    }

    /// Clears output override — routes to built-in receiver (earpiece).
    func selectBuiltInReceiver() {
        try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.none)
        nativeLog("[NativeBridge] selectBuiltInReceiver: output routed to receiver/earpiece")
    }

    /// Applies the persisted preferred input if the device is still available.
    /// Call after configureAudioSession() settles.
    func applyPersistedInput() {
        guard let uid = UserDefaults.standard.string(forKey: "NativeBridge.preferredInputUid") else { return }
        let session = AVAudioSession.sharedInstance()
        if let port = session.availableInputs?.first(where: { $0.uid == uid }) {
            // D3: Capture setPreferredInput result — tells us if iOS accepts but ignores the call
            // (success but activeInput unchanged) vs rejects it outright (throws).
            do {
                try setPreferredInputPreservingOutput(port)
                let activeInput = session.currentRoute.inputs.first?.portName ?? "none"
                nativeLog("[NativeBridge] applyPersistedInput: reapplied '\(port.portName)' — activeInput now: \(activeInput)")
            } catch {
                nativeLog("[NativeBridge] applyPersistedInput: setPreferredInput FAILED for '\(port.portName)': \(error)")
            }
        } else {
            nativeLog("[NativeBridge] applyPersistedInput: uid '\(uid)' no longer available, using default")
            // Notify web app so the picker UI can show "preferred device not connected"
            let escapedUid = uid.replacingOccurrences(of: "'", with: "\\'")
            DispatchQueue.main.async { [weak self] in
                self?.webView?.evaluateJavaScript(
                    "if(window.NativeBridge&&window.NativeBridge._onPreferredInputUnavailable)window.NativeBridge._onPreferredInputUnavailable('\(escapedUid)')"
                )
            }
        }
    }

    /// Presents the system AVRoutePickerView so the user can select Bluetooth/AirPlay output.
    /// The picker is kept as a property so the sheet stays alive after trigger.
    /// Uses the community-documented sendActions pattern (confirmed iOS 18).
    /// Note: Apple does not provide an official programmatic trigger API for AVRoutePickerView.
    func showOutputPicker() {
        DispatchQueue.main.async {
            // Reuse existing picker if available, otherwise create and attach to key window.
            // Use UIWindowScene.keyWindow (iOS 15+) instead of deprecated windows/isKeyWindow.
            if self.routePicker == nil {
                let picker = AVRoutePickerView(frame: CGRect(x: -100, y: -100, width: 1, height: 1))
                if let window = UIApplication.shared.connectedScenes
                    .compactMap({ $0 as? UIWindowScene })
                    .compactMap({ $0.keyWindow })
                    .first {
                    window.addSubview(picker)
                }
                self.routePicker = picker
            }
            guard let picker = self.routePicker else { return }
            // Trigger the system route picker sheet via the internal UIButton
            if let button = picker.subviews.first(where: { $0 is UIButton }) as? UIButton {
                button.sendActions(for: .touchUpInside)
            }
        }
    }

    /// Push current device list to web app. Called by route change observer.
    /// Debounced 500ms — during startMic(), engine stop+restart fires multiple route change
    /// notifications while iOS temporarily collapses availableInputs (external mic drops
    /// out mid-restart). Without debounce, the panel refreshes with a stale one-item list.
    /// The 500ms window covers the full engine restart sequence; the final notification
    /// reflects settled hardware state.
    ///
    /// Thread-safety: routeChangeNotification fires on the audio thread (queue: nil).
    /// All deviceUpdateWorkItem reads/writes are pinned to main via outer async dispatch —
    /// DispatchWorkItem is not thread-safe for concurrent cancel+execute across queues.
    func pushDeviceUpdateToWebApp() {
        DispatchQueue.main.async { [weak self] in
            guard let self = self else { return }
            self.deviceUpdateWorkItem?.cancel()
            let work = DispatchWorkItem { [weak self] in
                guard let self = self else { return }
                let payload = self.listAudioDevices()
                guard let json = try? JSONSerialization.data(withJSONObject: payload),
                      let jsonStr = String(data: json, encoding: .utf8) else { return }
                self.webView?.evaluateJavaScript(
                    "if(window.NativeBridge&&window.NativeBridge._onDevicesChanged)window.NativeBridge._onDevicesChanged(\(jsonStr))"
                )
            }
            self.deviceUpdateWorkItem = work
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5, execute: work)
        }
    }

    /// Play WAV audio — routes to AVAudioPlayer when USBAudio connected, AVAudioEngine otherwise.
    func playAudio(base64: String) {
        playCount += 1
        let playId = playCount
        let sinceCreate = engineCreatedAt.map { String(format: "%.0f", Date().timeIntervalSince($0) * 1000) } ?? "n/a"
        nativeLog("[NativeBridge] playAudio #\(playId) called (dataLen=\(base64.count) engineRunning=\(audioEngine?.isRunning ?? false) playerPlaying=\(playerNode?.isPlaying ?? false) micActive=\(isMicActive) engineAge=\(sinceCreate)ms isEngineReady=\(isEngineReady))")

        guard let data = Data(base64Encoded: base64) else {
            nativeLog("[NativeBridge] playAudio: base64 decode failed")
            return
        }

        // USB path: avoid AVAudioEngine entirely when a USBAudio device is connected.
        // AVAudioEngine requires VPIO, and VPIO permanently de-enumerates USBAudio from
        // availableInputs (process-scoped — survives engine teardown and session cycling).
        // AVAudioPlayer plays WAV through the session output route without touching VPIO,
        // keeping the Lark enumerated for the next mic press.
        if isUSBAudioConnected() {
            playAudioViaAVPlayer(data: data, playId: playId)
            return
        }

        // Built-in mic path: use AVAudioEngine + VPIO (hardware echo cancellation).
        // Ensure engine + player exist (creates them if first TTS before mic start).
        // This may create a new engine which won't be ready until config change settles.
        guard ensureEngine(), let engine = audioEngine, let player = playerNode else {
            nativeLog("[NativeBridge] playAudio: engine creation failed")
            return
        }

        // If engine was just created and VP config change hasn't settled yet,
        // queue the audio and return. The config change handler will drain the queue
        // after rebuilding the engine into a stable state.
        if !isEngineReady {
            pendingAudio.append(base64)
            nativeLog("[DIAG] playAudio #\(playId) queued (isEngineReady=false, pendingCount=\(pendingAudio.count))")
            return
        }

        // Only override speaker route — don't reconfigure the session
        // (reconfiguring kills the active mic tap)
        try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)

        // Ensure volumes are up
        engine.mainMixerNode.outputVolume = 1.0
        player.volume = 1.0

        // Diagnostic
        let route = AVAudioSession.sharedInstance().currentRoute.outputs.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",")
        nativeLog("[NativeBridge] playAudio: route=\(route) mixerVol=\(engine.mainMixerNode.outputVolume) playerVol=\(player.volume)")

        guard data.count > 44 else {
            nativeLog("[NativeBridge] playAudio: data too short (\(data.count) bytes)")
            return
        }

        // Read WAV header
        let wavChannels: UInt32
        let wavSampleRate: Double
        let bitsPerSample: UInt16
        do {
            let ch = data.withUnsafeBytes { $0.load(fromByteOffset: 22, as: UInt16.self) }
            let sr = data.withUnsafeBytes { $0.load(fromByteOffset: 24, as: UInt32.self) }
            let bps = data.withUnsafeBytes { $0.load(fromByteOffset: 34, as: UInt16.self) }
            wavChannels = UInt32(ch)
            wavSampleRate = Double(sr)
            bitsPerSample = bps
        }

        let pcmData = data.subdata(in: 44..<data.count)
        let bytesPerSample = Int(bitsPerSample) / 8
        guard bytesPerSample > 0, wavChannels > 0 else {
            nativeLog("[NativeBridge] playAudio: bad WAV header bps=\(bitsPerSample) ch=\(wavChannels)")
            return
        }
        let frameCount = pcmData.count / (bytesPerSample * Int(wavChannels))

        guard let wavFormat = AVAudioFormat(commonFormat: bitsPerSample == 32 ? .pcmFormatFloat32 : .pcmFormatInt16,
                                             sampleRate: wavSampleRate,
                                             channels: AVAudioChannelCount(wavChannels),
                                             interleaved: wavChannels > 1) else {
            nativeLog("[NativeBridge] playAudio: bad format sr=\(wavSampleRate) ch=\(wavChannels) bits=\(bitsPerSample)")
            return
        }

        guard let pcmBuffer = AVAudioPCMBuffer(pcmFormat: wavFormat,
                                                 frameCapacity: AVAudioFrameCount(frameCount)) else {
            nativeLog("[NativeBridge] playAudio: buffer creation failed")
            return
        }

        pcmBuffer.frameLength = AVAudioFrameCount(frameCount)
        pcmData.withUnsafeBytes { src in
            if bitsPerSample == 16, let dst = pcmBuffer.int16ChannelData {
                memcpy(dst[0], src.baseAddress!, pcmData.count)
            } else if bitsPerSample == 32, let dst = pcmBuffer.floatChannelData {
                memcpy(dst[0], src.baseAddress!, pcmData.count)
            }
        }

        // Always convert to the engine's output format. Even if sample rate and channel
        // count appear to match the WAV, the mixer↔outputNode format may differ (e.g.
        // mixer=48kHz/1ch but outputNode=48kHz/2ch during non-VP recording mode).
        // Skipping conversion in that case causes chipmunk playback and crashes.
        let outputFormat = engine.mainMixerNode.outputFormat(forBus: 0)
        if true {  // Always convert — format mismatch between mixer and outputNode is invisible here
            guard let converter = AVAudioConverter(from: wavFormat, to: outputFormat) else {
                nativeLog("[NativeBridge] playAudio: converter creation failed")
                return
            }
            let ratio = outputFormat.sampleRate / wavFormat.sampleRate
            let outFrameCount = AVAudioFrameCount(Double(frameCount) * ratio) + 1
            guard let outBuffer = AVAudioPCMBuffer(pcmFormat: outputFormat, frameCapacity: outFrameCount) else { return }

            var convError: NSError?
            converter.convert(to: outBuffer, error: &convError) { _, outStatus in
                outStatus.pointee = .haveData
                return pcmBuffer
            }
            guard convError == nil else {
                nativeLog("[NativeBridge] playAudio: conversion error \(convError!)")
                return
            }

            // Full diagnostic before play
            let session = AVAudioSession.sharedInstance()
            let route = session.currentRoute.outputs.map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",")
            let cat = session.category.rawValue
            let mode = session.mode.rawValue
            nativeLog("[NativeBridge] playAudio: \(frameCount)→\(outBuffer.frameLength) frames \(wavSampleRate)→\(outputFormat.sampleRate)Hz route=\(route) cat=\(cat) mode=\(mode) playing=\(player.isPlaying) running=\(engine.isRunning) vp=\(engine.inputNode.isVoiceProcessingEnabled) mixVol=\(engine.mainMixerNode.outputVolume) playerVol=\(player.volume)")

            // Ensure speaker output and player is active.
            // No engine restart here — isEngineReady gate ensures engine is stable.
            // If engine stops after being marked ready, config change handler will rebuild.
            try? session.overrideOutputAudioPort(.speaker)
            engine.mainMixerNode.outputVolume = 1.0
            player.volume = 1.0
            if !player.isPlaying { player.play() }

            // D4: Log player state at schedule time
            logEngineState("pre-schedule-converted-#\(playId)")
            let scheduleTime = Date()
            player.scheduleBuffer(outBuffer) { [weak self] in
                let elapsed = Date().timeIntervalSince(scheduleTime)
                self?.nativeLog("[NativeBridge] playAudio #\(playId) buffer completed (converted) elapsed=\(String(format: "%.1f", elapsed))s")
                self?.logEngineState("post-complete-converted-#\(playId)")
                DispatchQueue.main.async {
                    self?.webView?.evaluateJavaScript("window.NativeBridge._audioPlaybackDone()")
                }
            }
        } else {
            nativeLog("[NativeBridge] playAudio #\(playId): \(frameCount) frames at \(wavSampleRate)Hz (direct) playing=\(player.isPlaying) engineRunning=\(engine.isRunning)")

            try? AVAudioSession.sharedInstance().overrideOutputAudioPort(.speaker)
            if !player.isPlaying { player.play() }

            // D4: Log player state at schedule time
            logEngineState("pre-schedule-direct-#\(playId)")
            let scheduleTime = Date()
            player.scheduleBuffer(pcmBuffer) { [weak self] in
                let elapsed = Date().timeIntervalSince(scheduleTime)
                self?.nativeLog("[NativeBridge] playAudio #\(playId) buffer completed (direct) elapsed=\(String(format: "%.1f", elapsed))s")
                self?.logEngineState("post-complete-direct-#\(playId)")
                DispatchQueue.main.async {
                    self?.webView?.evaluateJavaScript("window.NativeBridge._audioPlaybackDone()")
                }
            }
        }
    }

    /// TTS playback via AVAudioPlayer — used when a USBAudio device is connected.
    /// Writes the WAV data to a temp file and plays through AVAudioPlayer, which uses the
    /// audio session's output route without requiring AVAudioEngine or VPIO. This keeps
    /// the Lark A1 enumerated for the next mic press.
    private func playAudioViaAVPlayer(data: Data, playId: Int) {
        // If capture is active, pause it during TTS to prevent TTS audio feeding back into
        // transcription. Use a shared flag so multiple concurrent chunks don't double-stop
        // or double-restart: only the first chunk pauses, only the last chunk restarts.
        if isCaptureMicActive && !captureWasPausedForTTS {
            captureWasPausedForTTS = true
            stopMicCapture()
            nativeLog("[NativeBridge] playAudioViaAVPlayer #\(playId): paused capture for TTS")
        }

        // Mute RPScreenRecorder's mic channel during TTS. TTS audio enters the recording
        // via the direct app audio channel (higher quality). Without this, TTS also plays
        // through the speaker and is picked up acoustically by the Lark, recording it twice.
        if RPScreenRecorder.shared().isRecording && !recMicWasMutedForTTS {
            recMicWasMutedForTTS = true
            RPScreenRecorder.shared().isMicrophoneEnabled = false
            nativeLog("[NativeBridge] playAudioViaAVPlayer #\(playId): muted rec mic for TTS")
        }

        let tmpURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("b3_tts_\(playId).wav")
        do {
            try data.write(to: tmpURL)
        } catch {
            nativeLog("[NativeBridge] playAudioViaAVPlayer #\(playId): write failed \(error)")
            DispatchQueue.main.async { [weak self] in
                self?.webView?.evaluateJavaScript("window.NativeBridge._audioPlaybackDone()")
            }
            return
        }

        // No speaker override here — let the audio session route naturally.
        // The session category uses .defaultToSpeaker which handles the default case.
        // Forcing .speaker would override headphones/earphones connected by the user.

        do {
            let player = try AVAudioPlayer(contentsOf: tmpURL)
            player.volume = 1.0
            player.prepareToPlay()
            audioPlayers.append(player)  // retain — multiple chunks may overlap

            let route = AVAudioSession.sharedInstance().currentRoute.outputs
                .map { "\($0.portName)(\($0.portType.rawValue))" }.joined(separator: ",")
            nativeLog("[NativeBridge] playAudioViaAVPlayer #\(playId): \(data.count) bytes route=\(route)")

            player.play()

            // Poll for completion on a background thread — AVAudioPlayer has no async callback.
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                // Wait up to 30s for playback to finish (longest expected TTS chunk)
                let deadline = Date().addingTimeInterval(30)
                while player.isPlaying && Date() < deadline {
                    Thread.sleep(forTimeInterval: 0.05)
                }
                try? FileManager.default.removeItem(at: tmpURL)
                self?.nativeLog("[NativeBridge] playAudioViaAVPlayer #\(playId): done (duration=\(String(format: "%.2f", player.duration))s)")
                DispatchQueue.main.async { [weak self] in
                    guard let self = self else { return }
                    self.audioPlayers.removeAll { $0 === player }
                    // After the last chunk: re-assert preferred input and restart capture.
                    // Multiple concurrent chunks may be in-flight; only fire when the array
                    // drains so we don't interrupt each other's input context.
                    if self.audioPlayers.isEmpty {
                        if let uid = UserDefaults.standard.string(forKey: "NativeBridge.preferredInputUid"),
                           let port = AVAudioSession.sharedInstance().availableInputs?.first(where: { $0.uid == uid }) {
                            try? self.setPreferredInputPreservingOutput(port)
                            self.nativeLog("[NativeBridge] playAudioViaAVPlayer: re-asserted preferred input '\(port.portName)'")
                        }
                        // Restart capture if it was paused for TTS — mic resumes on the same device.
                        if self.captureWasPausedForTTS {
                            self.captureWasPausedForTTS = false
                            self.startMicViaCapture()
                            self.nativeLog("[NativeBridge] playAudioViaAVPlayer: restarted capture after TTS")
                        }
                        // Re-enable RPScreenRecorder mic after all TTS chunks finish.
                        if self.recMicWasMutedForTTS {
                            self.recMicWasMutedForTTS = false
                            RPScreenRecorder.shared().isMicrophoneEnabled = true
                            self.nativeLog("[NativeBridge] playAudioViaAVPlayer: unmuted rec mic after TTS")
                        }
                    }
                    self.webView?.evaluateJavaScript("window.NativeBridge._audioPlaybackDone()")
                }
            }
        } catch {
            nativeLog("[NativeBridge] playAudioViaAVPlayer #\(playId): AVAudioPlayer init failed \(error)")
            try? FileManager.default.removeItem(at: tmpURL)
            DispatchQueue.main.async { [weak self] in
                self?.webView?.evaluateJavaScript("window.NativeBridge._audioPlaybackDone()")
            }
        }
    }

    func handleMessage(_ action: String, body: [String: Any]) {
        nativeLog("[NativeBridge] handleMessage: \(action)")
        switch action {
        case "startMic":
            startMic()
        case "stopMic":
            stopMic()
        case "playAudio":
            if let b64 = body["audio_b64"] as? String {
                playAudio(base64: b64)
            }
        case "log":
            if let msg = body["message"] as? String {
                print("[WebView] \(msg)")
            }
        case "listAudioDevices":
            let devices = listAudioDevices()
            if let json = try? JSONSerialization.data(withJSONObject: devices),
               let jsonStr = String(data: json, encoding: .utf8) {
                DispatchQueue.main.async { [weak self] in
                    self?.webView?.evaluateJavaScript("window.NativeBridge._onDeviceList(\(jsonStr))")
                }
            }
        case "selectAudioInput":
            if let uid = body["uid"] as? String {
                let success = selectAudioInput(uid: uid)
                DispatchQueue.main.async { [weak self] in
                    let successStr = success ? "true" : "false"
                    let escapedUid = uid.replacingOccurrences(of: "'", with: "\\'")
                    self?.webView?.evaluateJavaScript("window.NativeBridge._onInputSelected('\(escapedUid)',\(successStr))")
                }
            }
        case "selectBuiltInSpeaker":
            selectBuiltInSpeaker()
        case "selectBuiltInReceiver":
            selectBuiltInReceiver()
        case "showOutputPicker":
            showOutputPicker()
        case "getCurrentRoute":
            let route = getCurrentRoute()
            if let json = try? JSONSerialization.data(withJSONObject: route),
               let jsonStr = String(data: json, encoding: .utf8) {
                DispatchQueue.main.async { [weak self] in
                    self?.webView?.evaluateJavaScript("window.NativeBridge._onCurrentRoute(\(jsonStr))")
                }
            }
        default:
            nativeLog("[NativeBridge] Unknown: \(action)")
        }
    }

    static let bridgeJS = """
    (function() {
        window.NativeBridge = {
            isAvailable: true,
            version: '\(NativeBridge.version)',
            capabilities: ['sharedMic', 'screenRecord', 'camera', 'nativeAudio', 'audioDeviceSelection'],
            _micActive: false,
            _audioCallbacks: [],
            _micStartCallbacks: [],
            _micStopCallbacks: [],
            _playbackDoneCallbacks: [],

            requestMicStream: function() {
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'startMic'});
            },
            stopMicStream: function() {
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'stopMic'});
            },
            playAudio: function(audio_b64, onDone) {
                if (onDone) this._playbackDoneCallbacks.push(onDone);
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'playAudio', audio_b64: audio_b64});
            },
            onAudioData: function(cb) { this._audioCallbacks.push(cb); },
            onMicStarted: function(cb) { this._micStartCallbacks.push(cb); },
            onMicStopped: function(cb) { this._micStopCallbacks.push(cb); },

            _receiveAudio: function(b64) {
                var bin = atob(b64);
                var bytes = new Uint8Array(bin.length);
                for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
                var f32 = new Float32Array(bytes.buffer);
                for (var j = 0; j < this._audioCallbacks.length; j++) this._audioCallbacks[j](f32);
            },
            _micStarted: function() {
                this._micActive = true;
                for (var i = 0; i < this._micStartCallbacks.length; i++) this._micStartCallbacks[i]();
            },
            _micStopped: function() {
                this._micActive = false;
                for (var i = 0; i < this._micStopCallbacks.length; i++) this._micStopCallbacks[i]();
            },
            _audioPlaybackDone: function() {
                var cbs = this._playbackDoneCallbacks;
                this._playbackDoneCallbacks = [];
                for (var i = 0; i < cbs.length; i++) cbs[i]();
            },
            log: function(msg) {
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'log', message: msg});
            },

            // Audio device selection
            listAudioDevices: function(callback) {
                if (callback) this._deviceListCallbacks.push(callback);
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'listAudioDevices'});
            },
            // callback(uid, success): success=true means iOS accepted the preference,
            // NOT that the mic is already routing through the new device. The route
            // change is async — mic restarts after routeChangeNotification confirms.
            selectAudioInput: function(uid, callback) {
                if (callback) this._inputSelectedCallbacks.push(callback);
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'selectAudioInput', uid: uid});
            },
            selectBuiltInSpeaker: function() {
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'selectBuiltInSpeaker'});
            },
            selectBuiltInReceiver: function() {
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'selectBuiltInReceiver'});
            },
            showOutputPicker: function() {
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'showOutputPicker'});
            },
            getCurrentRoute: function(callback) {
                if (callback) this._currentRouteCallbacks.push(callback);
                window.webkit.messageHandlers.nativeBridge.postMessage({action: 'getCurrentRoute'});
            },
            onDevicesChanged: function(callback) {
                this._devicesChangedCallbacks.push(callback);
            },

            _deviceListCallbacks: [],
            _inputSelectedCallbacks: [],
            _currentRouteCallbacks: [],
            _devicesChangedCallbacks: [],

            _onDeviceList: function(devices) {
                var cbs = this._deviceListCallbacks;
                this._deviceListCallbacks = [];
                for (var i = 0; i < cbs.length; i++) cbs[i](devices);
            },
            _onInputSelected: function(uid, success) {
                var cbs = this._inputSelectedCallbacks;
                this._inputSelectedCallbacks = [];
                for (var i = 0; i < cbs.length; i++) cbs[i](uid, success);
            },
            _onCurrentRoute: function(route) {
                var cbs = this._currentRouteCallbacks;
                this._currentRouteCallbacks = [];
                for (var i = 0; i < cbs.length; i++) cbs[i](route);
            },
            _onDevicesChanged: function(devices) {
                for (var i = 0; i < this._devicesChangedCallbacks.length; i++) this._devicesChangedCallbacks[i](devices);
            },
            onPreferredInputUnavailable: function(callback) {
                this._preferredInputUnavailableCallbacks.push(callback);
            },
            onMicError: function(callback) {
                this._micErrorCallbacks.push(callback);
            },
            _preferredInputUnavailableCallbacks: [],
            _micErrorCallbacks: [],
            _onPreferredInputUnavailable: function(uid) {
                for (var i = 0; i < this._preferredInputUnavailableCallbacks.length; i++) this._preferredInputUnavailableCallbacks[i](uid);
            },
            _onMicError: function(reason) {
                for (var i = 0; i < this._micErrorCallbacks.length; i++) this._micErrorCallbacks[i](reason);
            }
        };
        console.log('[NativeBridge] v' + window.NativeBridge.version + ' Available (nativeAudio)');
    })();
    """
}

// MARK: - AVCaptureAudioDataOutputSampleBufferDelegate

extension NativeBridge: AVCaptureAudioDataOutputSampleBufferDelegate {
    /// Receives audio sample buffers from AVCaptureSession (USBAudio path).
    /// Converts CMSampleBuffer → Float32 PCM at 16kHz mono, sends as base64 to web app.
    func captureOutput(_ output: AVCaptureOutput,
                       didOutput sampleBuffer: CMSampleBuffer,
                       from connection: AVCaptureConnection) {
        guard isCaptureMicActive,
              let targetFormat = captureTargetFormat else { return }

        // Build converter lazily on first buffer — format is known only after capture starts.
        if captureConverter == nil {
            guard let desc = CMSampleBufferGetFormatDescription(sampleBuffer) else {
                nativeLog("[NativeBridge] captureOutput: could not derive source format")
                return
            }
            let srcFormat = AVAudioFormat(cmAudioFormatDescription: desc)
            guard let converter = AVAudioConverter(from: srcFormat, to: targetFormat) else {
                nativeLog("[NativeBridge] captureOutput: converter creation failed (src: \(srcFormat))")
                return
            }
            captureConverter = converter
            nativeLog("[NativeBridge] captureOutput: converter created \(srcFormat.sampleRate)/\(srcFormat.channelCount)ch → \(targetFormat.sampleRate)/1ch")
        }

        guard let converter = captureConverter else { return }

        // Wrap CMSampleBuffer in AVAudioPCMBuffer for conversion.
        var blockBuffer: CMBlockBuffer?
        var audioBufferList = AudioBufferList()
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sampleBuffer, bufferListSizeNeededOut: nil,
            bufferListOut: &audioBufferList, bufferListSize: MemoryLayout<AudioBufferList>.size,
            blockBufferAllocator: nil, blockBufferMemoryAllocator: nil,
            flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            blockBufferOut: &blockBuffer
        )

        let frameCount = AVAudioFrameCount(CMSampleBufferGetNumSamples(sampleBuffer))
        guard frameCount > 0,
              let srcFormat = converter.inputFormat as AVAudioFormat?,
              let srcBuffer = AVAudioPCMBuffer(pcmFormat: srcFormat, frameCapacity: frameCount) else { return }
        srcBuffer.frameLength = frameCount

        // Copy audio data from AudioBufferList into AVAudioPCMBuffer, buffer by buffer.
        let srcAbl = UnsafeMutableAudioBufferListPointer(&audioBufferList)
        let dstAbl = UnsafeMutableAudioBufferListPointer(srcBuffer.mutableAudioBufferList)
        for (i, srcBuf) in srcAbl.enumerated() {
            guard i < dstAbl.count else { break }
            if let srcData = srcBuf.mData, let dstData = dstAbl[i].mData {
                memcpy(dstData, srcData, Int(srcBuf.mDataByteSize))
            }
        }

        let outFrameCount = AVAudioFrameCount(Double(frameCount) * targetFormat.sampleRate / srcFormat.sampleRate)
        guard let outBuffer = AVAudioPCMBuffer(pcmFormat: targetFormat, frameCapacity: outFrameCount) else { return }

        var convertError: NSError?
        let status = converter.convert(to: outBuffer, error: &convertError) { _, outStatus in
            outStatus.pointee = .haveData
            return srcBuffer
        }
        guard status != .error, convertError == nil else { return }

        guard let channelData = outBuffer.floatChannelData?[0] else { return }
        let data = Data(bytes: channelData, count: Int(outBuffer.frameLength) * MemoryLayout<Float>.size)
        let base64 = data.base64EncodedString()

        audioChunkCount += 1
        DispatchQueue.main.async { [weak self] in
            guard let self = self else { return }
            self.webView?.evaluateJavaScript("window.NativeBridge._receiveAudio('\(base64)')")
            if self.audioChunkCount % 100 == 0 {
                self.nativeLog("[NativeBridge] capture chunk #\(self.audioChunkCount)")
            }
        }
    }
}
