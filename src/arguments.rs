// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime device policy accompanies, but does not alter, image configuration.

use hyper_service::io::{DISK, NETWORK};
use hyper_vm_policy::fleet::Configuration;

pub struct Arguments {
    pub configuration: Configuration,
    pub io_devices: u32,
}

impl Arguments {
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        has_session: bool,
    ) -> Result<Self, String> {
        let mut args = args.into_iter().peekable();
        let devices = match args
            .peek()
            .and_then(|value| value.strip_prefix("--io-devices="))
        {
            Some(value) => {
                let devices = match value {
                    "1" => DISK,
                    "2" => NETWORK,
                    "3" => DISK | NETWORK,
                    _ => return Err("invalid I/O device selection".into()),
                };
                args.next();
                devices
            }
            None => 0,
        };
        if has_session != (devices != 0) {
            return Err("I/O device selection requires exactly one matching session".into());
        }
        Ok(Self {
            configuration: Configuration::from_runtime_arguments(args)?,
            io_devices: devices,
        })
    }
}

#[cfg(test)]
#[path = "../tests/arguments.rs"]
mod tests;
