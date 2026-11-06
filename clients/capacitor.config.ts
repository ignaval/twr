import { KeyboardResize } from "@capacitor/keyboard";
import type { CapacitorConfig } from "@capacitor/cli";
const config: CapacitorConfig = {
  appId: "tools.twr.mobile.dev",
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
