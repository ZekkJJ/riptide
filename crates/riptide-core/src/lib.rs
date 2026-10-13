// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Riptide download engine.
//!
//! A free, open source multi-segment download engine. The pieces fit together as:
//!
//! - [`probe`] asks a server what it will serve: size, whether it honours byte ranges, the
//!   filename, and the validators needed to resume safely later.
//! - [`chunks`] holds the segmentation state as an exact tiling of the file, and hands out new
//!   segments by stealing the tail of whichever one has the most work left.
//! - [`storage`] writes every segment straight into its final position in one sparse file, so
//!   a finished transfer needs no reassembly pass.
//! - [`download`] runs a pool of connections over a shared chunk map, with retries, mirror
//!   rotation, pause and resume.
//! - [`state`] persists progress beside the payload and refuses to resume onto a file that
//!   changed on the server.
//! - [`media`] turns HLS and DASH manifests into an ordered segment list, and
//!   [`stream`] fetches that list into one file.
//! - [`category`] decides which folder a finished file belongs in.

pub mod category;
pub mod chunks;
pub mod download;
pub mod error;
pub mod media;
pub mod probe;
pub mod state;
pub mod storage;
pub mod stream;
pub mod throttle;

pub use chunks::{Chunk, ChunkMap};
pub use download::{Download, DownloadConfig, DownloadHandle, Progress};
pub use error::{Error, Result};
pub use probe::{RequestSpec, ResourceInfo};
pub use state::ResumeState;
