package tools.twr.mobile.dev;
import android.graphics.Rect;
import android.graphics.Color;
import android.content.res.Configuration;
import android.os.Build;
import android.view.Window;
import androidx.core.content.ContextCompat;
import androidx.core.util.Consumer;
import androidx.window.java.layout.WindowInfoTrackerCallbackAdapter;
import androidx.window.layout.*;
import com.getcapacitor.*;
import com.getcapacitor.annotation.CapacitorPlugin;

@CapacitorPlugin(name="TwrLayout")
public class TwrLayoutPlugin extends Plugin {
    private WindowInfoTrackerCallbackAdapter tracker;
    private Consumer<WindowLayoutInfo> listener;
    private JSObject layout=new JSObject().put("orientation","none");
    private Integer backgroundColor;
    @Override public void load(){
        tracker=new WindowInfoTrackerCallbackAdapter(WindowInfoTracker.getOrCreate(getActivity()));
        listener=info->{
            JSObject next=new JSObject().put("orientation","none");
            for(DisplayFeature feature:info.getDisplayFeatures()) if(feature instanceof FoldingFeature){
                FoldingFeature fold=(FoldingFeature)feature;Rect r=fold.getBounds();float scale=getContext().getResources().getDisplayMetrics().density;
                int[] location=new int[2];getBridge().getWebView().getLocationInWindow(location);
                next.put("orientation",fold.getOrientation()==FoldingFeature.Orientation.VERTICAL?"vertical":"horizontal");next.put("state",fold.getState()==FoldingFeature.State.HALF_OPENED?"half-opened":"flat");next.put("separating",fold.isSeparating());
                next.put("left",(r.left-location[0])/scale);next.put("top",(r.top-location[1])/scale);next.put("width",r.width()/scale);next.put("height",r.height()/scale);break;
            }
            layout=next;notifyListeners("change",next,true);
        };
        String startupBackground = getBridge().getConfig().getBackgroundColor();
        if (validBackground(startupBackground)) backgroundColor = Color.parseColor(startupBackground);
        getActivity().runOnUiThread(()->{
            tracker.addWindowLayoutInfoListener(getActivity(),ContextCompat.getMainExecutor(getContext()),listener);
            applyBackground();
        });
    }
    @PluginMethod public void setAppearance(PluginCall call) {
        String background = call.getString("background");
        if (!validBackground(background)) {
            call.reject("An opaque six-digit hexadecimal background color is required");
            return;
        }
        int color = Color.parseColor(background);
        getActivity().runOnUiThread(() -> {
            backgroundColor = color;
            applyBackground();
            call.resolve();
        });
    }
    private static boolean validBackground(String value) {
        return value != null && value.matches("^#[0-9a-fA-F]{6}$");
    }
    @SuppressWarnings("deprecation")
    private void applyBackground() {
        if (backgroundColor == null) return;
        Window window = getActivity().getWindow();
        window.getDecorView().setBackgroundColor(backgroundColor);
        getBridge().getWebView().setBackgroundColor(backgroundColor);
        // Modern edge-to-edge bars are transparent and show the decor color.
        // Older Android versions still draw explicit bar backgrounds.
        if (Build.VERSION.SDK_INT < 35) {
            window.setStatusBarColor(backgroundColor);
            window.setNavigationBarColor(backgroundColor);
        }
    }
    @Override protected void handleOnConfigurationChanged(Configuration configuration) {
        super.handleOnConfigurationChanged(configuration);
        // Core SystemBars reapplies icon appearance during this dispatch and
        // resets the decor background to the OS theme. Paint after all plugins.
        getActivity().getWindow().getDecorView().post(this::applyBackground);
    }
    @Override protected void handleOnResume() {
        super.handleOnResume();
        getActivity().getWindow().getDecorView().post(this::applyBackground);
    }
    @PluginMethod public void current(PluginCall call){call.resolve(layout);}
    @Override protected void handleOnDestroy(){if(tracker!=null&&listener!=null)tracker.removeWindowLayoutInfoListener(listener);}
}
