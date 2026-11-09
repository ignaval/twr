import { Capacitor } from "@capacitor/core";
import { PushNotifications } from "@capacitor/push-notifications";
import { LocalNotifications } from "@capacitor/local-notifications";
import type { RemoteHost } from "./transport";

const registering = new Map<
  RemoteHost,
  { promise: Promise<void>; allowPrompt: boolean }
>();
const generations = new WeakMap<RemoteHost, number>();
export async function disablePush(host: RemoteHost, id: string) {
  const previous = pushEnabled(id);
  const generation = (generations.get(host) || 0) + 1;
  generations.set(host, generation);
  localStorage.removeItem(pushPreferenceKey(id));
  try {
    await registering.get(host)?.promise.catch(() => {});
    await host.request("push-unregister", {}, 10000);
  } catch (error) {
    if (previous && generations.get(host) === generation)
      localStorage.setItem(pushPreferenceKey(id), "true");
    throw error;
  }
}
export const pushPreferenceKey = (id: string) => `twr.mobile.push.${id}`;
export const pushEnabled = (id: string) =>
  localStorage.getItem(pushPreferenceKey(id)) === "true";
export async function refreshPush(host: RemoteHost, id: string) {
  if (pushEnabled(id)) await enablePush(host, id, false);
}
/**
 * Removes the push and local notifications twr has delivered. Called when the app
 * becomes active and when a notification is tapped: Needs you shows what they were
 * about, so a stack of identical "Your agents have an update" helps no one.
 */
export async function clearDeliveredNotifications() {
  await Promise.all([
    PushNotifications.removeAllDeliveredNotifications().catch(() => {}),
    LocalNotifications.removeAllDeliveredNotifications().catch(() => {}),
  ]);
}
export async function notificationListeners(
  open: (data: Record<string, string>) => void,
  currentPushHost: () => RemoteHost | undefined = () => undefined,
) {
  const handles = await Promise.all([
    PushNotifications.addListener("registration", ({ value }) => {
      const host = currentPushHost();
      if (host && !registering.has(host))
        void host
          .request("push-register", {
            platform: Capacitor.getPlatform(),
            token: value,
          })
          .catch(() => {});
    }),
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
    PushNotifications.addListener(
      "pushNotificationActionPerformed",
      (event) => {
        void clearDeliveredNotifications();
        open(event.notification.data?.twr || event.notification.data || {});
      },
    ),
    LocalNotifications.addListener(
      "localNotificationActionPerformed",
      (event) => {
        void clearDeliveredNotifications();
        open(event.notification.extra || {});
      },
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
export function enablePush(
  host: RemoteHost,
  id: string,
  allowPrompt = true,
): Promise<void> {
  const current = registering.get(host);
  if (current)
    return allowPrompt && !current.allowPrompt
      ? current.promise.catch(() => {}).then(() => enablePush(host, id, true))
      : current.promise;
  const promise = registerPush(host, id, allowPrompt).finally(() =>
    registering.delete(host),
  );
  registering.set(host, { promise, allowPrompt });
  return promise;
}
async function registerPush(
  host: RemoteHost,
  id: string,
  allowPrompt: boolean,
) {
  const generation = generations.get(host) || 0;
  const current = () => (generations.get(host) || 0) === generation;
  const config = await host.request("push-status");
  if (!current()) return;
  const platform = Capacitor.getPlatform();
  if (!(platform === "ios" ? config.apns : config.fcm))
    throw new Error(
      "Remote push is not configured on this hub yet. Local test notifications are available.",
    );
  let permission = await PushNotifications.checkPermissions();
  if (!current()) return;
  if (permission.receive === "prompt" && allowPrompt)
    permission = await PushNotifications.requestPermissions();
  if (!allowPrompt && permission.receive !== "granted") return;
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
          if (current())
            await host.request("push-register", {
              platform,
              token: token.value,
            });
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
  if (current()) localStorage.setItem(pushPreferenceKey(id), "true");
}
