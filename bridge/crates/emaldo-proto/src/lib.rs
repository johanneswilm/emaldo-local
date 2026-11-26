// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Wire protocol of the Emaldo cloud platform, as spoken by
//   * the module binary (`/root/bin/app`) — REST with `gm=2` + MSCT/E2E over UDP,
//   * the vendor Android app and the community SDK — REST with `gm=1`,
//   * the e2e relay — MSCT routing with synthetic ACKs.
//
// This crate is pure codec: no sockets, no state. See docs/PROTOCOL.md for the
// reverse-engineering evidence behind every field.

pub mod codec;
pub mod cmds;
pub mod envelope;
pub mod msct;
pub mod params;

pub use codec::*;
pub use envelope::{answer_error, answer_gm1, answer_gm2, gm1_result, gm2_result, parse_request, Request};
pub use msct::{build, encode, AckStyle, Frame, Opt};
pub use params::*;
