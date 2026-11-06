import { Capacitor } from "@capacitor/core";
import { PushNotifications } from "@capacitor/push-notifications";
import { LocalNotifications } from "@capacitor/local-notifications";
import type { RemoteHost } from "./transport";

export async function notificationListeners(
  open: (data: Record<string, string>) => void,
) {
  const handles = await Promise.all([
    PushNotifications.addListener(
      "pushNotificationReceived",
      (notification) => {
        if (Capacitor.getPlatform() === "android")
          void LocalNotifications.schedule({
            notifications: [
              {
                id: 734271,
                title: "twr",
                body: "Your agents have an update",
                channelId: "twr-attention",
                isExactNotification: false,
                extra: notification.data?.twr || notification.data || {},
              },
            ],
          }).catch(() => {});
      },
    ),
    PushNotifications.addListener("pushNotificationActionPerformed", (event) =>
      open(event.notification.data?.twr || event.notification.data || {}),
    ),
    LocalNotifications.addListener(
      "localNotificationActionPerformed",
      (event) => open(event.notification.extra || {}),
    ),
  ]);
  return () => {
    handles.forEach((handle) => void handle.remove());
  };
}
export async function testNotification() {
  let permission = await LocalNotifications.checkPermissions();
  if (permission.display === "prompt")
    permission = await LocalNotifications.requestPermissions();
  if (permission.display !== "granted")
    throw new Error("Allow notifications in system Settings to run this test.");
  if (Capacitor.getPlatform() === "android")
    await LocalNotifications.createChannel({
      id: "twr-attention",
      name: "Agent updates",
      description: "Threads and requests that need you",
      importance: 4,
    });
  await LocalNotifications.schedule({
    notifications: [
      {
        id: Math.floor(Date.now() % 2147483647),
        title: "twr · local test",
        body: "Tap to open Needs you. Remote push delivery needs your Apple or Firebase configuration.",
        channelId: "twr-attention",
        isExactNotification: false,
        extra: { event: "test" },
      },
    ],
  });
}
export async function enablePush(host: RemoteHost) {
  const config = await host.request("push-status");
  const platform = Capacitor.getPlatform();
  if (!(platform === "ios" ? config.apns : config.fcm))
    throw new Error(
      "Remote push is not configured on this hub yet. Local test notifications are available.",
    );
  let permission = await PushNotifications.checkPermissions();
  if (permission.receive === "prompt")
    permission = await PushNotifications.requestPermissions();
  if (permission.receive !== "granted")
    throw new Error("Allow notifications in system Settings to enable push.");
  if (platform === "android")
    await PushNotifications.createChannel({
      id: "twr-attention",
      name: "Agent updates",
      importance: 4,
    });
  await new Promise<void>((resolve, reject) => {
    const timeout = setTimeout(() => {
      void clean();
      reject(
        new Error(
          "Push registration timed out. Check the native app configuration.",
        ),
      );
    }, 30000);
    const registered = PushNotifications.addListener(
      "registration",
      async (token) => {
        try {
          await host.request("push-register", { platform, token: token.value });
          resolve();
        } catch (error) {
          reject(error);
        } finally {
          await clean();
        }
      },
    );
    const failed = PushNotifications.addListener("registrationError", () => {
      void clean();
      reject(
        new Error(
          "Push registration failed. Check the Apple/Firebase configuration.",
        ),
      );
    });
    async function clean() {
      clearTimeout(timeout);
      (await registered).remove();
      (await failed).remove();
    }
    void Promise.all([registered, failed])
      .then(() => PushNotifications.register())
      .catch((error) => {
        void clean();
        reject(error);
      });
  });
}
