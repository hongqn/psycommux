//! psycommux: drive cmux workspaces (Claude Code sessions, terminals,
//! browsers, Markdown) from a phone, as a PWA served over cmux's control socket.

pub mod agentchat;
pub mod cmux;
pub mod events;
pub mod net;
pub mod panels;
pub mod secrets;
pub mod web;
