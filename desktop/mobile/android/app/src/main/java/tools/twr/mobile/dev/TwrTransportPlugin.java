package tools.twr.mobile.dev;

import android.content.Context;
import android.content.SharedPreferences;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.util.Base64;
import com.getcapacitor.*;
import com.getcapacitor.annotation.CapacitorPlugin;
import java.net.URI;
import java.security.KeyStore;
import java.util.concurrent.TimeUnit;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;
import okhttp3.*;
import org.json.*;

/** Tokens stay in Android Keystore protected storage and never cross the JS bridge. */
@CapacitorPlugin(name="TwrTransport")
public class TwrTransportPlugin extends Plugin {
    private static final String KEY="tools.twr.mobile.credentials";
    private final OkHttpClient http=new OkHttpClient.Builder().followRedirects(false).followSslRedirects(false).connectTimeout(15,TimeUnit.SECONDS).readTimeout(30,TimeUnit.SECONDS).pingInterval(20,TimeUnit.SECONDS).build();
    private volatile WebSocket socket;
    private final java.util.concurrent.atomic.AtomicLong connectionGeneration=new java.util.concurrent.atomic.AtomicLong();
    private String activeHub;
    private final java.util.concurrent.ExecutorService worker=java.util.concurrent.Executors.newSingleThreadExecutor();
    private SecretKey key() throws Exception {
        KeyStore store=KeyStore.getInstance("AndroidKeyStore");store.load(null);
        if(store.containsAlias(KEY)) return (SecretKey)store.getKey(KEY,null);
        KeyGenerator generator=KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES,"AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(KEY,KeyProperties.PURPOSE_ENCRYPT|KeyProperties.PURPOSE_DECRYPT).setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).build());
        return generator.generateKey();
    }
    private SharedPreferences prefs(){return getContext().getSharedPreferences("twr-private",Context.MODE_PRIVATE);}
    private synchronized JSONObject vault() throws Exception {
        String encoded=prefs().getString("hubs",null);if(encoded==null)return new JSONObject();
        byte[] bytes=Base64.decode(encoded,Base64.NO_WRAP);
        Cipher cipher=Cipher.getInstance("AES/GCM/NoPadding");cipher.init(Cipher.DECRYPT_MODE,key(),new GCMParameterSpec(128,bytes,0,12));
        return new JSONObject(new String(cipher.doFinal(bytes,12,bytes.length-12),java.nio.charset.StandardCharsets.UTF_8));
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
    private JSObject metadata(String id,JSONObject hub) throws JSONException {
        JSObject data=new JSObject();data.put("id",id);data.put("name",hub.optString("name"));data.put("url",hub.optString("url"));data.put("scopes",hub.optJSONArray("scopes"));data.put("gateway_id",hub.optString("gateway_id"));return data;
    }
    @PluginMethod public void listHubs(PluginCall call){try{JSONObject data=vault();JSArray hubs=new JSArray();java.util.Iterator<String> keys=data.keys();while(keys.hasNext()){String id=keys.next();hubs.put(metadata(id,data.getJSONObject(id)));}JSObject value=new JSObject();value.put("hubs",hubs);call.resolve(value);}catch(Exception e){call.reject("Could not read paired hubs",e);}}
    @PluginMethod public void pair(PluginCall call){worker.execute(()->{try{
        String url=endpoint(call.getString("url",""));JSONObject body=new JSONObject();body.put("code",call.getString("code",""));body.put("name",call.getString("name",""));
        try(Response health=http.newCall(new Request.Builder().url(url+"/health").build()).execute()) {
            if(!health.isSuccessful()||health.body()==null)throw new Exception("Could not identify the hub");
            String gateway=new JSONObject(health.body().string()).getString("gateway_id");
            JSONObject existing=vault();java.util.Iterator<String> keys=existing.keys();
            while(keys.hasNext())if(gateway.equals(existing.getJSONObject(keys.next()).optString("gateway_id")))throw new Exception("This hub is already paired. Use Revoke this device in its settings before pairing again.");
        }
        Request request=new Request.Builder().url(url+"/v1/pair").post(RequestBody.create(body.toString(),MediaType.get("application/json"))).build();
        try(Response response=http.newCall(request).execute()){
            if(!response.isSuccessful()||response.body()==null)throw new Exception("Pairing failed. Check the code and hub URL.");
            JSONObject record=new JSONObject(response.body().string());String id=java.util.UUID.randomUUID().toString();record.put("url",url);record.put("name",call.getString("hubName","My hub"));
            if(record.getString("token").length()!=64)throw new Exception("Invalid hub credentials");
            JSONObject all=vault();all.put(id,record);save(all);call.resolve(metadata(id,record));
        }
    }catch(Exception e){call.reject(e.getMessage(),e);}});}
    @PluginMethod public void connect(PluginCall call){try{
        final long generation=connectionGeneration.incrementAndGet();
        if(socket!=null)socket.close(1000,"Switching hub");
        String id=call.getString("id","");JSONObject hub=vault().getJSONObject(id);String url=endpoint(hub.getString("url")).replaceFirst("^http","ws")+"/v1/socket";
        activeHub=id;
        WebSocket opened=http.newWebSocket(new Request.Builder().url(url).header("Authorization","Bearer "+hub.getString("token")).build(),new WebSocketListener(){
            private boolean authenticated=false;
            @Override public void onOpen(WebSocket ws,Response response){}
            @Override public void onMessage(WebSocket ws,String text){if(generation!=connectionGeneration.get())return;try{if(text.length()>16*1024*1024)throw new Exception("Hub response too large");JSObject message=new JSObject(text);if("authenticated".equals(message.optString("type"))){authenticated=true;call.resolve();}JSObject value=new JSObject();value.put("message",message);notifyListeners("message",value);}catch(Exception e){ws.cancel();}}
            @Override public void onClosed(WebSocket ws,int code,String reason){if(!authenticated)call.reject("Hub authentication failed");closed(ws);}
            @Override public void onFailure(WebSocket ws,Throwable error,Response response){if(!authenticated)call.reject("Could not connect to the hub");closed(ws);}
            private void closed(WebSocket ws){if(generation!=connectionGeneration.get())return;JSObject value=new JSObject();value.put("message",new JSObject().put("type","transport-closed").put("connected",false).put("error","Remote hub connection closed"));notifyListeners("message",value);}
        });
        if(generation==connectionGeneration.get())socket=opened;else opened.cancel();
    }catch(Exception e){call.reject("Could not connect to the paired hub",e);}}
    @PluginMethod public void send(PluginCall call){String message=call.getString("message","");if(message.getBytes(java.nio.charset.StandardCharsets.UTF_8).length>2*1024*1024){call.reject("Request exceeds 2 MiB","REQUEST_TOO_LARGE");return;}if(socket==null||!socket.send(message)){call.reject("Hub connection is closed");}else call.resolve();}
    @PluginMethod public void disconnect(PluginCall call){connectionGeneration.incrementAndGet();if(socket!=null){socket.close(1000,"App disconnected");socket=null;}call.resolve();}
    @PluginMethod public void removeHub(PluginCall call){try{String id=call.getString("id","");JSONObject all=vault();all.remove(id);save(all);if(id.equals(activeHub)){connectionGeneration.incrementAndGet();if(socket!=null)socket.cancel();socket=null;}call.resolve();}catch(Exception e){call.reject("Could not remove hub",e);}}
    @Override protected void handleOnDestroy(){if(socket!=null)socket.cancel();worker.shutdown();http.dispatcher().executorService().shutdown();}
}
