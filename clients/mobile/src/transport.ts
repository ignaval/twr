import { secureRead, secureWrite, secureDelete } from "./offline";
import { registerPlugin } from "@capacitor/core";
import { Clipboard } from "@capacitor/clipboard";
import { Filesystem, Directory, Encoding } from "@capacitor/filesystem";
import { Share } from "@capacitor/share";
import {
  RemoteHost as SharedRemoteHost,
  type NativeTransport,
  type Hub,
} from "../../shared/remote";
export type { Hub, NativeTransport } from "../../shared/remote";
export const Transport = registerPlugin<
  NativeTransport & {
    scanInvitation(): Promise<{ invitation: string }>;
  }
>("TwrTransport");
export class RemoteHost extends SharedRemoteHost {
  constructor(
    hub: Hub,
    showHubs: () => void,
    chooseDirectory: () => Promise<string | null>,
  ) {
    super(hub, showHubs, chooseDirectory, Transport, {
      secureRead,
      secureWrite,
      secureDelete,
      platform: navigator.userAgent.includes("Android") ? "android" : "ios",
      copyText: async (text) => {
        await Clipboard.write({ string: text });
      },
      openExternal: async (url) => {
        const { Browser } = await import("@capacitor/browser");
        await Browser.open({ url });
      },
      reveal: async (path) => {
        const { Dialog } = await import("@capacitor/dialog");
        await Dialog.alert({ title: "Path on your hub", message: path });
      },
      exportText: async (name, content) => {
        const file = await Filesystem.writeFile({
          path: name,
          directory: Directory.Cache,
          encoding: Encoding.UTF8,
          data: content,
        });
        await Share.share({ title: name, url: file.uri });
      },
    });
  }
}
