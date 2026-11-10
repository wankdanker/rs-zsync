//! zsync — replicate ZFS datasets selected by glob.
//!
//! For each dataset, zsync lists the snapshots on the source and the destination, finds the
//! newest snapshot they share and sends exactly what the destination is missing (`zfs send -I`),
//! falling back to a full (replication) stream when there is nothing in common. Hosts are reached
//! over OpenSSH; streams can be relayed through this machine, sent host-to-host over ssh, or over
//! raw TCP, with optional compression, buffering, bandwidth limits, progress and resumable
//! receives.
//!
//! The pure logic lives in [`plan`]; [`ops`] runs the commands against a [`backend::Backend`].

#[macro_use]
pub mod log;

pub mod backend;
pub mod complete;
pub mod config;
pub mod dateformat;
pub mod exec;
pub mod host;
pub mod lock;
pub mod naming;
pub mod ops;
pub mod output;
pub mod plan;
pub mod select;
pub mod transport;
pub mod util;
pub mod zfs;
