import AVFoundation
import Capacitor
import Speech
import UIKit

/// Voice mode on iPhone and iPad (docs/mobile.md, Voice): Speech framework recognition from
/// the microphone, and AVSpeechSynthesizer saying the utterances the gateway's speaker sends.
///
/// - Recognition is on the device when the recognizer supports it
///   (`requiresOnDeviceRecognition`); otherwise the audio goes to Apple's servers, and
///   `status` says so (`local: false`).
/// - Push mode records until `stop`; hands-free keeps listening, ends an utterance after
///   2 s of silence (shared/voice.ts `PAUSE_MS`), and starts a new recognition task for the next (a task lasts
///   about a minute at most, so a long silence restarts it too).
/// - The audio session is `.playAndRecord` in `.voiceChat` mode with voice processing on
///   the input: the system's echo cancellation keeps the phone's own voice out of what is
///   heard.
/// - Foreground only: leaving the app ends recognition and the voice (an `error` event with
///   `background: true`).
/// - The session is given back (`.notifyOthersOnDeactivation`) once nothing listens or speaks,
///   so other apps' audio resumes. A call or Siri ends listening and the voice (an `error`
///   event with `interrupted: true`); hands-free listens again when the interruption ends with
///   `.shouldResume`. An audio route change restarts the engine in hands-free and ends a push
///   recording with what it heard.
/// Every event is one `voice` listener call with a `type`; all state lives on the main queue.
@objc(TwrVoicePlugin)
final class TwrVoicePlugin: CAPPlugin, CAPBridgedPlugin, AVSpeechSynthesizerDelegate {
    let identifier = "TwrVoicePlugin"
    let jsName = "TwrVoice"
    let pluginMethods: [CAPPluginMethod] = ["status", "start", "stop", "cancel", "speak", "stopSpeaking"].map {
        CAPPluginMethod(name: $0, returnType: CAPPluginReturnPromise)
    }

    /// Silence that ends a hands-free utterance (shared/voice.ts `PAUSE_MS`): long enough for a
    /// pause to think mid-sentence.
    private static let endSilence: TimeInterval = 2.0
    /// A sound loud enough to count as speech that brings no words this soon (a cough, a door)
    /// ends its utterance empty: the phone's own voice, paused for it, goes on.
    private static let noWords: TimeInterval = 2.5
    /// A recognition task is restarted before Apple's limit of about a minute.
    private static let taskLimit: TimeInterval = 50
    /// How long a final result may take after the audio ended.
    private static let finalWait: TimeInterval = 2

    /// One recognition task: what it heard, and whether its `final` was sent.
    private final class Pass {
        let request = SFSpeechAudioBufferRecognitionRequest()
        var task: SFSpeechRecognitionTask?
        var text = ""
        var delivered = false
        var ending = false
        let started = Date()
    }

    private let audio = AVAudioEngine()
    private var recognizer: SFSpeechRecognizer?
    private var pass: Pass?
    /// Read by the audio thread: the request buffers go to.
    private let bufferLock = NSLock()
    private var bufferTarget: SFSpeechAudioBufferRecognitionRequest?
    private var listening = false
    private var mode = "push"
    private var hints: [String] = []
    private var heardSpeech = false
    private var silenceTimer: Timer?
    private var limitTimer: Timer?
    private var lastLevel: TimeInterval = 0
    private var quickFailures = 0

    private let synthesizer = AVSpeechSynthesizer()
    private var utterances: [ObjectIdentifier: Int] = [:]
    private var observers: [NSObjectProtocol] = []
    /// This plugin activated the shared audio session and has not given it back yet.
    private var sessionActive = false
    /// A hands-free session a call, Siri or another app interrupted: its hints, to listen
    /// again when the interruption ends with `.shouldResume`.
    private var resumeHints: [String]?
    /// Recent engine restarts after configuration changes, to stop a restart loop.
    private var engineRestarts: [Date] = []

    override func load() {
        synthesizer.delegate = self
        recognizer = SFSpeechRecognizer(locale: Locale.current) ?? SFSpeechRecognizer(locale: Locale(identifier: "en-US"))
        let center = NotificationCenter.default
        observers = [
            center.addObserver(forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main) { [weak self] _ in
                self?.enteredBackground()
            },
            center.addObserver(forName: AVAudioSession.interruptionNotification, object: AVAudioSession.sharedInstance(), queue: .main) { [weak self] note in
                self?.interrupted(note)
            },
            center.addObserver(forName: .AVAudioEngineConfigurationChange, object: audio, queue: .main) { [weak self] _ in
                self?.engineConfigurationChanged()
            },
        ]
    }

    deinit {
        observers.forEach { NotificationCenter.default.removeObserver($0) }
    }

    private func emit(_ data: [String: Any]) {
        notifyListeners("voice", data: data)
    }

    // MARK: Status and permissions

    private func problem() -> String? {
        switch SFSpeechRecognizer.authorizationStatus() {
        case .denied: return "Speech recognition is off for twr. Allow it in Settings › Privacy & Security › Speech Recognition."
        case .restricted: return "Speech recognition is restricted on this device."
        default: break
        }
        if AVAudioApplication.shared.recordPermission == .denied {
            return "The microphone is off for twr. Allow it in Settings › Privacy & Security › Microphone."
        }
        guard let recognizer else { return "Speech recognition does not support this device's language." }
        if !recognizer.isAvailable { return "Speech recognition is not available right now." }
        return nil
    }

    @objc func status(_ call: CAPPluginCall) {
        DispatchQueue.main.async {
            let local = self.recognizer?.supportsOnDeviceRecognition ?? false
            var result: [String: Any] = [
                "available": self.problem() == nil,
                "local": local,
                "engine": local ? "iOS Speech (on this device)" : "iOS Speech (Apple's servers)",
            ]
            if let problem = self.problem() { result["problem"] = problem }
            call.resolve(result)
        }
    }

    /// Asks for speech recognition and the microphone the first time, with clear errors.
    private func authorize(_ done: @escaping (String?) -> Void) {
        SFSpeechRecognizer.requestAuthorization { status in
            guard status == .authorized else {
                DispatchQueue.main.async {
                    done(status == .restricted
                        ? "Speech recognition is restricted on this device."
                        : "Voice mode needs speech recognition. Allow it in Settings › Privacy & Security › Speech Recognition.")
                }
                return
            }
            AVAudioApplication.requestRecordPermission { granted in
                DispatchQueue.main.async {
                    done(granted ? nil : "Voice mode needs the microphone. Allow it in Settings › Privacy & Security › Microphone.")
                }
            }
        }
    }

    private func configureSession() throws {
        let session = AVAudioSession.sharedInstance()
        let options: AVAudioSession.CategoryOptions = [.defaultToSpeaker, .allowBluetoothHFP]
        if session.category != .playAndRecord || session.mode != .voiceChat || !session.categoryOptions.contains(.defaultToSpeaker) {
            try session.setCategory(.playAndRecord, mode: .voiceChat, options: options)
        }
        try session.setActive(true)
        sessionActive = true
    }

    /// Gives the audio session back once nothing listens or speaks, so the music or podcast
    /// voice mode interrupted resumes (`.notifyOthersOnDeactivation`).
    private func releaseSessionIfIdle() {
        guard sessionActive, !listening, pass == nil, !audio.isRunning,
              !synthesizer.isSpeaking, utterances.isEmpty else { return }
        sessionActive = false
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    /// The synthesizer reports `isSpeaking` a moment after it was stopped, and a reply's next
    /// sentence arrives over the network: check again after `delay`.
    private func scheduleRelease(after delay: TimeInterval = 0.5) {
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in self?.releaseSessionIfIdle() }
    }

    // MARK: Interruptions and route changes

    /// A call, Siri or another app's audio took the session: the engine has stopped, so the
    /// recognition and the voice end, and the UI hears it as an `error` (`interrupted: true`).
    /// A hands-free session listens again when the interruption ends with `.shouldResume`.
    private func interrupted(_ note: Notification) {
        guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
              let type = AVAudioSession.InterruptionType(rawValue: raw) else { return }
        switch type {
        case .began:
            // The system deactivated the session.
            sessionActive = false
            let wasListening = listening || pass != nil
            guard wasListening || synthesizer.isSpeaking || !utterances.isEmpty else { return }
            resumeHints = listening && mode == "hands-free" ? hints : nil
            teardown(release: false)
            utterances.removeAll()
            synthesizer.stopSpeaking(at: .immediate)
            emit(["type": "error", "message": "Voice mode stopped for a call or another app's audio", "interrupted": true])
        case .ended:
            guard let hints = resumeHints else { return }
            resumeHints = nil
            let options = AVAudioSession.InterruptionOptions(rawValue: note.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt ?? 0)
            guard options.contains(.shouldResume), UIApplication.shared.applicationState == .active else { return }
            do {
                teardown(release: false)
                mode = "hands-free"
                self.hints = hints
                quickFailures = 0
                try configureSession()
                try startAudio()
                listening = true
                beginPass()
            } catch {
                teardown()
                emit(["type": "error", "message": "The microphone could not start again: \(error.localizedDescription)"])
            }
        @unknown default:
            break
        }
    }

    /// The input or output route changed (headphones, Bluetooth, a dock) and the engine stopped
    /// itself. What the current pass heard is delivered; hands-free starts the engine again with
    /// the new input format, push mode ends its recording there.
    private func engineConfigurationChanged() {
        guard listening, !audio.isRunning else { return }
        let now = Date()
        engineRestarts = engineRestarts.filter { now.timeIntervalSince($0) < 10 } + [now]
        guard mode == "hands-free", engineRestarts.count <= 3 else {
            let handsFree = mode == "hands-free"
            listening = false
            if let pass { endPass(pass) } else { teardown() }
            stopAudio()
            if handsFree {
                emit(["type": "error", "message": "The microphone stopped after the audio route changed"])
            }
            scheduleRelease()
            return
        }
        bufferLock.lock()
        bufferTarget = nil
        bufferLock.unlock()
        audio.inputNode.removeTap(onBus: 0)
        if let pass { endPass(pass) } else { beginPass() }
        do {
            try configureSession()
            try startAudio()
        } catch {
            teardown()
            emit(["type": "error", "message": "The microphone could not start again: \(error.localizedDescription)"])
        }
    }

    // MARK: Recognition

    @objc func start(_ call: CAPPluginCall) {
        let mode = call.getString("mode") == "hands-free" ? "hands-free" : "push"
        let hints = (call.getArray("hint", String.self) ?? []).prefix(64).filter { $0.count <= 64 }
        DispatchQueue.main.async {
            guard UIApplication.shared.applicationState == .active else {
                call.reject("Voice mode works while twr is open")
                return
            }
            self.authorize { problem in
                if let problem { call.reject(problem); return }
                if let problem = self.problem() { call.reject(problem); return }
                do {
                    self.teardown(release: false)
                    self.engineRestarts = []
                    self.mode = mode
                    self.hints = Array(hints)
                    self.quickFailures = 0
                    try self.configureSession()
                    try self.startAudio()
                    self.listening = true
                    self.beginPass()
                    if mode == "push" { self.speechStarted() }
                    call.resolve()
                } catch {
                    self.teardown()
                    call.reject("The microphone could not start: \(error.localizedDescription)")
                }
            }
        }
    }

    private func startAudio() throws {
        let input = audio.inputNode
        // The system's echo cancellation: the phone's own voice is taken out of the input.
        try? input.setVoiceProcessingEnabled(true)
        let format = input.outputFormat(forBus: 0)
        input.removeTap(onBus: 0)
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
            guard let self else { return }
            self.bufferLock.lock()
            let target = self.bufferTarget
            self.bufferLock.unlock()
            target?.append(buffer)
            let level = Self.rms(buffer)
            DispatchQueue.main.async { self.heard(level: level) }
        }
        audio.prepare()
        try audio.start()
    }

    private static func rms(_ buffer: AVAudioPCMBuffer) -> Float {
        guard let data = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return 0 }
        var sum: Float = 0
        for i in 0..<Int(buffer.frameLength) { sum += data[i] * data[i] }
        return (sum / Float(buffer.frameLength)).squareRoot()
    }

    /// The input level: `level` events a few times a second, and `speechStart` the first time
    /// it is loud enough in an utterance (louder while the phone's own voice plays).
    private func heard(level: Float) {
        guard listening else { return }
        let now = Date().timeIntervalSince1970
        if now - lastLevel > 0.1 {
            lastLevel = now
            emit(["type": "level", "level": Double(min(1, level * 4))])
        }
        let threshold: Float = synthesizer.isSpeaking ? 0.08 : 0.025
        if !heardSpeech && level > threshold { speechStarted() }
    }

    private func speechStarted() {
        guard !heardSpeech else { return }
        heardSpeech = true
        emit(["type": "speechStart"])
        // Words re-arm this with the shorter end-of-speech silence (`recognized`).
        if mode == "hands-free", let pass, pass.text.isEmpty {
            silenceTimer?.invalidate()
            silenceTimer = Timer.scheduledTimer(withTimeInterval: Self.noWords, repeats: false) { [weak self, weak pass] _ in
                guard let self, let pass, pass === self.pass, pass.text.isEmpty else { return }
                self.endPass(pass)
            }
        }
    }

    private func beginPass() {
        guard let recognizer, listening else { return }
        let pass = Pass()
        pass.request.shouldReportPartialResults = true
        pass.request.requiresOnDeviceRecognition = recognizer.supportsOnDeviceRecognition
        pass.request.contextualStrings = hints
        pass.request.taskHint = .dictation
        pass.request.addsPunctuation = true
        self.pass = pass
        bufferLock.lock()
        bufferTarget = pass.request
        bufferLock.unlock()
        pass.task = recognizer.recognitionTask(with: pass.request) { [weak self, weak pass] result, error in
            DispatchQueue.main.async {
                guard let self, let pass else { return }
                self.recognized(pass, result: result, error: error)
            }
        }
        limitTimer?.invalidate()
        limitTimer = Timer.scheduledTimer(withTimeInterval: Self.taskLimit, repeats: false) { [weak self, weak pass] _ in
            guard let self, let pass, pass === self.pass else { return }
            self.endPass(pass)
        }
    }

    private func recognized(_ pass: Pass, result: SFSpeechRecognitionResult?, error: Error?) {
        if let result {
            let text = result.bestTranscription.formattedString
            if !pass.ending && text != pass.text {
                pass.text = text
                if !text.isEmpty {
                    quickFailures = 0
                    speechStarted()
                    emit(["type": "partial", "text": text])
                }
                if mode == "hands-free" && !text.isEmpty {
                    silenceTimer?.invalidate()
                    silenceTimer = Timer.scheduledTimer(withTimeInterval: Self.endSilence, repeats: false) { [weak self, weak pass] _ in
                        guard let self, let pass, pass === self.pass else { return }
                        self.endPass(pass)
                    }
                }
            }
            if result.isFinal {
                if pass.ending || text.count >= pass.text.count { pass.text = text }
                deliver(pass)
                return
            }
        }
        if let error {
            if pass.delivered { return }
            // A task that heard nothing ends with an error ("no speech detected", code 1110):
            // what it heard is the utterance, and hands-free listening goes on. Any other
            // error of a push recording that heard nothing is the user's to see.
            let code = (error as NSError).code
            if mode == "hands-free" && pass.text.isEmpty && Date().timeIntervalSince(pass.started) < 1 {
                // Tasks failing as they start would restart forever: three in a row end it.
                quickFailures += 1
                if quickFailures >= 3 {
                    teardown()
                    emit(["type": "error", "message": "Speech recognition stopped: \(error.localizedDescription)"])
                    return
                }
            }
            let report = pass.text.isEmpty && mode == "push" && pass === self.pass && code != 1110 && code != 216
            deliver(pass)
            if report {
                emit(["type": "error", "message": "Speech recognition stopped: \(error.localizedDescription)"])
            }
        }
    }

    /// Ends `pass`'s audio; its final result follows (or what it heard, after a moment).
    private func endPass(_ pass: Pass) {
        guard !pass.ending else { return }
        pass.ending = true
        silenceTimer?.invalidate()
        pass.request.endAudio()
        if self.pass === pass {
            bufferLock.lock()
            bufferTarget = nil
            bufferLock.unlock()
            // Hands-free: the next utterance has a task of its own, listening already.
            if mode == "hands-free" && listening { beginPass() }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.finalWait) { [weak self, weak pass] in
            guard let self, let pass else { return }
            self.deliver(pass)
        }
    }

    /// Sends `pass`'s `final` once.
    private func deliver(_ pass: Pass) {
        guard !pass.delivered else { return }
        pass.delivered = true
        pass.task?.cancel()
        heardSpeech = false
        emit(["type": "final", "text": pass.text.trimmingCharacters(in: .whitespacesAndNewlines)])
        if pass === self.pass {
            if mode == "hands-free" && listening {
                beginPass()
            } else {
                self.pass = nil
                if !listening || mode == "push" {
                    stopAudio()
                    releaseSessionIfIdle()
                }
            }
        }
    }

    @objc func stop(_ call: CAPPluginCall) {
        DispatchQueue.main.async {
            guard let pass = self.pass else {
                // Nothing to recognize: the composer still hears the end of the utterance.
                let wasListening = self.listening
                self.teardown()
                if wasListening { self.emit(["type": "final", "text": ""]) }
                call.resolve()
                return
            }
            self.listening = false
            self.endPass(pass)
            call.resolve()
        }
    }

    @objc func cancel(_ call: CAPPluginCall) {
        DispatchQueue.main.async {
            self.teardown()
            call.resolve()
        }
    }

    private func stopAudio() {
        bufferLock.lock()
        bufferTarget = nil
        bufferLock.unlock()
        if audio.isRunning { audio.stop() }
        audio.inputNode.removeTap(onBus: 0)
    }

    /// Stops listening and drops what was not recognized yet; with `release`, gives the audio
    /// session back unless the phone's voice is still speaking.
    private func teardown(release: Bool = true) {
        resumeHints = nil
        listening = false
        silenceTimer?.invalidate()
        limitTimer?.invalidate()
        if let pass {
            pass.delivered = true
            pass.task?.cancel()
        }
        pass = nil
        heardSpeech = false
        stopAudio()
        if release { releaseSessionIfIdle() }
    }

    private func enteredBackground() {
        let wasActive = listening || pass != nil || synthesizer.isSpeaking
        teardown(release: false)
        utterances.removeAll()
        synthesizer.stopSpeaking(at: .immediate)
        // Nothing listens or speaks any more: give the session back now, while the app still runs.
        if sessionActive {
            sessionActive = false
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        }
        if wasActive {
            emit(["type": "error", "message": "Voice mode stops while twr is in the background", "background": true])
        }
    }

    // MARK: The phone's voice

    @objc func speak(_ call: CAPPluginCall) {
        guard let id = call.getInt("id"), let text = call.getString("text"), !text.isEmpty, text.count <= 4096 else {
            call.reject("An utterance needs an id and its text")
            return
        }
        let voiceName = call.getString("voice")
        let rate = call.getInt("rate") ?? 0
        DispatchQueue.main.async {
            guard UIApplication.shared.applicationState == .active else {
                call.reject("Voice mode works while twr is open")
                return
            }
            do { try self.configureSession() } catch {
                call.reject("The phone's audio could not start: \(error.localizedDescription)")
                return
            }
            let utterance = AVSpeechUtterance(string: text)
            if let voiceName, !voiceName.isEmpty,
               let voice = AVSpeechSynthesisVoice.speechVoices().first(where: { $0.name == voiceName || $0.identifier == voiceName }) {
                utterance.voice = voice
            } else {
                utterance.voice = AVSpeechSynthesisVoice(language: AVSpeechSynthesisVoice.currentLanguageCode())
            }
            if rate > 0 {
                // AVSpeechUtteranceDefaultSpeechRate is about 180 words per minute.
                let scaled = AVSpeechUtteranceDefaultSpeechRate * Float(rate) / 180
                utterance.rate = min(max(scaled, AVSpeechUtteranceMinimumSpeechRate), AVSpeechUtteranceMaximumSpeechRate)
            }
            // One utterance at a time: a new one replaces what is still said.
            if self.synthesizer.isSpeaking {
                self.utterances.removeAll()
                self.synthesizer.stopSpeaking(at: .immediate)
            }
            self.utterances[ObjectIdentifier(utterance)] = id
            self.synthesizer.speak(utterance)
            call.resolve()
        }
    }

    @objc func stopSpeaking(_ call: CAPPluginCall) {
        let id = call.getInt("id")
        DispatchQueue.main.async {
            if id == nil || self.utterances.values.contains(id!) {
                self.utterances.removeAll()
                self.synthesizer.stopSpeaking(at: .immediate)
                self.scheduleRelease()
            }
            call.resolve()
        }
    }

    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didFinish utterance: AVSpeechUtterance) {
        DispatchQueue.main.async {
            if let id = self.utterances.removeValue(forKey: ObjectIdentifier(utterance)) {
                self.emit(["type": "spoken", "id": id])
            }
            // The next sentence usually follows at once; the session goes back if none does.
            if self.utterances.isEmpty { self.scheduleRelease(after: 2) }
        }
    }

    func speechSynthesizer(_ synthesizer: AVSpeechSynthesizer, didCancel utterance: AVSpeechUtterance) {
        DispatchQueue.main.async {
            // Stopped on purpose: the gateway dropped it already, nothing is acked.
            self.utterances.removeValue(forKey: ObjectIdentifier(utterance))
        }
    }
}
