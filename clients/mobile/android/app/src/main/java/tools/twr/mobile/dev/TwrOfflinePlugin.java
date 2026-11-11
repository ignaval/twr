package tools.twr.mobile.dev;

import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import com.getcapacitor.*;
import com.getcapacitor.annotation.CapacitorPlugin;
import java.io.*;
import java.nio.charset.StandardCharsets;
import android.system.Os;
import android.system.OsConstants;
import java.security.*;
import java.util.concurrent.*;
import javax.crypto.*;
import javax.crypto.spec.GCMParameterSpec;

/** AES-GCM files live outside Android backups; the key never leaves Keystore. */
@CapacitorPlugin(name="TwrOffline")
public class TwrOfflinePlugin extends Plugin {
    private static final int LIMIT=8*1024*1024;
    private static final String KEY="tools.twr.offline";
    private final ExecutorService worker=Executors.newSingleThreadExecutor();
    private SecretKey key() throws Exception {
        KeyStore store=KeyStore.getInstance("AndroidKeyStore");store.load(null);
        if(store.containsAlias(KEY))return (SecretKey)store.getKey(KEY,null);
        KeyGenerator generator=KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES,"AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(KEY,KeyProperties.PURPOSE_ENCRYPT|KeyProperties.PURPOSE_DECRYPT).setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).build());
        return generator.generateKey();
    }
    private File location(PluginCall call) throws Exception {
        String id=call.getString("key","");
        if(id.isEmpty()||id.getBytes(StandardCharsets.UTF_8).length>512||id.chars().anyMatch(c->c<32))throw new Exception("Invalid storage key");
        File directory=new File(getContext().getNoBackupFilesDir(),"twr-offline");
        if(!directory.isDirectory()&&!directory.mkdirs())throw new Exception("Could not create encrypted storage");
        byte[] digest=MessageDigest.getInstance("SHA-256").digest(id.getBytes(StandardCharsets.UTF_8));
        StringBuilder name=new StringBuilder();for(byte b:digest)name.append(String.format("%02x",b));
        return new File(directory,name+".enc");
    }
    @PluginMethod public void read(PluginCall call){worker.execute(()->{try{
        File file=location(call);JSObject reply=new JSObject();
        if(!file.exists()){reply.put("value",org.json.JSONObject.NULL);call.resolve(reply);return;}
        if(OsConstants.S_ISLNK(Os.lstat(file.getPath()).st_mode)||file.length()>LIMIT*3||file.length()<28)throw new Exception("Invalid offline history file");
        byte[] bytes;try(FileInputStream in=new FileInputStream(file);ByteArrayOutputStream out=new ByteArrayOutputStream()){byte[] buffer=new byte[8192];int count;while((count=in.read(buffer))!=-1){if(out.size()+count>LIMIT*3)throw new Exception("Invalid offline history file");out.write(buffer,0,count);}bytes=out.toByteArray();}Cipher cipher=Cipher.getInstance("AES/GCM/NoPadding");
        cipher.init(Cipher.DECRYPT_MODE,key(),new GCMParameterSpec(128,bytes,0,12));cipher.updateAAD(call.getString("key","").getBytes(StandardCharsets.UTF_8));
        reply.put("value",new String(cipher.doFinal(bytes,12,bytes.length-12),StandardCharsets.UTF_8));call.resolve(reply);
    }catch(Exception e){call.reject("Could not read encrypted offline history",e);}});}
    @PluginMethod public void write(PluginCall call){worker.execute(()->{File staged=null;try{
        File file=location(call);String value=call.getString("value");if(value==null||value.getBytes(StandardCharsets.UTF_8).length>LIMIT)throw new Exception("Offline state exceeds 8 MiB");
        Cipher cipher=Cipher.getInstance("AES/GCM/NoPadding");cipher.init(Cipher.ENCRYPT_MODE,key());cipher.updateAAD(call.getString("key","").getBytes(StandardCharsets.UTF_8));
        byte[] encrypted=cipher.doFinal(value.getBytes(StandardCharsets.UTF_8));long total=12+encrypted.length;
        File[] files=file.getParentFile().listFiles();if(files==null)throw new Exception("Could not inspect offline storage");
        for(File entry:files)if(!entry.equals(file))total+=entry.length();if(total>64*1024*1024)throw new Exception("Offline storage is full. Clear saved history in Settings.");
        staged=File.createTempFile("offline-",".part",file.getParentFile());
        try(FileOutputStream out=new FileOutputStream(staged)){out.write(cipher.getIV());out.write(encrypted);out.getFD().sync();}
        Os.rename(staged.getPath(),file.getPath());call.resolve();
    }catch(Exception e){call.reject(e.getMessage(),e);}finally{if(staged!=null)staged.delete();}});}
    @PluginMethod public void remove(PluginCall call){worker.execute(()->{try{File file=location(call);if(file.exists()&&!file.delete())throw new Exception("Could not delete file");call.resolve();}catch(Exception e){call.reject("Could not clear offline history",e);}});}
    @PluginMethod public void textScale(PluginCall call){JSObject reply=new JSObject();reply.put("scale",getContext().getResources().getConfiguration().fontScale);call.resolve(reply);}
    @Override protected void handleOnConfigurationChanged(android.content.res.Configuration configuration){JSObject reply=new JSObject();reply.put("scale",configuration.fontScale);notifyListeners("textScale",reply);}
    @Override protected void handleOnResume(){JSObject reply=new JSObject();reply.put("scale",getContext().getResources().getConfiguration().fontScale);notifyListeners("textScale",reply);}
}
