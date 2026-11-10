//! What a paired device may do, where the hub decides (`docs/mobile.md`, `docs/security-model.md`).
//!
//! A device's connection is the user's (`Caller::User("device:<id>")`, its turns `from: user`):
//! the server checks each request's method against the device's scopes
//! (`twr_core::devices::Device::allows_params`), and the hub checks here what a method's
//! parameters or a later step would do beyond them. The grant is read from the gateway's device
//! records at the decision, by the client name, so a step that comes later than the request (the
//! orchestrator relaying a mode change from a device's message, a confirm card answered from a
//! device) is judged by the device it came from, and a device revoked since has no scope. A
//! client name of the form `device:<id>` is a device's only: `hello` takes no `:` in a local
//! client's name.
//!
//! - `terminal`: a runtime that is a command line (`pty:<command>`, `acp:<command>`); without
//!   it a device names only what the orchestrator may ([`orch::check_chosen_runtime`]: a
//!   built-in agent runtime, `:<account>` included, or a configured `[runtimes.<name>]`
//!   preset), or no runtime (the project's, else `default_runtime`).
//! - `admin`: every permission mode change (`thread.set_mode`, a short mode command in `say`,
//!   the orchestrator's relayed `set_mode` and confirm cards that carry one), a new thread in a
//!   mode outside [`twr_core::devices::OPERATE_MODES`], moving a thread to another runtime
//!   (directly, relayed or on a card), and `outside_pin`.
//!
//! The local user (any other client) is never limited here.

use twr_core::devices::grants;
use twr_core::proto::{codes, RpcError};

use super::{orch, rpc, Caller, Inner};

/// The client-name prefix of a paired device's connection (`server::hello`).
pub(super) const DEVICE_PREFIX: &str = "device:";

/// A paired device's scopes, as its records say now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeviceGrant {
    /// The device's id.
    pub(super) id: String,
    /// Its scopes; none when it is no longer paired.
    pub(super) scopes: Vec<String>,
}

impl DeviceGrant {
    /// Whether the device holds `scope`.
    pub(super) fn has(&self, scope: &str) -> bool {
        grants(&self.scopes, scope)
    }

    /// `unauthorized`, naming what needs `scope`.
    pub(super) fn refusal(&self, what: &str, scope: &str) -> RpcError {
        rpc(
            codes::UNAUTHORIZED,
            format!(
                "This device may not {what}: that needs the {scope} permission, which it was \
                 not paired with. Do it on the computer running the hub, or pair the device \
                 again with --allow-{scope}"
            ),
        )
    }

    /// `Ok` when the device holds `scope`, else [`DeviceGrant::refusal`].
    pub(super) fn require(&self, what: &str, scope: &str) -> Result<(), RpcError> {
        if self.has(scope) {
            Ok(())
        } else {
            Err(self.refusal(what, scope))
        }
    }
}

/// What a paired device's request gets while the hub cannot read the device records
/// ([`devices_unreadable`]): the gateway's words for `twr_core::devices::Standing::Unknown`.
/// Nothing was done; the same request may succeed once they read again.
pub(crate) const DEVICES_UNREADABLE: &str =
    "The hub could not read its paired devices. Try again in a moment.";

/// The refusal of a paired device's request while the device records cannot be read: never a
/// grant (the device may have been revoked or narrowed since), and not `unauthorized` either
/// (nothing says it lost a scope): `internal`, saying to try again.
pub(crate) fn devices_unreadable() -> RpcError {
    rpc(codes::INTERNAL, DEVICES_UNREADABLE)
}

impl Inner {
    /// The grant of the paired device whose connection is named `client` (`device:<id>`);
    /// `Ok(None)` for every other client (the local user's), without reading anything. A
    /// device the records no longer list grants nothing; records that can't be read refuse the
    /// request ([`devices_unreadable`]): an error there is never taken for a grant.
    pub(super) fn device_grant(&self, client: &str) -> Result<Option<DeviceGrant>, RpcError> {
        let Some(id) = client.strip_prefix(DEVICE_PREFIX) else {
            return Ok(None);
        };
        let devices = self.device_records().map_err(|e| {
            tracing::warn!(device = %id, error = %format!("{e:#}"), "cannot read the paired devices; refusing the device's request");
            devices_unreadable()
        })?;
        let scopes = devices
            .as_array()
            .and_then(|devices| {
                devices.iter().find_map(|d| {
                    (d["id"].as_str() == Some(id)).then(|| {
                        d["scopes"]
                            .as_array()
                            .map(|s| {
                                s.iter()
                                    .filter_map(|s| s.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default()
                    })
                })
            })
            .unwrap_or_default();
        Ok(Some(DeviceGrant {
            id: id.to_string(),
            scopes,
        }))
    }

    /// [`Inner::device_grant`] of `caller`; `Ok(None)` for the orchestrator and the local user.
    pub(super) fn caller_grant(&self, caller: &Caller) -> Result<Option<DeviceGrant>, RpcError> {
        match caller {
            Caller::User(client) => self.device_grant(client),
            Caller::Orchestrator => Ok(None),
        }
    }

    /// The gateway's device records, beside the config file (`mobile/`).
    pub(super) fn device_auth(&self) -> anyhow::Result<twr_core::devices::Auth> {
        let config = self
            .config_file
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Device pairing requires a configured hub"))?;
        twr_core::devices::Auth::open(&config.with_file_name("mobile"))
    }

    /// The paired devices (`twr_core::devices::Auth::devices`).
    fn device_records(&self) -> anyhow::Result<serde_json::Value> {
        self.device_auth()?.devices()
    }

    /// Whether `caller` may name runtime `spec` for a thread (`thread.new` / `thread.spawn` /
    /// `thread.set_model` `runtime`) or the orchestrator (`orchestrator.use`): the local user
    /// any; a device with `terminal` any; another device only what the orchestrator may
    /// ([`orch::check_chosen_runtime`]), never a command line.
    pub(super) fn check_caller_runtime(&self, caller: &Caller, spec: &str) -> Result<(), RpcError> {
        match self.caller_grant(caller)? {
            Some(g) if !g.has("terminal") => {
                orch::check_chosen_runtime(spec, &self.config, orch::Chooser::Device)
            }
            _ => Ok(()),
        }
    }

    /// `Ok` unless `caller` is a device without `scope` ([`DeviceGrant::require`]).
    pub(super) fn require_scope(
        &self,
        caller: &Caller,
        what: &str,
        scope: &str,
    ) -> Result<(), RpcError> {
        match self.caller_grant(caller)? {
            Some(g) => g.require(what, scope),
            None => Ok(()),
        }
    }
}
