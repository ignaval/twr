import type { DesktopAPI, Json } from "./types";
import {
  createUploadFile,
  type UploadRequest,
  type UploadResume,
} from "./uploads";

type LocalUploadRequest = (
  workspace: string,
  method: string,
  params: Json,
) => Promise<any>;
declare global {
  interface Window {
    twrUploadRequest: LocalUploadRequest;
  }
}
/** Native IPC validates this narrow attachment lane and captures its workspace bridge. */
export function withLocalUploads(
  api: DesktopAPI,
  nativeRequest: LocalUploadRequest,
): DesktopAPI {
  return {
    ...api,
    uploadFile: async (file, thread, options) => {
      const { workspace } = await api.bootstrap();
      if (
        (options?.workspace && options.workspace !== workspace) ||
        (options?.resume && options.resume.workspace !== workspace)
      )
        throw new Error(
          "Workspace changed; return to the attachment's workspace to resume",
        );
      const request: UploadRequest = (method, params) =>
        nativeRequest(workspace, method, params);
      return createUploadFile(request, () => workspace)(file, thread, options);
    },
    cancelUpload: async (resume: UploadResume) => {
      const { workspace } = await api.bootstrap();
      if (workspace !== resume.workspace)
        throw new Error(
          "Workspace changed; return to the attachment's workspace to cancel",
        );
      return await nativeRequest(workspace, "upload-cancel", {
        id: resume.id,
        thread: resume.thread,
        sha256: resume.sha256,
      });
    },
  };
}
