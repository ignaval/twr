package tools.twr.mobile.dev;

import android.content.Context;
import androidx.activity.result.ActivityResult;
import com.getcapacitor.annotation.ActivityCallback;
import com.getcapacitor.annotation.Permission;
import com.getcapacitor.annotation.PermissionCallback;
import com.google.zxing.integration.android.IntentIntegrator;
import com.google.zxing.integration.android.IntentResult;
import android.content.SharedPreferences;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.util.Base64;
import com.getcapacitor.*;
import com.getcapacitor.annotation.CapacitorPlugin;
import java.io.IOException;
import java.net.URI;
import java.security.KeyStore;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;
import javax.net.SocketFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManager;
import okhttp3.*;
import org.json.*;
import tools.twr.mobile.dev.pairing.Base64Url;
import tools.twr.mobile.dev.pairing.Failures;
import tools.twr.mobile.dev.pairing.HubMismatchException;
import tools.twr.mobile.dev.pairing.NetworkType;
import tools.twr.mobile.dev.pairing.PairingProof;
import tools.twr.mobile.dev.pairing.PinCapture;
import tools.twr.mobile.dev.pairing.PinnedHostnameVerifier;
import tools.twr.mobile.dev.pairing.PinnedTrustManager;
import tools.twr.mobile.dev.pairing.Pins;
import tools.twr.mobile.dev.pairing.Route;
import tools.twr.mobile.dev.pairing.RouteRace;
import tools.twr.mobile.dev.pairing.SavedRoutes;

/**
 * Tokens stay in Android Keystore protected storage and never cross the JS bridge.
 *
 * Two trust modes, never mixed (docs/design/phone-pairing.md): a hub paired by URL keeps system
 * trust; a hub paired by pin (a scanned version 2 invitation, or a typed address and code) is
 * reached only through a client that accepts its pinned key alone, on its saved routes. A saved
 * hub's mode is fixed by whether its record has a {@code pin}. The pin, proof and route logic
 * is in {@code tools.twr.mobile.dev.pairing}, plain Java with unit tests.
 */
@CapacitorPlugin(name="TwrTransport", permissions={@Permission(alias="camera",strings={android.Manifest.permission.CAMERA})})
public class TwrTransportPlugin extends Plugin {
    private static final String KEY="tools.twr.mobile.credentials";
    // Application heartbeats/watchdog observe frame progress; OkHttp's fixed
    // pong deadline mistakes TCP-buffered slow transfers for dead links.
    private final OkHttpClient http=new OkHttpClient.Builder().followRedirects(false).followSslRedirects(false).connectTimeout(15,TimeUnit.SECONDS).readTimeout(30,TimeUnit.SECONDS).pingInterval(0,TimeUnit.SECONDS).build();
    private volatile WebSocket socket;
    private final java.util.concurrent.atomic.AtomicLong connectionGeneration=new java.util.concurrent.atomic.AtomicLong();
    private String activeHub;
    private final java.util.concurrent.ExecutorService worker=java.util.concurrent.Executors.newSingleThreadExecutor();
    private static final String VAULT_RESET="Saved hubs couldn't be restored on this device; pair again.";
    /** The saved hubs could not be decrypted and were dropped: listHubs says so once. */
    private boolean vaultReset=false;

    /** The remembered route's head start before the others are dialed (Racing routes). */
    private static final long HEAD_START_MILLIS=250;
    /** How long a race waits for any route; each probe also has its own call timeout. */
    private static final long RACE_DEADLINE_MILLIS=20_000;
    private static final int PROBE_TIMEOUT_SECONDS=10;
    /** /health, /v1/pair/proof and /v1/pair answers are small; a longer one is refused unread. */
    private static final long SMALL_BODY_BYTES=64*1024;
    /** A scanned invitation: 2048 bytes of JSON, or the twr://pair# link to it (base64url, 4/3 longer). */
    private static final int MAX_SCANNED_CHARS=11+2731;
    /** Route probes, at most 16 at a time (a hub's route limit). */
    private final ExecutorService probes=Executors.newCachedThreadPool();
    /** Runs connect's race and socket setup off the plugin thread. */
    private final ExecutorService connector=Executors.newSingleThreadExecutor();
    /** Argon2id for typed pairing (64 MiB, about a second): never on the main or plugin thread. */
    private final ExecutorService kdf=Executors.newSingleThreadExecutor();
    /** One OkHttp client per pin; they share the dispatcher and pool, but never a connection (OkHttp keys connections by socket factory and verifier). */
    private final Map<String,OkHttpClient> pinnedClients=new HashMap<>();
    /** The race connect is running, cancelled by the next connect, disconnect or removeHub. */
    private RouteRace<Route> race;
    private ConnectivityManager.NetworkCallback networkCallback;
    private Network defaultNetwork;
    private NetworkType networkType;

    /** A pairing failure for the user, with a reject code (PIN_MISMATCH, BAD_CODE, TIMEOUT, UNREACHABLE…). */
    private static final class PairFailure extends Exception {
        final String code;
        PairFailure(String message,String code){super(message);this.code=code;}
    }
    /** A non-2xx answer, with its (bounded) body. */
    private static final class HttpStatus extends IOException {
        final int status;final String body;
        HttpStatus(int status,String body){super("HTTP "+status);this.status=status;this.body=body;}
    }

    /** A Keystore key that is gone or unusable, or data a different key encrypted. */
    private static boolean unrecoverable(Throwable error) {
        for(Throwable cause=error;cause!=null;cause=cause.getCause()) {
            if(cause instanceof javax.crypto.AEADBadTagException||cause instanceof java.security.UnrecoverableKeyException||cause instanceof android.security.keystore.KeyPermanentlyInvalidatedException)return true;
        }
        return false;
    }
    private SecretKey key() throws Exception {
        KeyStore store=KeyStore.getInstance("AndroidKeyStore");store.load(null);
        if(store.containsAlias(KEY)) {
            try {
                return (SecretKey)store.getKey(KEY,null);
            } catch(java.security.UnrecoverableKeyException unusable) {
                // What it encrypted can't be read any more: start again with a new key.
                store.deleteEntry(KEY);
            }
        }
        KeyGenerator generator=KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES,"AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(KEY,KeyProperties.PURPOSE_ENCRYPT|KeyProperties.PURPOSE_DECRYPT).setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).build());
        return generator.generateKey();
    }
    private SharedPreferences prefs(){return getContext().getSharedPreferences("twr-private",Context.MODE_PRIVATE);}
    private synchronized JSONObject vault() throws Exception {
        String encoded=prefs().getString("hubs",null);if(encoded==null)return new JSONObject();
        byte[] bytes=Base64.decode(encoded,Base64.NO_WRAP);
        try {
            Cipher cipher=Cipher.getInstance("AES/GCM/NoPadding");cipher.init(Cipher.DECRYPT_MODE,key(),new GCMParameterSpec(128,bytes,0,12));
            return new JSONObject(new String(cipher.doFinal(bytes,12,bytes.length-12),java.nio.charset.StandardCharsets.UTF_8));
        } catch(Exception error) {
            // Restored from another phone, or its Keystore key was lost: the tokens can never
            // be decrypted here. Drop them so pairing works again instead of failing forever.
            if(!unrecoverable(error))throw error;
            if(!prefs().edit().remove("hubs").commit())throw new Exception(VAULT_RESET,error);
            vaultReset=true;
            return new JSONObject();
        }
    }
    private synchronized void save(JSONObject data) throws Exception {
        Cipher cipher=Cipher.getInstance("AES/GCM/NoPadding");cipher.init(Cipher.ENCRYPT_MODE,key());
        byte[] encrypted=cipher.doFinal(data.toString().getBytes(java.nio.charset.StandardCharsets.UTF_8));
        byte[] result=new byte[12+encrypted.length];System.arraycopy(cipher.getIV(),0,result,0,12);System.arraycopy(encrypted,0,result,12,encrypted.length);
        if(!prefs().edit().putString("hubs",Base64.encodeToString(result,Base64.NO_WRAP)).commit())throw new Exception("Could not save paired hub");
    }
    private String endpoint(String input) throws Exception {
        URI uri=new URI(input);String scheme=uri.getScheme();String host=uri.getHost();
        boolean local=BuildConfig.DEBUG&&("localhost".equals(host)||"127.0.0.1".equals(host)||"::1".equals(host));
        if(!("https".equals(scheme)||("http".equals(scheme)&&local))||uri.getUserInfo()!=null||uri.getQuery()!=null||uri.getFragment()!=null||host==null||!(uri.getPath().isEmpty()||uri.getPath().equals("/")))throw new Exception("Use an HTTPS hub URL. Debug builds also allow loopback for local tests.");
        return input.replaceAll("/+$","");
    }
    /** What JS sees of a saved hub: never the token. Pinned hubs add pin, routes and route. */
    private JSObject metadata(String id,JSONObject hub) throws JSONException {
        JSObject data=new JSObject();data.put("id",id);data.put("name",hub.optString("name"));data.put("url",hub.optString("url"));data.put("scopes",hub.optJSONArray("scopes"));data.put("gateway_id",hub.optString("gateway_id"));
        if(hub.has("pin")) {
            SavedRoutes saved=readRoutes(hub);
            data.put("pin",hub.getString("pin"));
            data.put("routes",new JSArray(saved.routes()));
            if(saved.lastRoute()!=null)data.put("route",saved.lastRoute());
        }
        return data;
    }

    // Saved routes, in the vault record: "routes": [{"route","seen"}], "last": {network: route}, "route".
    private static SavedRoutes readRoutes(JSONObject hub) {
        SavedRoutes saved=new SavedRoutes();
        JSONArray routes=hub.optJSONArray("routes");
        if(routes!=null)for(int i=0;i<routes.length();i++) {
            JSONObject entry=routes.optJSONObject(i);
            if(entry!=null)saved.restore(entry.optString("route",""),entry.optLong("seen",0));
        }
        JSONObject last=hub.optJSONObject("last");
        if(last!=null) {
            java.util.Iterator<String> networks=last.keys();
            while(networks.hasNext()){String network=networks.next();saved.restoreLast(network,last.optString(network,null));}
        }
        saved.restoreLastRoute(hub.optString("route",null));
        saved.prune();
        return saved;
    }
    private static void writeRoutes(JSONObject hub,SavedRoutes saved) throws JSONException {
        JSONArray routes=new JSONArray();
        for(SavedRoutes.Entry entry:saved.entries())routes.put(new JSONObject().put("route",entry.route).put("seen",entry.seen));
        hub.put("routes",routes);
        JSONObject last=new JSONObject();
        for(Map.Entry<String,String> entry:saved.lastByNetwork().entrySet())last.put(entry.getKey(),entry.getValue());
        hub.put("last",last);
        if(saved.lastRoute()!=null)hub.put("route",saved.lastRoute());else hub.remove("route");
    }
    private static List<String> strings(JSONArray array) {
        List<String> out=new ArrayList<>();
        if(array!=null)for(int i=0;i<array.length();i++){Object value=array.opt(i);if(value instanceof String)out.add((String)value);}
        return out;
    }

    // Network: the default network's type, and the "network" event when it changes.
    @Override public void load() {
        ConnectivityManager connectivity=(ConnectivityManager)getContext().getSystemService(Context.CONNECTIVITY_SERVICE);
        if(connectivity==null)return;
        ConnectivityManager.NetworkCallback callback=new ConnectivityManager.NetworkCallback() {
            @Override public void onAvailable(Network network){networkChanged(network,connectivity.getNetworkCapabilities(network));}
            @Override public void onCapabilitiesChanged(Network network,NetworkCapabilities capabilities){networkChanged(network,capabilities);}
            // A default-network callback only loses the network it last made available.
            @Override public void onLost(Network network){networkChanged(null,null);}
        };
        try {
            connectivity.registerDefaultNetworkCallback(callback);
            networkCallback=callback;
        } catch(RuntimeException unavailable) {
            // No ACCESS_NETWORK_STATE, or too many callbacks: no event; connect still reads the type.
        }
    }
    private static NetworkType typeOf(NetworkCapabilities capabilities) {
        if(capabilities==null)return NetworkType.OTHER;
        return NetworkType.of(true,capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI),capabilities.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR));
    }
    private void networkChanged(Network network,NetworkCapabilities capabilities) {
        NetworkType type=network==null?NetworkType.NONE:typeOf(capabilities);
        synchronized(this) {
            if(java.util.Objects.equals(network,defaultNetwork)&&type==networkType)return;
            defaultNetwork=network;networkType=type;
        }
        notifyListeners("network",new JSObject().put("type",type.wire));
    }
    private NetworkType currentNetworkType() {
        ConnectivityManager connectivity=(ConnectivityManager)getContext().getSystemService(Context.CONNECTIVITY_SERVICE);
        if(connectivity==null)return NetworkType.OTHER;
        try {
            Network network=connectivity.getActiveNetwork();
            return network==null?NetworkType.NONE:typeOf(connectivity.getNetworkCapabilities(network));
        } catch(RuntimeException unavailable) {
            return NetworkType.OTHER;
        }
    }

    // Pinned transport.
    /** The client for a pinned hub: its key and nothing else, TLS 1.3 preferred. */
    private synchronized OkHttpClient pinnedClient(String pin) throws java.security.GeneralSecurityException {
        OkHttpClient client=pinnedClients.get(pin);
        if(client!=null)return client;
        PinnedTrustManager trust=new PinnedTrustManager(pin);
        SSLContext context=SSLContext.getInstance("TLS");
        context.init(null,new TrustManager[]{trust},null);
        ConnectionSpec spec=new ConnectionSpec.Builder(ConnectionSpec.MODERN_TLS).tlsVersions(TlsVersion.TLS_1_3,TlsVersion.TLS_1_2).build();
        client=http.newBuilder().sslSocketFactory(context.getSocketFactory(),trust).hostnameVerifier(new PinnedHostnameVerifier(trust)).connectionSpecs(Collections.singletonList(spec)).connectTimeout(PROBE_TIMEOUT_SECONDS,TimeUnit.SECONDS).build();
        pinnedClients.put(pin,client);
        return client;
    }
    private static String smallBody(Response response) throws IOException {
        ResponseBody body=response.body();
        if(body==null)return "";
        okio.BufferedSource source=body.source();
        if(source.request(SMALL_BODY_BYTES+1))throw new IOException("response too large");
        return source.getBuffer().readUtf8();
    }
    private static JSONObject fetchJson(Call call) throws IOException {
        try(Response response=call.execute()) {
            String text=smallBody(response);
            if(!response.isSuccessful())throw new HttpStatus(response.code(),text);
            try {return new JSONObject(text);} catch(JSONException malformed) {throw new HubMismatchException("not a twr gateway (malformed answer)");}
        }
    }
    private static Call get(OkHttpClient client,String url) {
        Call call=client.newCall(new Request.Builder().url(url).build());
        call.timeout().timeout(PROBE_TIMEOUT_SECONDS,TimeUnit.SECONDS);
        return call;
    }
    /** Wins when the route passes the pin check (the client) and its /health names {@code gatewayId}. */
    private static RouteRace.Probe<Route> healthProbe(OkHttpClient client,String gatewayId) {
        return (route,attempt)->{
            Call call=get(client,route.origin()+"/health");
            attempt.onCancel(call::cancel);
            JSONObject health;
            try {health=fetchJson(call);} catch(HttpStatus status) {throw new HubMismatchException("not a twr gateway (HTTP "+status.status+")");}
            String answered=health.optString("gateway_id","");
            if(!gatewayId.equals(answered))throw new HubMismatchException(answered.isEmpty()?"not a twr gateway":"another hub answered (gateway "+answered+")");
            return route;
        };
    }
    private PairFailure raceFailure(RouteRace.Failed failed,String hub) {
        return new PairFailure("Could not reach "+hub+": "+Failures.summary(failed.failures,failed.skipped,hub),Failures.code(failed.failures));
    }
    private static PairFailure routeFailure(Route route,Throwable error,String hub) {
        String code=Failures.pinMismatch(error)?Failures.PIN_MISMATCH:Failures.timedOut(error)?Failures.TIMEOUT:Failures.UNREACHABLE;
        return new PairFailure(route+" "+Failures.reason(error,hub),code);
    }

    private final java.util.concurrent.atomic.AtomicBoolean scanning=new java.util.concurrent.atomic.AtomicBoolean();
    @PluginMethod public void scanInvitation(PluginCall call) {
        if(!scanning.compareAndSet(false,true)){call.reject("The QR scanner is already open","SCAN_BUSY");return;}
        if(!getContext().getPackageManager().hasSystemFeature(android.content.pm.PackageManager.FEATURE_CAMERA_ANY)){scanning.set(false);call.reject("This device has no available camera. Enter the address and code manually.","CAMERA_UNAVAILABLE");return;}
        if(getPermissionState("camera")!=PermissionState.GRANTED){requestPermissionForAlias("camera",call,"cameraPermission");return;}
        launchScanner(call);
    }
    @PermissionCallback private void cameraPermission(PluginCall call) {
        if(getPermissionState("camera")!=PermissionState.GRANTED){scanning.set(false);call.reject("Camera permission was denied. Allow it in Settings or enter the address and code manually.","CAMERA_PERMISSION_DENIED");return;}
        launchScanner(call);
    }
    private void launchScanner(PluginCall call) {
        getActivity().runOnUiThread(()->{
            try {
                android.content.Intent intent=new IntentIntegrator(getActivity()).setDesiredBarcodeFormats(IntentIntegrator.QR_CODE).setPrompt("Scan the one-use invitation from twr on your computer").setBeepEnabled(false).setOrientationLocked(false).createScanIntent();
                startActivityForResult(call,intent,"invitationScanned");
            } catch(Exception error) { scanning.set(false);call.reject("Could not open the camera. Enter the address and code manually.","CAMERA_UNAVAILABLE",error); }
        });
    }
    @ActivityCallback private void invitationScanned(PluginCall call, ActivityResult result) {
        scanning.set(false);if(call==null)return;
        IntentResult scanned=IntentIntegrator.parseActivityResult(IntentIntegrator.REQUEST_CODE,result.getResultCode(),result.getData());
        if(scanned==null||scanned.getContents()==null){call.reject("QR scan cancelled","SCAN_CANCELLED");return;}
        String text=scanned.getContents();
        if(text.length()>MAX_SCANNED_CHARS){call.reject("Scan a twr pairing invitation");return;}
        call.resolve(new JSObject().put("invitation",text));
    }
    @PluginMethod public void listHubs(PluginCall call){try{JSONObject data=vault();JSArray hubs=new JSArray();java.util.Iterator<String> keys=data.keys();while(keys.hasNext()){String id=keys.next();hubs.put(metadata(id,data.getJSONObject(id)));}JSObject value=new JSObject();value.put("hubs",hubs);synchronized(this){if(vaultReset){vaultReset=false;value.put("notice",VAULT_RESET);}}call.resolve(value);}catch(Exception e){call.reject("Could not read paired hubs",e);}}

    /**
     * Three modes (docs/design/phone-pairing.md, Client API): {@code pin} (a scanned version 2
     * invitation), {@code address} (typed, with the pin proof), else {@code url} (certificate mode).
     */
    @PluginMethod public void pair(PluginCall call){worker.execute(()->{try{
        String pin=call.getString("pin","");String address=call.getString("address","");
        JSObject hub;
        if(!pin.isEmpty())hub=pairScanned(call,pin);
        else if(!address.isEmpty())hub=pairTyped(call,address);
        else hub=pairCertificate(call);
        call.resolve(hub);
    }catch(PairFailure e){call.reject(e.getMessage(),e.code);}catch(Exception e){call.reject(e.getMessage(),e);}});}

    private void ensureNotPaired(String gateway) throws Exception {
        JSONObject existing=vault();java.util.Iterator<String> keys=existing.keys();
        while(keys.hasNext()) {
            JSONObject saved=existing.getJSONObject(keys.next());
            if(!gateway.isEmpty()&&gateway.equals(saved.optString("gateway_id")))throw new Exception("This hub is already paired. Use Revoke this device in its settings, or Forget this hub under Device access if it cannot connect, then pair again.");
        }
    }
    /** POST /v1/pair; the response is the record to save (token, gateway_id, scopes…). */
    private static JSONObject postPair(OkHttpClient client,String base,String code,String name,String failurePrefix) throws Exception {
        JSONObject body=new JSONObject();body.put("code",code);body.put("name",name);
        Request request=new Request.Builder().url(base+"/v1/pair").post(RequestBody.create(body.toString(),MediaType.get("application/json"))).build();
        try(Response response=client.newCall(request).execute()){
            String responseText=smallBody(response);
            if(!response.isSuccessful()) {
                String detail="Pairing failed. Check the code and hub URL.";
                try {detail=new JSONObject(responseText).optString("error",detail);}catch(JSONException ignored){}
                detail=failurePrefix+detail;
                throw new PairFailure(detail.substring(0,Math.min(detail.length(),240)),response.code()==429?"RATE_LIMITED":"PAIR_REFUSED");
            }
            JSONObject record=new JSONObject(responseText);
            if(record.getString("token").length()!=64)throw new Exception("Invalid hub credentials");
            return record;
        }
    }
    private JSObject store(JSONObject record) throws Exception {
        String id=java.util.UUID.randomUUID().toString();
        synchronized(this){JSONObject all=vault();all.put(id,record);save(all);vaultReset=false;}
        return metadata(id,record);
    }

    /** Certificate mode, version 1: today's behaviour, system trust. */
    private JSObject pairCertificate(PluginCall call) throws Exception {
        String url=endpoint(call.getString("url",""));
        try(Response health=http.newCall(new Request.Builder().url(url+"/health").build()).execute()) {
            if(!health.isSuccessful()||health.body()==null)throw new Exception("Could not identify the hub");
            ensureNotPaired(new JSONObject(smallBody(health)).optString("gateway_id",""));
        }
        JSONObject record=postPair(http,url,call.getString("code",""),call.getString("name",""),"");
        // The mode is how it was paired: a field in the answer never makes this a pinned hub.
        for(String pinned:new String[]{"pin","iroh","routes","last","route"})record.remove(pinned);
        record.put("url",url);record.put("name",call.getString("hubName","My hub"));
        return store(record);
    }

    /** A scanned version 2 invitation: race its routes with the pin check, then pair on the winner. */
    private JSObject pairScanned(PluginCall call,String pin) throws Exception {
        String hubName=call.getString("hubName","My hub");
        String gatewayId=call.getString("gatewayId","");
        if(!Pins.valid(pin))throw new PairFailure("This invitation's key is malformed. Scan it again.","BAD_INVITATION");
        if(gatewayId.isEmpty())throw new PairFailure("This invitation names no hub. Scan it again.","BAD_INVITATION");
        String code=PairingProof.normalizeCode(call.getString("code",""));
        if(code==null)throw new PairFailure("This invitation's code is malformed. Scan it again.",Failures.BAD_CODE);
        long now=System.currentTimeMillis();
        SavedRoutes saved=new SavedRoutes();
        saved.merge(strings(call.getArray("routes")),now);
        ensureNotPaired(gatewayId);
        NetworkType network=currentNetworkType();
        OkHttpClient client=pinnedClient(pin);
        RouteRace.Won<Route> won;
        try {
            won=new RouteRace<>(probes,healthProbe(client,gatewayId)).run(saved.plan(network),HEAD_START_MILLIS,RACE_DEADLINE_MILLIS);
        } catch(RouteRace.Failed failed) {
            throw raceFailure(failed,hubName);
        }
        JSONObject record;
        try {
            record=postPair(client,won.route.origin(),code,call.getString("name",""),won.route+": ");
        } catch(IOException lost) {
            throw routeFailure(won.route,lost,hubName);
        }
        return storePinned(record,pin,gatewayId,call.getString("iroh",""),hubName,saved,won.route,network);
    }

    /**
     * Typed pairing, with the proof: note the key the address presents (refusing that
     * handshake), ask /health for the gateway id and /v1/pair/proof for a proof over a fresh
     * nonce, both on connections pinned to that key, check the proof with the typed code, and
     * only then send the code.
     */
    private JSObject pairTyped(PluginCall call,String address) throws Exception {
        String hubName=call.getString("hubName","My hub");
        Route route;
        try {route=Route.fromAddress(address);} catch(IllegalArgumentException bad) {throw new PairFailure("Check the hub's address ("+bad.getMessage()+").","BAD_ADDRESS");}
        String code=PairingProof.normalizeCode(call.getString("code",""));
        if(code==null)throw new PairFailure("Enter the 12-character code twr shows on the computer.",Failures.BAD_CODE);
        // The code's Argon2id runs while the network steps do.
        Future<byte[]> proofKey=kdf.submit(()->PairingProof.proofKey(code));
        try {
            String pin;
            try {pin=PinCapture.capture(SocketFactory.getDefault(),route.host,route.port,PROBE_TIMEOUT_SECONDS*1000);}
            catch(IOException unreachable) {throw routeFailure(route,unreachable,hubName);}
            OkHttpClient client=pinnedClient(pin);
            JSONObject health;
            try {health=fetchJson(get(client,route.origin()+"/health"));}
            catch(HttpStatus status) {throw new PairFailure(route+" isn't a twr gateway (HTTP "+status.status+").",Failures.UNREACHABLE);}
            catch(IOException lost) {throw routeFailure(route,lost,hubName);}
            String gatewayId=health.optString("gateway_id","");
            if(gatewayId.isEmpty())throw new PairFailure(route+" isn't a twr gateway.",Failures.UNREACHABLE);
            if(!health.has("pin"))throw new PairFailure(route+" uses a certificate: pair with its https:// address instead.","CERTIFICATE_MODE");
            if(!pin.equals(health.optString("pin")))throw new PairFailure(route+" "+Failures.PIN_MISMATCH+": its TLS key isn't the hub's own, so another device is in the way, not "+hubName+".",Failures.PIN_MISMATCH);
            ensureNotPaired(gatewayId);
            byte[] nonce=new byte[PairingProof.NONCE_BYTES];new SecureRandom().nextBytes(nonce);
            JSONObject answer;
            try {answer=fetchJson(get(client,route.origin()+"/v1/pair/proof?nonce="+Base64Url.encode(nonce)));}
            catch(HttpStatus status) {
                if(status.status==404)throw new PairFailure("No pairing invitation is open on "+hubName+" ("+route+"). Start pairing on the computer again.","NO_INVITATION");
                if(status.status==429)throw new PairFailure(route+": too many attempts. Wait a minute and try again.","RATE_LIMITED");
                throw new PairFailure(route+" refused the proof request (HTTP "+status.status+").",Failures.UNREACHABLE);
            }
            catch(IOException lost) {throw routeFailure(route,lost,hubName);}
            byte[] proof;
            try {proof=Base64Url.decode(answer.optString("proof",""));} catch(IllegalArgumentException malformed) {proof=new byte[0];}
            byte[] key=awaitProofKey(proofKey);
            boolean proven=PairingProof.verify(key,Pins.decodePin(pin),nonce,gatewayId,proof);
            java.util.Arrays.fill(key,(byte)0);
            if(!proven)throw new PairFailure(route+": the code doesn't match. Check it; if it is right, this is another device, not "+hubName+".",Failures.BAD_CODE);
            JSONObject record;
            try {record=postPair(client,route.origin(),code,call.getString("name",""),route+": ");}
            catch(IOException lost) {throw routeFailure(route,lost,hubName);}
            SavedRoutes saved=new SavedRoutes();
            saved.merge(Collections.singletonList(route.toString()),System.currentTimeMillis());
            return storePinned(record,pin,gatewayId,"",hubName,saved,route,currentNetworkType());
        } finally {
            proofKey.cancel(false);
        }
    }
    private static byte[] awaitProofKey(Future<byte[]> proofKey) throws Exception {
        try {
            return proofKey.get(60,TimeUnit.SECONDS);
        } catch(ExecutionException failed) {
            if(failed.getCause() instanceof OutOfMemoryError)throw new PairFailure("This phone couldn't spare the 64 MiB that checking a typed code takes. Scan the QR code instead.","KDF_FAILED");
            throw new PairFailure("Couldn't check the code on this phone. Scan the QR code instead.","KDF_FAILED");
        } catch(java.util.concurrent.TimeoutException slow) {
            throw new PairFailure("Checking the code took too long. Scan the QR code instead.","KDF_FAILED");
        }
    }
    private JSObject storePinned(JSONObject record,String pin,String gatewayId,String iroh,String hubName,SavedRoutes saved,Route winner,NetworkType network) throws Exception {
        String answered=record.optString("gateway_id","");
        if(answered.isEmpty())record.put("gateway_id",gatewayId);
        else if(!answered.equals(gatewayId))throw new PairFailure(winner+": another hub answered (gateway "+answered+").",Failures.UNREACHABLE);
        record.put("pin",pin);
        if(!iroh.isEmpty())record.put("iroh",iroh);
        saved.connected(winner,network,System.currentTimeMillis());
        writeRoutes(record,saved);
        record.put("url",winner.origin());
        record.put("name",hubName);
        return store(record);
    }

    private synchronized void cancelRace(){if(race!=null){race.cancel();race=null;}}
    @PluginMethod public void connect(PluginCall call){try{
        final long generation=connectionGeneration.incrementAndGet();
        cancelRace();
        if(socket!=null)socket.close(1000,"Switching hub");
        String id=call.getString("id","");JSONObject hub=vault().getJSONObject(id);
        activeHub=id;
        String token=hub.getString("token");
        if(!hub.has("pin")){openSocket(call,generation,http,endpoint(hub.getString("url")).replaceFirst("^http","ws")+"/v1/socket",token);return;}
        // Pinned: race the saved routes, then open the socket on the winner with the same pin check.
        String pin=hub.getString("pin");String gatewayId=hub.optString("gateway_id","");String name=hub.optString("name","the hub");
        SavedRoutes saved=readRoutes(hub);
        NetworkType network=currentNetworkType();
        OkHttpClient client=pinnedClient(pin);
        RouteRace<Route> attempt=new RouteRace<>(probes,healthProbe(client,gatewayId));
        synchronized(this){race=attempt;}
        connector.execute(()->{
            try {
                RouteRace.Won<Route> won=attempt.run(saved.plan(network),HEAD_START_MILLIS,RACE_DEADLINE_MILLIS);
                if(generation!=connectionGeneration.get()){call.reject("Connection superseded");return;}
                rememberWinner(id,pin,won.route,network);
                openSocket(call,generation,client,won.route.origin().replaceFirst("^https","wss")+"/v1/socket",token);
            } catch(RouteRace.Failed failed) {
                if(failed.cancelled||generation!=connectionGeneration.get()){call.reject("Connection superseded");return;}
                PairFailure failure=raceFailure(failed,name);
                call.reject(failure.getMessage(),failure.code);
                transportClosed(generation,failure.getMessage());
            } catch(Exception e) {
                call.reject("Could not connect to the paired hub",e);
            }
        });
    }catch(Exception e){call.reject("Could not connect to the paired hub",e);}}
    /** The winner becomes {@code last} for this network and the display URL; a failure to save doesn't stop the connection. */
    private void rememberWinner(String id,String pin,Route winner,NetworkType network) {
        try {
            synchronized(this) {
                JSONObject all=vault();
                JSONObject hub=all.optJSONObject(id);
                if(hub==null||!pin.equals(hub.optString("pin")))return;
                SavedRoutes saved=readRoutes(hub);
                saved.connected(winner,network,System.currentTimeMillis());
                writeRoutes(hub,saved);
                hub.put("url",winner.origin());
                save(all);
            }
        } catch(Exception ignored) {
            // The next race starts without the head start; nothing else depends on it.
        }
    }
    private void transportClosed(long generation,String error){if(generation!=connectionGeneration.get())return;JSObject value=new JSObject();value.put("message",new JSObject().put("type","transport-closed").put("connected",false).put("error",error));notifyListeners("message",value);}
    private void openSocket(PluginCall call,long generation,OkHttpClient client,String url,String token) {
        WebSocket opened=client.newWebSocket(new Request.Builder().url(url).header("Authorization","Bearer "+token).header("X-Twr-Framing","1").build(),new WebSocketListener(){
            private boolean authenticated=false;
            @Override public void onOpen(WebSocket ws,Response response){}
            @Override public void onMessage(WebSocket ws,String text){if(generation!=connectionGeneration.get())return;try{if(text.length()>16*1024*1024)throw new Exception("Hub response too large");JSObject message=new JSObject(text);if("authenticated".equals(message.optString("type"))){authenticated=true;call.resolve();}JSObject value=new JSObject();value.put("message",message);notifyListeners("message",value);}catch(Exception e){ws.cancel();}}
            @Override public void onClosed(WebSocket ws,int code,String reason){if(!authenticated)call.reject("Hub authentication failed");closed(ws);}
            // Only 401 means the gateway no longer knows this token. A 403 and 5xx are
            // refusals the app retries like any other lost connection.
            @Override public void onFailure(WebSocket ws,Throwable error,Response response){boolean revoked=response!=null&&response.code()==401;boolean refused=!authenticated&&response!=null&&response.code()!=101;String message=revoked?"AUTH_REVOKED: This device no longer has access to the hub. Forget it and pair again.":refused?"Gateway refused the connection ("+response.code()+")":Failures.pinMismatch(error)?Failures.PIN_MISMATCH+": this is another device, not the paired hub":"Remote hub connection closed";if(!authenticated)call.reject(message,revoked?"AUTH_REVOKED":Failures.pinMismatch(error)?Failures.PIN_MISMATCH:null);closed(ws,message);}
            private void closed(WebSocket ws){closed(ws,"Remote hub connection closed");}
            private void closed(WebSocket ws,String error){transportClosed(generation,error);}
        });
        if(generation==connectionGeneration.get())socket=opened;else opened.cancel();
    }
    @PluginMethod public void send(PluginCall call){String message=call.getString("message","");if(message.getBytes(java.nio.charset.StandardCharsets.UTF_8).length>2*1024*1024){call.reject("Request exceeds 2 MiB","REQUEST_TOO_LARGE");return;}if(socket==null||!socket.send(message)){call.reject("Hub connection is closed");}else call.resolve();}
    @PluginMethod public void disconnect(PluginCall call){connectionGeneration.incrementAndGet();cancelRace();if(socket!=null){socket.close(1000,"App disconnected");socket=null;}call.resolve();}
    @PluginMethod public void removeHub(PluginCall call){try{String id=call.getString("id","");synchronized(this){JSONObject all=vault();all.remove(id);save(all);}if(id.equals(activeHub)){connectionGeneration.incrementAndGet();cancelRace();if(socket!=null)socket.cancel();socket=null;}call.resolve();}catch(Exception e){call.reject("Could not remove hub",e);}}

    /**
     * Merges the routes a pinned hub's gateway reports (its hello's {@code routes}) into the
     * saved hub: validated, deduplicated, at most 16, unknown kinds kept but never dialed. A
     * certificate-mode hub has no routes, so nothing changes for one.
     */
    @PluginMethod public void updateRoutes(PluginCall call){try{
        String id=call.getString("id","");JSArray reported=call.getArray("routes");
        if(reported==null){call.reject("routes must be a list of strings","BAD_ROUTES");return;}
        if(reported.length()>64){call.reject("Too many routes","BAD_ROUTES");return;}
        synchronized(this) {
            JSONObject all=vault();JSONObject hub=all.optJSONObject(id);
            if(hub==null){call.reject("This hub isn't paired","UNKNOWN_HUB");return;}
            if(hub.has("pin")) {
                SavedRoutes saved=readRoutes(hub);
                saved.merge(strings(reported),System.currentTimeMillis());
                writeRoutes(hub,saved);
                save(all);
            }
        }
        call.resolve();
    }catch(Exception e){call.reject("Could not update the hub's routes",e);}}

    @Override protected void handleOnDestroy(){
        if(networkCallback!=null){try{((ConnectivityManager)getContext().getSystemService(Context.CONNECTIVITY_SERVICE)).unregisterNetworkCallback(networkCallback);}catch(RuntimeException ignored){}networkCallback=null;}
        cancelRace();if(socket!=null)socket.cancel();worker.shutdown();connector.shutdown();kdf.shutdown();probes.shutdownNow();http.dispatcher().executorService().shutdown();
    }
}
