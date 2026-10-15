// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-VM runtime process entry.

#[cfg(target_os = "hyper")]
extern crate hyper_vm_policy_shared as hyper_vm_policy;
#[cfg(target_os = "hyper")]
extern crate hyper_vm_support_shared as hyper_vm_support;

mod error;
mod image;
mod io_devices;
mod runtime;
mod supervisor;

#[cfg(feature = "test-power-crash")]
#[path = "../tests/power_crash.rs"]
mod power_crash;

use hyper_os::startup::Startup;
use hyper_service::vm as vm_contract;
use std::process::ExitCode;
use std::time::Instant;

fn application_main(mut startup: Startup<'_>, started: Instant) -> ExitCode {
    if hyper_os::require_core_abi().is_err() {
        return ExitCode::FAILURE;
    }
    let control = match startup.take(vm_contract::INSTANCE_CONTROL) {
        Ok(control) => control,
        Err(_) => return ExitCode::FAILURE,
    };
    let channel = control.as_byte_channel();
    eprintln!("HypeR vm-runtime: starting");
    match runtime::run(&mut startup, &channel, started) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("HypeR vm-runtime: failed: {error:?}");
            let _ = channel.send(&vm_contract::InstanceStatus::Failed(error.failure()).encode());
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let started = Instant::now();
    match hyper_rt::process::startup() {
        Ok(startup) => application_main(startup, started),
        Err(_) => ExitCode::FAILURE,
    }
}
