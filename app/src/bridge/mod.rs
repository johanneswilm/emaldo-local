// Bridge role: answers the vendor REST/E2E protocol locally so the vendor Go app
// (`/root/bin/app`) has no cloud egress, and serves the bridge web UI.
//
// These modules came from the standalone `emaldo-bridge` crate and still keep
// their own config/state/log/tls stack (bridge::config, bridge::state). The
// single-app plan (docs/SINGLE_APP_PLAN.md) folds them onto the shared
// `telemetry::State`, `config` store and web UI; until that lands the bridge
// role stays self-contained here so both roles can ship in one binary.
pub mod api;
pub mod config;
pub mod dns;
pub mod e2e;
pub mod log;
pub mod mode;
pub mod mqtt_pub;
pub mod net;
pub mod poller;
pub mod relay;
pub mod run;
pub mod actuate;
pub mod state;
pub mod telem;
pub mod tls;
pub mod ui;
pub mod vendor;

#[cfg(test)]
mod tests;
