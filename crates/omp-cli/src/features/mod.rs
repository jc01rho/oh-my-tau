//! omt feature commands. Each module exposes `command(rt, args)`, wired from
//! `rpc/slash.rs` (`dispatch_ported`) and `slash_extra.rs` (`dispatch_extra`).

pub mod advisor;
pub mod browser;
pub mod btw;
pub mod budget;
pub mod local_editor;
pub mod loop_mode;
pub mod memory;
pub mod plan;
pub mod report_issue;
pub mod restart;
pub mod sandbox;
pub mod security;
pub mod shake;
pub mod share;
pub mod skills;
pub mod ssh;
pub mod worktree;
