pub mod asciicast;
mod attach_input;
mod attach_keyboard;
pub mod attach_status;
mod attachment_control;
mod attachment_view;
pub mod build_info;
pub mod cast_reader;
pub mod cli;
mod conpty_startup;
pub mod da;
pub mod duration_parser;
mod generation_recovery;
mod host;
mod http_uds;
mod image_backing;
mod image_delivery;
mod image_shm;
mod keyboard;
pub mod keys;
mod kitty_output;
pub mod packet;
pub mod platform;
pub mod program_status;
pub mod protocol;
pub mod provider;
mod provider_daemon;
pub mod provider_ffi;
pub mod proxy;
pub mod proxy_command;
pub mod recording;
pub mod recreate;
pub mod replay;
pub mod runtime;
mod screen_activity;
pub mod screen_query;
pub mod server;
pub mod session;
mod session_runtime;
pub mod socket_client;
mod status_output;
pub mod vt;

mod mouse;
mod output_admission;

mod attach_mouse;

mod terminal_identity;

#[cfg(unix)]
pub mod child_observation;
#[cfg(unix)]
pub mod fd_transfer;
#[cfg(unix)]
pub mod hosting_epoch;
#[cfg(unix)]
mod transfer;
#[cfg(unix)]
pub mod transfer_manifest;

#[cfg(unix)]
mod embedded_transfer;

#[cfg(unix)]
mod termination_diagnostics;

pub mod clipboard;

pub mod recognizer;
