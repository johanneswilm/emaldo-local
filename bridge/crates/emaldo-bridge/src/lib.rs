// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//! Local-first bridge for the Emaldo Power Core: vendor REST front, E2E relay,
//! operator web UI, and the local/vendor mode switch — one static binary that
//! runs on the battery.

pub mod api;
pub mod config;
pub mod dns;
pub mod e2e;
pub mod log;
pub mod mode;
pub mod net;
pub mod poller;
pub mod relay;
pub mod state;
pub mod tls;
pub mod ui;
pub mod vendor;

#[cfg(test)]
mod tests;
