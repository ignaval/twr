import { z } from "zod";
import { workspaceSchema } from "./validation";

const common = z.object({
  id: z
    .string()
    .min(1)
    .max(64)
    .regex(/^[a-zA-Z0-9-]+$/),
  thread: z.string().min(1).max(256),
  sha256: z.string().regex(/^[0-9a-f]{64}$/),
});
const begin = common
  .extend({
    name: z.string().min(1).max(120),
    size: z
      .number()
      .int()
      .min(0)
      .max(25 * 1024 * 1024),
  })
  .strict();
const chunk = common
  .extend({
    offset: z
      .number()
      .int()
      .min(0)
      .max(25 * 1024 * 1024),
    data: z
      .string()
      .max(87384)
      .regex(/^[A-Za-z0-9+/]*={0,2}$/)
      .refine(
        (data) =>
          data.length % 4 === 0 && Buffer.byteLength(data, "base64") <= 65536,
        "Upload chunks can be at most 64 KiB",
      ),
  })
  .strict();
interface Context {
  workspace: string;
  ready: boolean;
  bridge: {
    call(method: string, params: unknown, timeout?: number): Promise<any>;
  };
}
/** Called only after the main process checks the IPC sender. No renderer filesystem paths. */
export function localUploadRequest(context: () => Context) {
  return async (
    requestedWorkspace: unknown,
    method: unknown,
    input: unknown,
  ) => {
    const selectedWorkspace = workspaceSchema.parse(requestedWorkspace);
    const current = context();
    if (!current.ready || selectedWorkspace !== current.workspace)
      throw new Error(
        "Workspace changed; return to the attachment's workspace to continue",
      );
    const action = z
      .enum([
        "upload-begin",
        "upload-status",
        "upload-chunk",
        "upload-commit",
        "upload-cancel",
      ])
      .parse(method);
    const schema =
      action === "upload-begin"
        ? begin
        : action === "upload-chunk"
          ? chunk
          : common.strict();
    const params = schema.parse(input);
    // Capture before awaiting: a workspace switch cannot redirect an admitted write.
    const result = await current.bridge.call(
      "thread.upload",
      {
        ...params,
        action: action.slice(7),
      },
      15_000,
    );
    return result && typeof result === "object"
      ? { ...result, chunk_size: 65536 }
      : result;
  };
}
