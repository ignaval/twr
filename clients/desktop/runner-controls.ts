import type { DesktopAPI } from "../shared/types";

/** Runner hosting belongs to this desktop even while its view shows a remote hub. */
export function routeRunnerControls(
  local: DesktopAPI["rpc"],
  remote: DesktopAPI["rpc"],
  current: () => boolean = () => true,
): DesktopAPI["rpc"] {
  return (method, params) => {
    // Unmount must retire its exact local code even after the hub view is fenced.
    if (
      method === "desktop.runner.cancel" &&
      typeof params?.session === "string" &&
      params.session.length > 0
    )
      return local(method, params);
    if (!current())
      return Promise.reject(new Error("This hub connection was closed"));
    return [
      "desktop.runner.use",
      "desktop.runner.cancel",
      "desktop.runner.status",
    ].includes(method)
      ? local(method, params)
      : remote(method, params);
  };
}
