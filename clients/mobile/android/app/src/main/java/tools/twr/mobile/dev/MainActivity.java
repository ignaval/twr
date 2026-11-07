package tools.twr.mobile.dev;

import com.getcapacitor.BridgeActivity;

public class MainActivity extends BridgeActivity {
    @Override public void onCreate(android.os.Bundle state) {
        registerPlugin(TwrTransportPlugin.class);
        registerPlugin(TwrLayoutPlugin.class);
        registerPlugin(TwrOfflinePlugin.class);
        super.onCreate(state);
        applyWebTextZoom();
    }
    private void applyWebTextZoom() {
        if (getBridge() != null && getBridge().getWebView() != null)
            getBridge().getWebView().getSettings().setTextZoom(100);
    }
    @Override public void onConfigurationChanged(android.content.res.Configuration configuration) {
        super.onConfigurationChanged(configuration);
        // CSS applies the system scale once; WebView's independent font zoom must stay neutral.
        applyWebTextZoom();
    }
}
