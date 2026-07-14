//! Hardware-independent policy extracted from Gamescope.
//!
//! This crate intentionally has no Wayland, Vulkan, DRM, X11, or event-loop
//! dependencies. Protocol dispatchers and backends can drive these state
//! machines while unit tests exercise the compatibility-sensitive behavior.

#![forbid(unsafe_code)]

pub mod action_binding;
pub mod control;
pub mod input_method;
pub mod ratio;
pub mod refresh_rate;
pub mod scaling;
pub mod swapchain;
pub mod wire;
