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
        const path = `${EXPORTS}/${name}`;
        const file = await Filesystem.writeFile({
          path,
          directory: Directory.Cache,
          encoding: Encoding.UTF8,
          data: content,
          recursive: true,
        });
        try {
          await Share.share({ title: name, url: file.uri });
        } finally {
          // The transcript is plaintext: it lives only as long as the share needs it.
          // Android hands the receiving app a content URI it may read just after the
          // chooser returns, so the file outlives the sheet there for a minute.
          const remove = () =>
            void Filesystem.deleteFile({
              path,
              directory: Directory.Cache,
            }).catch(() => {});
          if (navigator.userAgent.includes("Android"))
            setTimeout(remove, 60_000);
          else remove();
        }
      },
    });
  }
}

/** Cache folder of transcript exports waiting for the share sheet (docs/mobile.md). */
const EXPORTS = "exports";
/** Deletes every export left behind: at startup, and when a hub is forgotten. */
export async function sweepExports() {
  await Filesystem.rmdir({
    path: EXPORTS,
    directory: Directory.Cache,
    recursive: true,
  }).catch(() => {});
}
