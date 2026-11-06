import type { CapacitorConfig } from "@capacitor/cli";
const config: CapacitorConfig = {
  appId: "tools.twr.mobile.dev",
  appName: "twr",
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
    Keyboard: { resize: "native", resizeOnFullScreen: true },
    PushNotifications: { presentationOptions: ["badge", "sound", "alert"] },
  },
};
export default config;
