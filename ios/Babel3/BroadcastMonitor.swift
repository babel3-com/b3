import Foundation
import Photos
import UIKit

/// Monitors broadcast extension lifecycle via shared app group container.
/// Polls for a signal file written by the extension when recording finishes,
/// then saves the MP4 to the camera roll.
class BroadcastMonitor {
    static let shared = BroadcastMonitor()

    private let appGroupID = "group.com.babel3.ios"
    private var pollTimer: Timer?

    func startMonitoring() {
        print("[Broadcast] Monitor started — polling for recordings")

        // Poll every 2 seconds for completed recordings
        pollTimer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.checkForCompletedRecording()
        }
    }

    private func checkForCompletedRecording() {
        guard let containerURL = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: appGroupID
        ) else { return }

        let signalFile = containerURL.appendingPathComponent("broadcast_done.signal")
        guard FileManager.default.fileExists(atPath: signalFile.path) else { return }

        // Signal found — recording is complete
        try? FileManager.default.removeItem(at: signalFile)

        let fileURL = containerURL.appendingPathComponent("broadcast.mp4")
        guard FileManager.default.fileExists(atPath: fileURL.path) else {
            print("[Broadcast] Signal found but no broadcast.mp4")
            return
        }

        let fileSize = (try? FileManager.default.attributesOfItem(atPath: fileURL.path)[.size] as? Int) ?? 0
        print("[Broadcast] Recording complete — \(fileSize / 1024)KB")

        saveToPhotoLibrary(fileURL: fileURL)
    }

    private func saveToPhotoLibrary(fileURL: URL) {
        PHPhotoLibrary.requestAuthorization(for: .addOnly) { status in
            guard status == .authorized else {
                print("[Broadcast] Photo library access denied")
                return
            }

            PHPhotoLibrary.shared().performChanges({
                PHAssetChangeRequest.creationRequestForAssetFromVideo(atFileURL: fileURL)
            }) { success, error in
                if success {
                    print("[Broadcast] Saved to camera roll")
                    try? FileManager.default.removeItem(at: fileURL)
                } else {
                    print("[Broadcast] Save failed: \(error?.localizedDescription ?? "unknown")")
                }
            }
        }
    }
}
