package tools.twr.mobile.dev;

import android.Manifest;
import android.content.Intent;
import android.media.AudioAttributes;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
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
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

/**
 * Voice mode on Android (docs/mobile.md, Voice): SpeechRecognizer for what the user says, and
 * TextToSpeech for the utterances the gateway's speaker sends.
 *
 * <p>Recognition is on the device with {@code createOnDeviceSpeechRecognizer} (API 31+) when
 * {@code isOnDeviceRecognitionAvailable}; otherwise the default recognizer is asked to prefer
 * offline, and the audio may go to its service ({@code status} says {@code local: false}).
 * Android has no continuous recognition: hands-free restarts the recognizer after each result,
 * no-match or speech timeout, after a short backoff, so a word said in that gap can be missed.
 * Foreground only: pausing the app ends recognition and the voice (an {@code error} event with
 * {@code background: true}). Every event is one {@code voice} listener call with a {@code type};
 * recognizer calls and state stay on the main thread.
 */
@CapacitorPlugin(
        name = "TwrVoice",
        permissions = {@Permission(alias = "microphone", strings = {Manifest.permission.RECORD_AUDIO})})
public class TwrVoicePlugin extends Plugin {
    private static final long RESTART_AFTER_RESULT = 250, RESTART_AFTER_SILENCE = 500, RESTART_WHEN_BUSY = 1000;
    private static final long FINAL_WAIT = 3000;
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

    private void emit(JSObject data) {
        notifyListeners("voice", data);
    }

    private static JSObject event(String type) {
        return new JSObject().put("type", type);
    }

    @Override
    public void load() {
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
            // Hints: recognizers may keep their own endpointing.
            intent.putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_COMPLETE_SILENCE_LENGTH_MILLIS, 900L);
            intent.putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_POSSIBLY_COMPLETE_SILENCE_LENGTH_MILLIS, 900L);
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
            boolean active = listening || recognizer != null || speaking != null;
            teardown();
            speaking = null;
            if (tts != null && ttsReady) tts.stop();
            if (active) emit(event("error").put("message", "Voice mode stops while twr is in the background").put("background", true));
        });
    }

    @Override
    protected void handleOnDestroy() {
        main.post(() -> {
            teardown();
            if (tts != null) tts.shutdown();
            tts = null;
        });
    }

    // ---- The phone's voice

    @PluginMethod
    public void speak(PluginCall call) {
        Integer id = call.getInt("id");
        String text = call.getString("text");
        if (id == null || text == null || text.isEmpty() || text.length() > 4096) {
            call.reject("An utterance needs an id and its text");
            return;
        }
        String voiceName = call.getString("voice");
        int rate = call.getInt("rate", 0);
        main.post(() -> {
            Runnable say = () -> {
                if (!ttsReady || tts == null) {
                    call.reject("This phone has no text-to-speech voice. Install one in Settings › Accessibility › Text-to-speech.");
                    return;
                }
                if (voiceName != null && !voiceName.isEmpty()) {
                    try {
                        for (Voice voice : tts.getVoices())
                            if (voice.getName().equals(voiceName)) { tts.setVoice(voice); break; }
                    } catch (RuntimeException ignored) {}
                }
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
}
