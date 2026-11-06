package tools.twr.mobile.dev;
import android.graphics.Rect;
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
        getActivity().runOnUiThread(()->tracker.addWindowLayoutInfoListener(getActivity(),ContextCompat.getMainExecutor(getContext()),listener));
    }
    @PluginMethod public void current(PluginCall call){call.resolve(layout);}
    @Override protected void handleOnDestroy(){if(tracker!=null&&listener!=null)tracker.removeWindowLayoutInfoListener(listener);}
}
