package tools.twr.mobile.dev;

import android.Manifest;
import android.content.Intent;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioTrack;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.util.Base64;
import android.speech.RecognitionListener;
import android.speech.RecognizerIntent;
import android.speech.SpeechRecognizer;
import android.speech.tts.TextToSpeech;
import android.speech.tts.UtteranceProgressListener;
import android.speech.tts.Voice;
import androidx.lifecycle.Lifecycle;
import com.getcapacitor.*;
import com.getcapacitor.annotation.CapacitorPlugin;
import com.getcapacitor.annotation.Permission;
import com.getcapacitor.annotation.PermissionCallback;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Locale;
import java.util.Set;

/**
 * Voice mode on Android (docs/mobile.md, Voice): SpeechRecognizer for what the user says, and
 * the utterances the gateway's speaker sends: text TextToSpeech says with one of the phone's
 * voices ({@code speak}), or audio a neural voice on the hub's computer made, played on an
 * AudioTrack as it arrives ({@code playAudio}). One utterance at a time, spoken or played: a new
 * one replaces the one still going, with no ack for it.
 *
 * <p>Recognition is on the device with {@code createOnDeviceSpeechRecognizer} (API 31+) when
 * {@code isOnDeviceRecognitionAvailable}; otherwise the default recognizer is asked to prefer
 * offline, and the audio may go to its service ({@code status} says {@code local: false}).
 * Android has no continuous recognition: hands-free restarts the recognizer after each result,
 * no-match or speech timeout, after a short backoff, so a word said in that gap can be missed.
 * Foreground only: pausing the app ends recognition and the voice (an {@code error} event with
 * {@code background: true}). Every event is one {@code voice} listener call with a {@code type};
 * recognizer calls and state stay on the main thread, AudioTrack calls on its own thread.
 */
@CapacitorPlugin(
        name = "TwrVoice",
        permissions = {@Permission(alias = "microphone", strings = {Manifest.permission.RECORD_AUDIO})})
public class TwrVoicePlugin extends Plugin {
    private static final long RESTART_AFTER_RESULT = 250, RESTART_AFTER_SILENCE = 500, RESTART_WHEN_BUSY = 1000;
    private static final long FINAL_WAIT = 3000;
    /** Silence that ends a hands-free utterance (shared/voice.ts {@code PAUSE_MS}). */
    private static final long END_SILENCE = 2000;
    private final Handler main = new Handler(Looper.getMainLooper());
    private SpeechRecognizer recognizer;
    private boolean onDevice;
    private boolean listening;
    private boolean heardSpeech;
    private String mode = "push";
    private ArrayList<String> hints = new ArrayList<>();
    private String partial = "";
    private int quickFailures;
    private long passStarted;
    private long lastLevel;
    /** Bumped whenever recognition is torn down or restarted: late callbacks are ignored. */
    private int generation;

    private TextToSpeech tts;
    private boolean ttsReady, ttsFailed;
    private final List<Runnable> waitingForTts = new ArrayList<>();
    private Integer speaking;
    private static final String NO_TTS = "This phone has no text-to-speech voice. Install one in Settings › Accessibility › Text-to-speech.";
    /** How long {@code voices} waits for TextToSpeech to start. */
    private static final long TTS_WAIT = 5000;

    /** The largest audio chunk {@code playAudio} takes, decoded: 2 MB is about 44 s at 24 kHz. */
    private static final int MAX_CHUNK = 2 * 1024 * 1024;
    /** Audio queued but not played yet, at most, in seconds. */
    private static final int MAX_QUEUED = 180;
    /** How often the audio thread writes what is waiting and checks what has played. */
    private static final long PUMP_EVERY = 20, WATCH_EVERY = 50;
    /** A drained track whose position stops moving this long has played all it will. */
    private static final long STALLED = 1500;

    /**
     * One utterance of audio ({@code playAudio}). The main thread owns {@code nextSeq},
     * {@code frames} and {@code last}; the audio thread owns the track and what waits for it.
     */
    private static final class Clip {
        final int id, sampleRate;
        Clip(int id, int sampleRate) { this.id = id; this.sampleRate = sampleRate; }
        int nextSeq;
        /** Frames handed to the audio thread. */
        long frames;
        boolean last;
        /** Frames the track has played, for the main thread's cap on what is queued. */
        volatile long played;

        AudioTrack track;
        final ArrayDeque<byte[]> waiting = new ArrayDeque<>();
        int offset;
        long written;
        boolean ending, closed, pumping;
        long lastHead = -1, movedAt;
    }

    private HandlerThread audioThread;
    private Handler audio;
    private Clip clip;

    private void emit(JSObject data) {
        notifyListeners("voice", data);
    }

    private static JSObject event(String type) {
        return new JSObject().put("type", type);
    }

    @Override
    public void load() {
        audioThread = new HandlerThread("twr-voice-audio");
        audioThread.start();
        audio = new Handler(audioThread.getLooper());
        tts = new TextToSpeech(getContext(), status -> main.post(() -> {
            ttsReady = status == TextToSpeech.SUCCESS;
            ttsFailed = !ttsReady;
            if (ttsReady) {
                tts.setAudioAttributes(new AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_ASSISTANT)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH).build());
                tts.setOnUtteranceProgressListener(new UtteranceProgressListener() {
                    @Override public void onStart(String id) {}
                    @Override public void onDone(String id) { main.post(() -> finished(id, null)); }
                    @Override public void onError(String id) { main.post(() -> finished(id, "The phone's voice failed")); }
                    @Override public void onError(String id, int code) { main.post(() -> finished(id, "The phone's voice failed (" + code + ")")); }
                    @Override public void onStop(String id, boolean interrupted) { main.post(() -> stopped(id)); }
                });
            }
            for (Runnable r : waitingForTts) r.run();
            waitingForTts.clear();
        }));
    }

    // ---- Status and permission

    private boolean onDeviceAvailable() {
        return Build.VERSION.SDK_INT >= 31 && SpeechRecognizer.isOnDeviceRecognitionAvailable(getContext());
    }

    @PluginMethod
    public void status(PluginCall call) {
        main.post(() -> {
            boolean local = onDeviceAvailable();
            boolean available = local || SpeechRecognizer.isRecognitionAvailable(getContext());
            JSObject result = new JSObject()
                    .put("available", available)
                    .put("local", local)
                    .put("engine", local ? "Android speech recognition (on this device)" : "Android speech recognition (its service may use the network)");
            if (!available) result.put("problem", "This phone has no speech recognition service. Install or enable one (Google's speech services) in Settings.");
            call.resolve(result);
        });
    }

    @PluginMethod
    public void start(PluginCall call) {
        if (getPermissionState("microphone") != PermissionState.GRANTED) {
            requestPermissionForAlias("microphone", call, "microphoneAnswered");
            return;
        }
        main.post(() -> begin(call));
    }

    @PermissionCallback
    private void microphoneAnswered(PluginCall call) {
        if (getPermissionState("microphone") != PermissionState.GRANTED) {
            call.reject("Voice mode needs the microphone. Allow it in Settings › Apps › twr › Permissions.");
            return;
        }
        main.post(() -> begin(call));
    }

    // ---- Recognition

    private void begin(PluginCall call) {
        if (getActivity() == null || !getActivity().getLifecycle().getCurrentState().isAtLeast(Lifecycle.State.RESUMED)) {
            call.reject("Voice mode works while twr is open");
            return;
        }
        if (!onDeviceAvailable() && !SpeechRecognizer.isRecognitionAvailable(getContext())) {
            call.reject("This phone has no speech recognition service. Install or enable one (Google's speech services) in Settings.");
            return;
        }
        teardown();
        mode = "hands-free".equals(call.getString("mode")) ? "hands-free" : "push";
        hints = new ArrayList<>();
        JSArray hint = call.getArray("hint", new JSArray());
        for (int i = 0; i < hint.length() && hints.size() < 64; i++) {
            String word = hint.optString(i, "");
            if (!word.isEmpty() && word.length() <= 64) hints.add(word);
        }
        quickFailures = 0;
        listening = true;
        try {
            listen();
        } catch (RuntimeException e) {
            teardown();
            call.reject("Speech recognition could not start: " + e.getMessage());
            return;
        }
        if ("push".equals(mode)) speechStarted();
        call.resolve();
    }

    private Intent intent() {
        Intent intent = new Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH);
        intent.putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM);
        intent.putExtra(RecognizerIntent.EXTRA_LANGUAGE, Locale.getDefault().toLanguageTag());
        intent.putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true);
        intent.putExtra(RecognizerIntent.EXTRA_PREFER_OFFLINE, true);
        intent.putExtra(RecognizerIntent.EXTRA_CALLING_PACKAGE, getContext().getPackageName());
        intent.putExtra(RecognizerIntent.EXTRA_MAX_RESULTS, 1);
        if ("hands-free".equals(mode)) {
            // Hints (shared/voice.ts `PAUSE_MS`): recognizers may keep their own endpointing.
            intent.putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_COMPLETE_SILENCE_LENGTH_MILLIS, END_SILENCE);
            intent.putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_POSSIBLY_COMPLETE_SILENCE_LENGTH_MILLIS, END_SILENCE);
        } else {
            intent.putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_COMPLETE_SILENCE_LENGTH_MILLIS, 10000L);
            intent.putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_POSSIBLY_COMPLETE_SILENCE_LENGTH_MILLIS, 10000L);
        }
        if (Build.VERSION.SDK_INT >= 33 && !hints.isEmpty())
            intent.putStringArrayListExtra(RecognizerIntent.EXTRA_BIASING_STRINGS, hints);
        return intent;
    }

    /** A recognizer listening for one utterance. */
    private void listen() {
        if (recognizer == null) {
            onDevice = onDeviceAvailable();
            recognizer = onDevice && Build.VERSION.SDK_INT >= 31
                    ? SpeechRecognizer.createOnDeviceSpeechRecognizer(getContext())
                    : SpeechRecognizer.createSpeechRecognizer(getContext());
        }
        final int pass = ++generation;
        partial = "";
        passStarted = System.currentTimeMillis();
        recognizer.setRecognitionListener(new RecognitionListener() {
            @Override public void onReadyForSpeech(Bundle params) {}
            @Override public void onBeginningOfSpeech() { if (pass == generation) speechStarted(); }
            @Override public void onRmsChanged(float rms) { if (pass == generation) level(rms); }
            @Override public void onBufferReceived(byte[] buffer) {}
            @Override public void onEndOfSpeech() {}
            @Override public void onError(int error) { if (pass == generation) failed(error); }
            @Override public void onResults(Bundle results) { if (pass == generation) result(text(results)); }
            @Override public void onPartialResults(Bundle results) {
                if (pass != generation) return;
                String text = text(results);
                if (!text.isEmpty() && !text.equals(partial)) {
                    partial = text;
                    quickFailures = 0;
                    speechStarted();
                    emit(event("partial").put("text", text));
                }
            }
            @Override public void onEvent(int type, Bundle params) {}
        });
        recognizer.startListening(intent());
    }

    private static String text(Bundle results) {
        ArrayList<String> found = results == null ? null : results.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION);
        return found == null || found.isEmpty() || found.get(0) == null ? "" : found.get(0).trim();
    }

    private void speechStarted() {
        if (heardSpeech) return;
        heardSpeech = true;
        emit(event("speechStart"));
    }

    private void level(float rms) {
        long now = System.currentTimeMillis();
        if (now - lastLevel < 100) return;
        lastLevel = now;
        emit(event("level").put("level", Math.max(0, Math.min(1, (rms + 2) / 12))));
    }

    /** One utterance recognized: its `final`, then (hands-free) the next one. */
    private void result(String text) {
        final String said = text.isEmpty() ? partial : text;
        heardSpeech = false;
        partial = "";
        emit(event("final").put("text", said));
        next(RESTART_AFTER_RESULT);
    }

    private void next(long delay) {
        final int pass = ++generation;
        if ("hands-free".equals(mode) && listening) {
            main.postDelayed(() -> {
                if (pass != generation || !listening) return;
                try { listen(); } catch (RuntimeException e) { stopWith("Speech recognition stopped: " + e.getMessage()); }
            }, delay);
        } else {
            listening = false;
            destroyRecognizer();
        }
    }

    private void failed(int error) {
        boolean silent = error == SpeechRecognizer.ERROR_NO_MATCH || error == SpeechRecognizer.ERROR_SPEECH_TIMEOUT;
        if (silent) {
            // Heard nothing usable: what partial text there was is the utterance; hands-free
            // listening goes on.
            if (heardSpeech || "push".equals(mode)) result(partial);
            else next(RESTART_AFTER_SILENCE);
            return;
        }
        if (error == SpeechRecognizer.ERROR_CLIENT && !listening) return;
        if ("hands-free".equals(mode) && (error == SpeechRecognizer.ERROR_RECOGNIZER_BUSY || error == SpeechRecognizer.ERROR_CLIENT)) {
            // Recognizers that were just restarted may be busy: three quick failures end it.
            if (System.currentTimeMillis() - passStarted < 1000 && ++quickFailures >= 3) {
                stopWith(describe(error));
                return;
            }
            if (heardSpeech) result(partial);
            else next(RESTART_WHEN_BUSY);
            return;
        }
        String message = describe(error);
        if (heardSpeech || "push".equals(mode)) {
            heardSpeech = false;
            emit(event("final").put("text", partial));
        }
        stopWith(message);
    }

    private static String describe(int error) {
        switch (error) {
            case SpeechRecognizer.ERROR_NETWORK:
            case SpeechRecognizer.ERROR_NETWORK_TIMEOUT:
            case SpeechRecognizer.ERROR_SERVER:
                return "Speech recognition needs its service's network here and could not reach it";
            case SpeechRecognizer.ERROR_AUDIO:
                return "The microphone could not record";
            case SpeechRecognizer.ERROR_INSUFFICIENT_PERMISSIONS:
                return "Voice mode needs the microphone. Allow it in Settings › Apps › twr › Permissions.";
            case SpeechRecognizer.ERROR_RECOGNIZER_BUSY:
                return "Speech recognition is busy with another app";
            case 12: // ERROR_LANGUAGE_NOT_SUPPORTED
            case 13: // ERROR_LANGUAGE_UNAVAILABLE
                return "Speech recognition does not have this phone's language; download it in Settings";
            default:
                return "Speech recognition stopped (error " + error + ")";
        }
    }

    private void stopWith(String message) {
        teardown();
        emit(event("error").put("message", message));
    }

    @PluginMethod
    public void stop(PluginCall call) {
        main.post(() -> {
            if (recognizer == null || !listening) {
                boolean was = listening;
                teardown();
                if (was) emit(event("final").put("text", ""));
                call.resolve();
                return;
            }
            final boolean push = "push".equals(mode);
            listening = false;
            final int pass = generation;
            recognizer.stopListening();
            if (push) {
                // Its result follows; a recognizer that never answers still ends the utterance.
                main.postDelayed(() -> { if (pass == generation) result(partial); }, FINAL_WAIT);
            } else {
                main.postDelayed(() -> { if (pass == generation) { if (heardSpeech) result(partial); else teardown(); } }, FINAL_WAIT);
            }
            call.resolve();
        });
    }

    @PluginMethod
    public void cancel(PluginCall call) {
        main.post(() -> {
            teardown();
            call.resolve();
        });
    }

    private void destroyRecognizer() {
        if (recognizer != null) {
            try { recognizer.cancel(); } catch (RuntimeException ignored) {}
            recognizer.destroy();
            recognizer = null;
        }
    }

    /** Stops listening and drops what was not recognized yet. */
    private void teardown() {
        generation++;
        listening = false;
        heardSpeech = false;
        partial = "";
        destroyRecognizer();
    }

    @Override
    protected void handleOnPause() {
        main.post(() -> {
            boolean active = listening || recognizer != null || speaking != null || clip != null;
            teardown();
            stopVoice();
            if (active) emit(event("error").put("message", "Voice mode stops while twr is in the background").put("background", true));
        });
    }

    @Override
    protected void handleOnDestroy() {
        main.post(() -> {
            teardown();
            stopPlayback();
            if (audioThread != null) audioThread.quitSafely();
            if (tts != null) tts.shutdown();
            tts = null;
        });
    }

    // ---- The phone's voice

    /** Runs {@code ready} once TextToSpeech has started, or rejects {@code call}. */
    private void withTts(PluginCall call, Runnable ready) {
        if (ttsReady && tts != null) { ready.run(); return; }
        if (ttsFailed || tts == null) { call.reject(NO_TTS); return; }
        final boolean[] done = {false};
        waitingForTts.add(() -> {
            if (done[0]) return;
            done[0] = true;
            if (ttsReady && tts != null) ready.run();
            else call.reject(NO_TTS);
        });
        main.postDelayed(() -> {
            if (done[0]) return;
            done[0] = true;
            call.reject("The phone's text-to-speech did not start in time");
        }, TTS_WAIT);
    }

    private static int rank(Voice voice) {
        if (voice.getQuality() >= Voice.QUALITY_VERY_HIGH) return 0;
        if (voice.getQuality() >= Voice.QUALITY_HIGH) return 1;
        return 2;
    }

    private static boolean installed(Voice voice) {
        Set<String> features = voice.getFeatures();
        return features == null || !features.contains(TextToSpeech.Engine.KEY_FEATURE_NOT_INSTALLED);
    }

    /**
     * What to show for a voice: its locale in the phone's language, and the short name the
     * engine tells its voices apart by ("en-us-x-iol-local" is "English (United States) · IOL").
     * A local voice and its network twin have the same name: {@code network} tells them apart.
     */
    private static String readableName(Voice voice) {
        String name = voice.getLocale().getDisplayName();
        String raw = voice.getName();
        String tag = null;
        int x = raw.indexOf("-x-");
        if (x >= 0) {
            String rest = raw.substring(x + 3);
            int dash = rest.indexOf('-');
            tag = dash >= 0 ? rest.substring(0, dash) : rest;
        } else if (!raw.endsWith("-language") && !raw.equalsIgnoreCase(voice.getLocale().toLanguageTag())) {
            tag = raw;
        }
        return tag == null || tag.isEmpty() ? name : name + " · " + tag.toUpperCase(Locale.ROOT);
    }

    /**
     * The phone's voices a reply may be read with, best first: very high, high, then the
     * other qualities, each by name. Voices that need the network (the text goes to the
     * engine's servers) or are not downloaded yet are listed and marked.
     */
    @PluginMethod
    public void voices(PluginCall call) {
        main.post(() -> withTts(call, () -> {
            List<Voice> found = new ArrayList<>();
            try {
                Set<Voice> all = tts.getVoices();
                if (all != null) for (Voice voice : all) if (voice != null && voice.getLocale() != null) found.add(voice);
            } catch (RuntimeException e) {
                call.reject("The phone's voices could not be listed: " + e.getMessage());
                return;
            }
            List<JSObject> entries = new ArrayList<>();
            List<String> names = new ArrayList<>();
            for (Voice voice : found) names.add(readableName(voice));
            List<Integer> order = new ArrayList<>();
            for (int i = 0; i < found.size(); i++) order.add(i);
            Collections.sort(order, (a, b) -> {
                int byRank = Integer.compare(rank(found.get(a)), rank(found.get(b)));
                if (byRank != 0) return byRank;
                int byName = names.get(a).compareToIgnoreCase(names.get(b));
                return byName != 0 ? byName : found.get(a).getName().compareTo(found.get(b).getName());
            });
            JSArray voices = new JSArray();
            for (int i : order) {
                Voice voice = found.get(i);
                int rank = rank(voice);
                voices.put(new JSObject()
                        .put("id", voice.getName())
                        .put("name", names.get(i))
                        .put("language", voice.getLocale().toLanguageTag())
                        .put("quality", rank == 0 ? "premium" : rank == 1 ? "enhanced" : "default")
                        .put("network", voice.isNetworkConnectionRequired())
                        .put("installed", installed(voice)));
            }
            call.resolve(new JSObject().put("voices", voices));
        }));
    }

    /**
     * The locale for {@code lang} ("en", "es", or a tag with its region like "es-MX"): a bare
     * language takes the phone's own region when the phone speaks it; none is the phone's own.
     */
    private static Locale locale(String lang) {
        Locale phone = Locale.getDefault();
        if (lang == null || lang.trim().isEmpty()) return phone;
        Locale wanted = Locale.forLanguageTag(lang.trim().replace('_', '-'));
        if (wanted.getLanguage().isEmpty()) return phone;
        if (wanted.getCountry().isEmpty() && wanted.getLanguage().equals(phone.getLanguage())) return phone;
        return wanted;
    }

    /**
     * Sets the voice for the next utterance every time, so one an earlier utterance named does
     * not stay: {@code voiceName} when the phone has it, else the default voice of {@code lang},
     * else of the phone's language.
     */
    private void chooseVoice(String voiceName, String lang) {
        if (voiceName != null && !voiceName.isEmpty()) {
            try {
                Set<Voice> all = tts.getVoices();
                if (all != null) for (Voice voice : all)
                    if (voice != null && voice.getName().equals(voiceName) && tts.setVoice(voice) == TextToSpeech.SUCCESS) return;
            } catch (RuntimeException ignored) {}
        }
        // setLanguage also picks that language's default voice.
        int set;
        try { set = tts.setLanguage(locale(lang)); } catch (RuntimeException e) { set = TextToSpeech.LANG_NOT_SUPPORTED; }
        if (set < TextToSpeech.LANG_AVAILABLE) {
            try { tts.setLanguage(Locale.getDefault()); } catch (RuntimeException ignored) {}
        }
    }

    /** Stops what the phone says or plays, at once and with no ack: it was replaced or dropped. */
    private void stopVoice() {
        speaking = null;
        if (tts != null && ttsReady) tts.stop();
        stopPlayback();
    }

    @PluginMethod
    public void speak(PluginCall call) {
        Integer id = call.getInt("id");
        String text = call.getString("text");
        if (id == null || text == null || text.isEmpty() || text.length() > 4096) {
            call.reject("An utterance needs an id and its text");
            return;
        }
        String voiceName = call.getString("voice");
        String lang = call.getString("lang");
        int rate = call.getInt("rate", 0);
        main.post(() -> {
            Runnable say = () -> {
                if (!ttsReady || tts == null) {
                    call.reject(NO_TTS);
                    return;
                }
                // One utterance at a time: a new one replaces what is still said or played.
                stopPlayback();
                chooseVoice(voiceName, lang);
                // About 180 words per minute is TextToSpeech's normal rate.
                tts.setSpeechRate(rate > 0 ? Math.max(0.3f, Math.min(3f, rate / 180f)) : 1f);
                speaking = id;
                int queued = tts.speak(text, TextToSpeech.QUEUE_FLUSH, null, String.valueOf(id));
                if (queued != TextToSpeech.SUCCESS) {
                    speaking = null;
                    call.reject("The phone's voice could not start");
                    return;
                }
                call.resolve();
            };
            if (ttsReady || ttsFailed) say.run();
            else waitingForTts.add(say);
        });
    }

    @PluginMethod
    public void stopSpeaking(PluginCall call) {
        Integer id = call.getInt("id");
        main.post(() -> {
            if (id == null || id.equals(speaking)) {
                speaking = null;
                if (tts != null && ttsReady) tts.stop();
            }
            if (clip != null && (id == null || id == clip.id)) stopPlayback();
            call.resolve();
        });
    }

    private void finished(String utterance, String error) {
        Integer id;
        try { id = Integer.valueOf(utterance); } catch (NumberFormatException e) { return; }
        if (!id.equals(speaking)) return;
        speaking = null;
        emit(error == null ? event("spoken").put("id", id) : event("speakError").put("id", id).put("message", error));
    }

    private void stopped(String utterance) {
        // Stopped on purpose: the gateway dropped it already, nothing is acked.
        if (String.valueOf(speaking).equals(utterance)) speaking = null;
    }

    // ---- The hub's voice

    /**
     * A chunk of an utterance a neural voice on the hub's computer made: {@code pcm} is base64
     * of signed 16-bit little-endian mono samples at {@code sampleRate}, {@code seq} its
     * position from 0, and {@code last} marks the utterance's final chunk. Chunks play back to
     * back; {@code spoken} follows once the last one has played, {@code speakError} when one is
     * missing or the audio fails.
     *
     * <p>A first chunk ({@code seq} 0) replaces whatever is said or played, with no ack for it,
     * and needs twr in the foreground. A later chunk of an utterance that was replaced or
     * stopped is ignored. A chunk that can't be played (malformed, over 2 MB, a new sample rate,
     * more than 3 minutes queued) is rejected, and its utterance stops without an event: the
     * rejection is its error. Decoding happens on the plugin's thread, writing on the audio
     * thread: the main thread never waits for audio.
     */
    @PluginMethod
    public void playAudio(PluginCall call) {
        Integer id = call.getInt("id");
        Integer seq = call.getInt("seq");
        if (id == null || seq == null || seq < 0) {
            call.reject("An audio chunk needs its utterance's id and its position");
            return;
        }
        int sampleRate = call.getInt("sampleRate", 0);
        if (sampleRate < 8000 || sampleRate > 96000) {
            call.reject("An audio chunk needs a sample rate from 8000 to 96000 Hz");
            return;
        }
        boolean last = Boolean.TRUE.equals(call.getBoolean("last", false));
        String encoded = call.getString("pcm", "");
        if (encoded == null) encoded = "";
        // Base64 is 4 characters for 3 bytes: refuse a chunk too big before decoding it.
        if (encoded.length() > (MAX_CHUNK / 3 + 1) * 4) {
            call.reject("An audio chunk may hold 2 MB at most");
            return;
        }
        final byte[] data;
        try {
            data = encoded.isEmpty() ? new byte[0] : Base64.decode(encoded, Base64.NO_WRAP);
        } catch (IllegalArgumentException e) {
            call.reject("The audio chunk is not base64");
            return;
        }
        if (data.length > MAX_CHUNK) {
            call.reject("An audio chunk may hold 2 MB at most");
            return;
        }
        if (data.length % 2 != 0) {
            call.reject("An audio chunk holds whole 16-bit samples");
            return;
        }
        main.post(() -> queueAudio(call, id, seq, sampleRate, data, last));
    }

    private boolean resumed() {
        return getActivity() != null && getActivity().getLifecycle().getCurrentState().isAtLeast(Lifecycle.State.RESUMED);
    }

    private void queueAudio(PluginCall call, int id, int seq, int sampleRate, byte[] data, boolean last) {
        long frames = data.length / 2;
        long limit = (long) MAX_QUEUED * sampleRate;
        final Clip c;
        if (seq == 0) {
            if (!resumed()) {
                call.reject("Voice mode works while twr is open");
                return;
            }
            if (frames > limit) {
                call.reject("An utterance may queue 3 minutes of audio at most");
                return;
            }
            // One utterance at a time: this one replaces what is still said or played.
            stopVoice();
            c = new Clip(id, sampleRate);
            clip = c;
            audio.post(() -> open(c));
        } else {
            // A chunk of an utterance that was replaced, stopped or has ended.
            if (clip == null || clip.id != id || clip.last) {
                call.resolve();
                return;
            }
            c = clip;
            if (seq != c.nextSeq) {
                fail(c, "A piece of the audio went missing");
                call.resolve();
                return;
            }
            if (sampleRate != c.sampleRate) {
                stopPlayback();
                call.reject("An utterance's audio keeps one sample rate");
                return;
            }
            if (c.frames - c.played + frames > limit) {
                stopPlayback();
                call.reject("An utterance may queue 3 minutes of audio at most");
                return;
            }
        }
        c.nextSeq = seq + 1;
        c.frames += frames;
        c.last = last;
        audio.post(() -> feed(c, data, last));
        call.resolve();
    }

    /** Stops the audio utterance at once, with no ack. */
    private void stopPlayback() {
        final Clip c = clip;
        clip = null;
        if (c != null && audio != null) audio.post(() -> close(c));
    }

    private void fail(Clip c, String message) {
        if (c != clip) return;
        stopPlayback();
        emit(event("speakError").put("id", c.id).put("message", message));
    }

    /** From the audio thread: {@code c} failed, unless it was replaced or stopped already. */
    private void failed(Clip c, String message) {
        close(c);
        main.post(() -> fail(c, message));
    }

    // Audio thread from here on.

    private void open(Clip c) {
        if (c.closed) return;
        try {
            int min = AudioTrack.getMinBufferSize(c.sampleRate, AudioFormat.CHANNEL_OUT_MONO, AudioFormat.ENCODING_PCM_16BIT);
            // About a quarter second: a small buffer starts playing sooner.
            int size = Math.max(min > 0 ? min : 0, c.sampleRate / 2);
            c.track = new AudioTrack.Builder()
                    .setAudioAttributes(new AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_ASSISTANT)
                            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH).build())
                    .setAudioFormat(new AudioFormat.Builder()
                            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                            .setSampleRate(c.sampleRate)
                            .setChannelMask(AudioFormat.CHANNEL_OUT_MONO).build())
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .setBufferSizeInBytes(size)
                    .build();
            if (c.track.getState() != AudioTrack.STATE_INITIALIZED) throw new IllegalStateException("not initialized");
            c.track.play();
        } catch (RuntimeException e) {
            failed(c, "The phone's audio could not start: " + e.getMessage());
        }
    }

    private void feed(Clip c, byte[] data, boolean last) {
        if (c.closed) return;
        if (data.length > 0) c.waiting.add(data);
        if (last) c.ending = true;
        // A pump already waiting for room in the track writes this too.
        if (!c.pumping) pump(c);
    }

    /**
     * Writes what fits without blocking, and comes back for the rest. Once the last chunk is
     * written, {@code stop} makes the track play out what it holds (a stream track that holds
     * less than its start threshold would never start), and {@code watch} waits for the end.
     */
    private void pump(Clip c) {
        c.pumping = false;
        if (c.closed || c.track == null) return;
        while (!c.waiting.isEmpty()) {
            byte[] chunk = c.waiting.peek();
            int n = c.track.write(chunk, c.offset, chunk.length - c.offset, AudioTrack.WRITE_NON_BLOCKING);
            if (n < 0) {
                failed(c, "The phone's audio failed (" + n + ")");
                return;
            }
            c.offset += n;
            c.written += n;
            if (c.offset < chunk.length) break;
            c.waiting.poll();
            c.offset = 0;
        }
        progress(c);
        if (!c.waiting.isEmpty()) {
            c.pumping = true;
            audio.postDelayed(() -> pump(c), PUMP_EVERY);
        } else if (c.ending) {
            try { c.track.stop(); } catch (IllegalStateException e) {
                failed(c, "The phone's audio failed: " + e.getMessage());
                return;
            }
            c.movedAt = System.currentTimeMillis();
            watch(c);
        }
    }

    private long head(Clip c) {
        return c.track.getPlaybackHeadPosition() & 0xFFFFFFFFL;
    }

    private void progress(Clip c) {
        c.played = Math.min(c.written / 2, head(c));
    }

    /** Waits until the track has played every frame written, then the utterance is spoken. */
    private void watch(Clip c) {
        if (c.closed) return;
        long head = head(c);
        long total = c.written / 2;
        long now = System.currentTimeMillis();
        if (head != c.lastHead) { c.lastHead = head; c.movedAt = now; }
        c.played = Math.min(total, head);
        boolean played = head >= total;
        // Some tracks reset their position once drained: a position that stops moving is the end,
        // unless it never moved for a clip long enough to have been heard.
        boolean stalled = now - c.movedAt > STALLED;
        if (!played && stalled && head == 0 && total > c.sampleRate / 2) {
            failed(c, "The phone's audio did not play");
            return;
        }
        if (played || stalled) {
            close(c);
            main.post(() -> {
                if (c != clip) return;
                clip = null;
                emit(event("spoken").put("id", c.id));
            });
            return;
        }
        audio.postDelayed(() -> watch(c), WATCH_EVERY);
    }

    private void close(Clip c) {
        if (c.closed) return;
        c.closed = true;
        c.waiting.clear();
        if (c.track != null) {
            try {
                c.track.pause();
                c.track.flush();
            } catch (IllegalStateException ignored) {}
            c.track.release();
            c.track = null;
        }
    }
}
