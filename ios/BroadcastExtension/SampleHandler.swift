import ReplayKit
import AVFoundation

/// Broadcast Upload Extension — records screen system-wide with audio.
/// Runs as a separate process from the main app. Receives video frames,
/// system audio, and mic audio from ReplayKit. Writes to MP4 in the
/// shared app group container. Signals the main app via a file marker.
class SampleHandler: RPBroadcastSampleHandler {

    private var assetWriter: AVAssetWriter?
    private var videoInput: AVAssetWriterInput?
    private var audioInput: AVAssetWriterInput?
    private var isSessionStarted = false

    private let appGroupID = "group.com.babel3.ios"

    override func broadcastStarted(withSetupInfo setupInfo: [String: NSObject]?) {
        guard let containerURL = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: appGroupID
        ) else {
            let fileURL = FileManager.default.temporaryDirectory.appendingPathComponent("broadcast.mp4")
            setupWriter(at: fileURL)
            return
        }

        let fileURL = containerURL.appendingPathComponent("broadcast.mp4")
        try? FileManager.default.removeItem(at: fileURL)
        // Remove stale signal file
        let signalFile = containerURL.appendingPathComponent("broadcast_done.signal")
        try? FileManager.default.removeItem(at: signalFile)

        setupWriter(at: fileURL)
    }

    private func setupWriter(at fileURL: URL) {
        do {
            assetWriter = try AVAssetWriter(outputURL: fileURL, fileType: .mp4)
        } catch {
            finishBroadcastWithError(error as NSError)
            return
        }

        let screenScale = UIScreen.main.scale
        let screenBounds = UIScreen.main.bounds
        let videoSettings: [String: Any] = [
            AVVideoCodecKey: AVVideoCodecType.h264,
            AVVideoWidthKey: Int(screenBounds.width * screenScale),
            AVVideoHeightKey: Int(screenBounds.height * screenScale),
            AVVideoCompressionPropertiesKey: [
                AVVideoAverageBitRateKey: 6_000_000,
            ]
        ]
        videoInput = AVAssetWriterInput(mediaType: .video, outputSettings: videoSettings)
        videoInput?.expectsMediaDataInRealTime = true

        let audioSettings: [String: Any] = [
            AVFormatIDKey: kAudioFormatMPEG4AAC,
            AVSampleRateKey: 44100,
            AVNumberOfChannelsKey: 2,
            AVEncoderBitRateKey: 128_000,
        ]
        audioInput = AVAssetWriterInput(mediaType: .audio, outputSettings: audioSettings)
        audioInput?.expectsMediaDataInRealTime = true

        assetWriter?.add(videoInput!)
        assetWriter?.add(audioInput!)
        assetWriter?.startWriting()
    }

    override func broadcastPaused() {}
    override func broadcastResumed() {}

    override func broadcastFinished() {
        videoInput?.markAsFinished()
        audioInput?.markAsFinished()

        assetWriter?.finishWriting { [weak self] in
            self?.writeSignalFile()
        }
    }

    override func processSampleBuffer(_ sampleBuffer: CMSampleBuffer,
                                       with sampleBufferType: RPSampleBufferType) {
        guard assetWriter?.status == .writing else { return }

        if !isSessionStarted {
            let timestamp = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
            assetWriter?.startSession(atSourceTime: timestamp)
            isSessionStarted = true
        }

        switch sampleBufferType {
        case .video:
            if videoInput?.isReadyForMoreMediaData == true {
                videoInput?.append(sampleBuffer)
            }
        case .audioApp, .audioMic:
            if audioInput?.isReadyForMoreMediaData == true {
                audioInput?.append(sampleBuffer)
            }
        @unknown default:
            break
        }
    }

    /// Write a signal file so the main app knows recording is done.
    private func writeSignalFile() {
        guard let containerURL = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: appGroupID
        ) else { return }
        let signalFile = containerURL.appendingPathComponent("broadcast_done.signal")
        try? "done".write(to: signalFile, atomically: true, encoding: .utf8)
    }
}
