import type { Hub } from "./remote";
import { displayPin, displayRoute } from "./phone-pairing";

/** A pinned hub's key (its pin, grouped for reading), its known routes and the route in
 * use: for comparing the key with `twr phone` on the hub. Nothing for a certificate-mode
 * hub. Shared by the desktop's saved hubs and the phone's hub sheet. */
export function HubKeyDetails({ hub, open }: { hub: Hub; open?: boolean }) {
  if (!hub.pin) return null;
  return (
    <details className="hub-key" open={open}>
      <summary>Key and addresses</summary>
      <p>
        Hub key: <code>{displayPin(hub.pin)}</code>
      </p>
      {hub.route && <p>In use: {displayRoute(hub.route)}</p>}
      {!!hub.routes?.length && (
        <>
          <p>Known addresses:</p>
          <ul aria-label="Known addresses">
            {hub.routes.map((route) => (
              <li key={route}>{displayRoute(route)}</li>
            ))}
          </ul>
        </>
      )}
    </details>
  );
}
