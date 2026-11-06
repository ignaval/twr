package tools.twr.mobile.dev;

import com.getcapacitor.BridgeActivity;

public class MainActivity extends BridgeActivity {
    @Override public void onCreate(android.os.Bundle state) {
        registerPlugin(TwrTransportPlugin.class);
        registerPlugin(TwrLayoutPlugin.class);
        super.onCreate(state);
    }
}
