// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Business VM control, console, and shared I/O session ownership.

#[cfg(target_os = "hyper")]
extern crate hyper_vm_policy_shared as hyper_vm_policy;
#[cfg(target_os = "hyper")]
extern crate hyper_vm_support_shared as hyper_vm_support;

pub mod arguments;
pub mod console;
pub mod control;
pub mod io_backends;
