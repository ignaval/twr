import AVFoundation
import Capacitor
import Speech
import UIKit

/// Voice mode on iPhone and iPad (docs/mobile.md, Voice): Speech framework recognition from
/// the microphone, and the utterances the gateway's speaker sends: text AVSpeechSynthesizer
/// says with one of the phone's voices (`speak`), or audio a neural voice on the hub's computer
/// made, played as it arrives (`playAudio`).
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
/// - One utterance at a time, spoken or played: a new one replaces the one still going, with
///   no ack for it.
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
    let pluginMethods: [CAPPluginMethod] = ["status", "start", "stop", "cancel", "voices", "speak", "playAudio", "stopSpeaking"].map {
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
    /// The largest audio chunk `playAudio` takes, decoded: 2 MB is about 44 s at 24 kHz.
    private static let maxChunk = 2 * 1024 * 1024
    /// Audio queued but not played yet, at most: a gateway that sends faster than it plays
    /// can't fill the phone's memory.
    private static let maxQueued: TimeInterval = 180

    /// One recognition task: what it heard, and whether its `final` was sent.
    private final class Pass {
        let generation: Int
        init(generation: Int) { self.generation = generation }
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
    /// Cancelling invalidates every unfinished pass, including earlier hands-free utterances.
    private var recognitionGeneration = 0
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

    /// One utterance of audio (`playAudio`): its chunks are buffers scheduled back to back on
    /// `player`, and it is `spoken` once the last one has played.
    private final class Clip {
        let id: Int
        let format: AVAudioFormat
        init(id: Int, format: AVAudioFormat) {
            self.id = id
            self.format = format
        }
        var nextSeq = 0
        /// Buffers scheduled and not played back yet, and their frames.
        var scheduled = 0
        var queuedFrames = 0
        var lastQueued = false
    }

    /// The audio the gateway sends plays on an engine of its own: the recognition engine
    /// starts and stops with listening.
    private let playback = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    /// The format `player` is connected with (Float32 mono at the utterance's sample rate).
    private var playbackFormat: AVAudioFormat?
    private var clip: Clip?

    override func load() {
        synthesizer.delegate = self
        playback.attach(player)
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
            center.addObserver(forName: .AVAudioEngineConfigurationChange, object: playback, queue: .main) { [weak self] _ in
                self?.playbackConfigurationChanged()
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
              !synthesizer.isSpeaking, utterances.isEmpty, clip == nil else { return }
        // A running engine would keep the session busy.
        if playback.isRunning { playback.stop() }
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
            guard wasListening || synthesizer.isSpeaking || !utterances.isEmpty || clip != nil else { return }
            resumeHints = listening && mode == "hands-free" ? hints : nil
            teardown(release: false)
            stopVoice()
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
            let generation = self.recognitionGeneration
            self.authorize { problem in
                guard generation == self.recognitionGeneration else { call.resolve(); return }
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
        let threshold: Float = synthesizer.isSpeaking || clip != nil ? 0.08 : 0.025
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
        let pass = Pass(generation: recognitionGeneration)
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
        guard pass.generation == recognitionGeneration, !pass.delivered else { return }
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
        guard pass.generation == recognitionGeneration, !pass.ending else { return }
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
        guard pass.generation == recognitionGeneration, !pass.delivered else { return }
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
        recognitionGeneration += 1
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
        let wasActive = listening || pass != nil || synthesizer.isSpeaking || clip != nil
        teardown(release: false)
        stopVoice()
        if playback.isRunning { playback.stop() }
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

    private static func quality(_ voice: AVSpeechSynthesisVoice) -> String {
        switch voice.quality {
        case .premium: return "premium"
        case .enhanced: return "enhanced"
        default: return "default"
        }
    }

    private static func rank(_ voice: AVSpeechSynthesisVoice) -> Int {
        switch voice.quality {
        case .premium: return 0
        case .enhanced: return 1
        default: return 2
        }
    }

    /// The phone's voices a reply may be read with, best first: premium, enhanced, then the
    /// default ones, each by name. Novelty voices and the user's Personal Voice are left out.
    private static func phoneVoices() -> [AVSpeechSynthesisVoice] {
        AVSpeechSynthesisVoice.speechVoices()
            .filter { !$0.voiceTraits.contains(.isNoveltyVoice) && !$0.voiceTraits.contains(.isPersonalVoice) }
            .sorted { a, b in
                if rank(a) != rank(b) { return rank(a) < rank(b) }
                let byName = a.name.localizedCaseInsensitiveCompare(b.name)
                return byName == .orderedSame ? a.language < b.language : byName == .orderedAscending
            }
    }

    @objc func voices(_ call: CAPPluginCall) {
        let voices = Self.phoneVoices().map { voice -> [String: Any] in
            ["id": voice.identifier, "name": voice.name, "language": voice.language, "quality": Self.quality(voice)]
        }
        call.resolve(["voices": voices])
    }

    /// A voice of `lang` ("en", "es", or a tag with its region like "es-MX"). A bare language
    /// takes the phone's own region when it speaks that language (an en-GB phone reads "en" in
    /// en-GB); without `lang`, or with one the phone has no voice for, the phone's language.
    private static func voice(forLanguage lang: String?) -> AVSpeechSynthesisVoice? {
        let current = AVSpeechSynthesisVoice.currentLanguageCode()
        let tag = (lang ?? "").trimmingCharacters(in: .whitespaces).replacingOccurrences(of: "_", with: "-")
        guard !tag.isEmpty else { return AVSpeechSynthesisVoice(language: current) }
        let base = String(tag.split(separator: "-").first ?? Substring(tag)).lowercased()
        if tag.contains("-"), let voice = AVSpeechSynthesisVoice(language: tag) { return voice }
        if current.lowercased().hasPrefix(base + "-"), let voice = AVSpeechSynthesisVoice(language: current) { return voice }
        if let region = Locale.current.region?.identifier, let voice = AVSpeechSynthesisVoice(language: "\(base)-\(region)") {
            return voice
        }
        if let voice = AVSpeechSynthesisVoice(language: base) { return voice }
        let any = phoneVoices().first {
            let language = $0.language.lowercased()
            return language == base || language.hasPrefix(base + "-")
        }
        return any ?? AVSpeechSynthesisVoice(language: current)
    }

    /// `voice` by its identifier (what `voices` lists), else by name, in `lang` when a name is
    /// used in several languages.
    private static func voice(named name: String, lang: String?) -> AVSpeechSynthesisVoice? {
        let all = AVSpeechSynthesisVoice.speechVoices()
        if let voice = all.first(where: { $0.identifier == name }) { return voice }
        let named = all.filter { $0.name == name }
        let base = (lang ?? "").split(whereSeparator: { $0 == "-" || $0 == "_" }).first.map { $0.lowercased() } ?? ""
        return named.first(where: { !base.isEmpty && $0.language.lowercased().hasPrefix(base) }) ?? named.first
    }

    /// Stops what the phone says or plays, at once and with no ack: it was replaced or dropped.
    private func stopVoice() {
        if synthesizer.isSpeaking || !utterances.isEmpty {
            utterances.removeAll()
            synthesizer.stopSpeaking(at: .immediate)
        }
        stopPlayback()
    }

    @objc func speak(_ call: CAPPluginCall) {
        guard let id = call.getInt("id"), let text = call.getString("text"), !text.isEmpty, text.count <= 4096 else {
            call.reject("An utterance needs an id and its text")
            return
        }
        let voiceName = call.getString("voice")
        let lang = call.getString("lang")
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
            if let voiceName, !voiceName.isEmpty, let voice = Self.voice(named: voiceName, lang: lang) {
                utterance.voice = voice
            } else {
                utterance.voice = Self.voice(forLanguage: lang)
            }
            if rate > 0 {
                // AVSpeechUtteranceDefaultSpeechRate is about 180 words per minute.
                let scaled = AVSpeechUtteranceDefaultSpeechRate * Float(rate) / 180
                utterance.rate = min(max(scaled, AVSpeechUtteranceMinimumSpeechRate), AVSpeechUtteranceMaximumSpeechRate)
            }
            // One utterance at a time: a new one replaces what is still said or played.
            self.stopVoice()
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
            }
            if let clip = self.clip, id == nil || clip.id == id {
                self.stopPlayback()
            }
            self.scheduleRelease()
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

    // MARK: The hub's voice

    /// A chunk of an utterance a neural voice on the hub's computer made: `pcm` is base64 of
    /// signed 16-bit little-endian mono samples at `sampleRate`, `seq` its position from 0, and
    /// `last` marks the utterance's final chunk. Chunks play back to back; `spoken` follows once
    /// the last one has played, `speakError` when one is missing or the audio stops.
    ///
    /// A first chunk (`seq` 0) replaces whatever is said or played, with no ack for it, and
    /// needs twr in the foreground. A later chunk of an utterance that was replaced or stopped
    /// is ignored. A chunk that can't be played (malformed, over 2 MB, a new sample rate, more
    /// than 3 minutes queued) is rejected, and its utterance stops without an event: the
    /// rejection is its error.
    @objc func playAudio(_ call: CAPPluginCall) {
        guard let id = call.getInt("id"), let seq = call.getInt("seq"), seq >= 0 else {
            call.reject("An audio chunk needs its utterance's id and its position")
            return
        }
        let sampleRate = call.getInt("sampleRate") ?? 0
        guard (8000...96000).contains(sampleRate),
              let format = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: Double(sampleRate), channels: 1, interleaved: false) else {
            call.reject("An audio chunk needs a sample rate from 8000 to 96000 Hz")
            return
        }
        let last = call.getBool("last") ?? false
        let encoded = call.getString("pcm") ?? ""
        // Base64 is 4 characters for 3 bytes: refuse a chunk too big before decoding it.
        guard encoded.utf8.count <= (Self.maxChunk / 3 + 1) * 4 else {
            call.reject("An audio chunk may hold 2 MB at most")
            return
        }
        guard let data = encoded.isEmpty ? Data() : Data(base64Encoded: encoded) else {
            call.reject("The audio chunk is not base64")
            return
        }
        guard data.count <= Self.maxChunk else {
            call.reject("An audio chunk may hold 2 MB at most")
            return
        }
        guard data.count % 2 == 0 else {
            call.reject("An audio chunk holds whole 16-bit samples")
            return
        }
        var buffer: AVAudioPCMBuffer?
        if !data.isEmpty {
            guard let converted = Self.pcmBuffer(data, format: format) else {
                call.reject("The audio chunk could not be read")
                return
            }
            buffer = converted
        }
        DispatchQueue.main.async {
            self.queueAudio(call, id: id, seq: seq, format: format, buffer: buffer, last: last)
        }
    }

    /// Signed 16-bit little-endian samples as the player's Float32.
    private static func pcmBuffer(_ data: Data, format: AVAudioFormat) -> AVAudioPCMBuffer? {
        let frames = data.count / 2
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)),
              let samples = buffer.floatChannelData?[0] else { return nil }
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            for i in 0..<frames {
                samples[i] = Float(Int16(littleEndian: raw.loadUnaligned(fromByteOffset: i * 2, as: Int16.self))) / 32768
            }
        }
        buffer.frameLength = AVAudioFrameCount(frames)
        return buffer
    }

    private func queueAudio(_ call: CAPPluginCall, id: Int, seq: Int, format: AVAudioFormat, buffer: AVAudioPCMBuffer?, last: Bool) {
        let frames = Int(buffer?.frameLength ?? 0)
        let limit = Int(format.sampleRate * Self.maxQueued)
        let clip: Clip
        if seq == 0 {
            guard UIApplication.shared.applicationState == .active else {
                call.reject("Voice mode works while twr is open")
                return
            }
            guard frames <= limit else {
                call.reject("An utterance may queue 3 minutes of audio at most")
                return
            }
            do { try configureSession() } catch {
                call.reject("The phone's audio could not start: \(error.localizedDescription)")
                return
            }
            // One utterance at a time: this one replaces what is still said or played.
            stopVoice()
            clip = Clip(id: id, format: format)
            self.clip = clip
            do { try startPlayback(format) } catch {
                self.clip = nil
                scheduleRelease()
                call.reject("The phone's audio could not start: \(error.localizedDescription)")
                return
            }
        } else {
            // A chunk of an utterance that was replaced, stopped or has ended.
            guard let current = self.clip, current.id == id, !current.lastQueued else {
                call.resolve()
                return
            }
            guard seq == current.nextSeq else {
                fail(current, "A piece of the audio went missing")
                call.resolve()
                return
            }
            guard current.format.sampleRate == format.sampleRate else {
                stopPlayback()
                scheduleRelease()
                call.reject("An utterance's audio keeps one sample rate")
                return
            }
            guard current.queuedFrames + frames <= limit else {
                stopPlayback()
                scheduleRelease()
                call.reject("An utterance may queue 3 minutes of audio at most")
                return
            }
            clip = current
        }
        clip.nextSeq = seq + 1
        if let buffer {
            clip.scheduled += 1
            clip.queuedFrames += frames
            player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self, weak clip] _ in
                DispatchQueue.main.async {
                    guard let self, let clip, clip === self.clip else { return }
                    // A stopped engine calls this too: `playbackConfigurationChanged` ends it.
                    guard self.playback.isRunning else { return }
                    clip.scheduled -= 1
                    clip.queuedFrames -= frames
                    self.finishIfPlayed(clip)
                }
            }
        }
        if last {
            clip.lastQueued = true
            finishIfPlayed(clip)
        }
        call.resolve()
    }

    /// Connects `player` for `format` (a new sample rate reconnects it) and starts it.
    private func startPlayback(_ format: AVAudioFormat) throws {
        if playbackFormat?.sampleRate != format.sampleRate {
            if playback.isRunning { playback.stop() }
            playback.disconnectNodeOutput(player)
            playback.connect(player, to: playback.mainMixerNode, format: format)
            playbackFormat = format
        }
        if !playback.isRunning {
            playback.prepare()
            try playback.start()
        }
        player.play()
    }

    /// Stops the audio utterance at once, with no ack. The engine stays up for the next one
    /// until the session is given back.
    private func stopPlayback() {
        clip = nil
        // Its buffers' completion handlers run now and find no clip.
        if playbackFormat != nil { player.stop() }
    }

    private func finishIfPlayed(_ clip: Clip) {
        guard clip === self.clip, clip.lastQueued, clip.scheduled == 0 else { return }
        self.clip = nil
        emit(["type": "spoken", "id": clip.id])
        // The next sentence usually follows at once; the session goes back if none does.
        scheduleRelease(after: 2)
    }

    private func fail(_ clip: Clip, _ message: String) {
        guard clip === self.clip else { return }
        stopPlayback()
        emit(["type": "speakError", "id": clip.id, "message": message])
        scheduleRelease()
    }

    /// The audio route changed (headphones, Bluetooth) and the playback engine stopped itself,
    /// dropping what it had queued: the utterance can't go on.
    private func playbackConfigurationChanged() {
        guard let clip, !playback.isRunning else { return }
        fail(clip, "The phone's audio stopped when the audio route changed")
    }
}
