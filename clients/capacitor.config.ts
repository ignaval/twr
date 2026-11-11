import { KeyboardResize } from "@capacitor/keyboard";
import type { CapacitorConfig } from "@capacitor/cli";
const config: CapacitorConfig = {
  // A development build's id. A store build is `tools.twr.mobile`: the release scripts set
  // TWR_MOBILE_APP_ID for `cap sync`, and the native projects name it for their release builds.
  appId: process.env.TWR_MOBILE_APP_ID || "tools.twr.mobile.dev",
  appName: "twr",
  backgroundColor: "#0e1013",
  loggingBehavior: "none",
  webDir: "mobile/www",
  android: {
    path: "mobile/android",
    allowMixedContent: false,
    minWebViewVersion: 119,
  },
  ios: {
    path: "mobile/ios",
    contentInset: "never",
    preferredContentMode: "mobile",
  },
  plugins: {
    SystemBars: { style: "DARK" },
    Keyboard: { resize: KeyboardResize.Native },
    PushNotifications: { presentationOptions: ["badge", "sound", "alert"] },
  },
};
export default config;
