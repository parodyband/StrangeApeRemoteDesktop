import AVFoundation

/// Plays a screen session's sound: pulls 48 kHz stereo from the core's
/// jitter buffer on the audio thread.
final class AudioPlayer {
    private let engine = AVAudioEngine()
    private var node: AVAudioSourceNode?
    /// Read on the audio thread; written on the main thread before start and
    /// after stop only.
    private var session: OpaquePointer?

    func start(session: OpaquePointer) {
        stop()
        self.session = session
        let audio = AVAudioSession.sharedInstance()
        try? audio.setCategory(.playback, mode: .default, options: [.mixWithOthers])
        try? audio.setPreferredIOBufferDuration(0.005)
        try? audio.setActive(true)
        guard let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2) else { return }
        // Scratch for interleaved int16 from the core (allocated once, not
        // on the audio thread).
        let scratch = UnsafeMutablePointer<Int16>.allocate(capacity: 8192 * 2)
        let node = AVAudioSourceNode(format: format) { [unowned self] _, _, frameCount, bufferList in
            let frames = min(Int(frameCount), 8192)
            sard_audio_pull(self.session, scratch, frames)
            let buffers = UnsafeMutableAudioBufferListPointer(bufferList)
            guard buffers.count >= 2,
                  let left = buffers[0].mData?.assumingMemoryBound(to: Float.self),
                  let right = buffers[1].mData?.assumingMemoryBound(to: Float.self) else { return noErr }
            for i in 0..<frames {
                left[i] = Float(scratch[2 * i]) / 32768
                right[i] = Float(scratch[2 * i + 1]) / 32768
            }
            return noErr
        }
        self.node = node
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        do {
            try engine.start()
        } catch {
            NSLog("Sard audio: \(error)")
        }
        // The scratch buffer lives as long as the node (freed in stop()).
        scratchBuffer = scratch
    }

    private var scratchBuffer: UnsafeMutablePointer<Int16>?

    func stop() {
        if let node {
            engine.stop()
            engine.detach(node)
            self.node = nil
        }
        session = nil
        scratchBuffer?.deallocate()
        scratchBuffer = nil
    }

    deinit { stop() }
}
