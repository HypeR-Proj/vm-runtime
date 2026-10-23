// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::Arguments;
use hyper_vm_policy::fleet::{Affinity, Configuration};

fn configuration() -> Configuration {
    Configuration {
        virtio_transport: Default::default(),
        memory_bytes: 128 * 1024 * 1024,
        vcpus: 1,
        bootargs: "console=ttyAMA0 --io-devices=3".into(),
        scheduling: None,
        affinity: vec![Affinity {
            vcpu: 0,
            cpus: vec![0, 2],
        }],
    }
}

#[test]
fn existing_no_io_arguments_preserve_image_policy() -> Result<(), String> {
    let configuration = configuration();
    let parsed = Arguments::parse(configuration.runtime_arguments()?, false)?;
    assert_eq!(parsed.io_devices, 0);
    assert_eq!(parsed.configuration, configuration);
    Ok(())
}

#[test]
fn one_session_selects_disk_network_or_both() -> Result<(), String> {
    let configuration = configuration();
    for devices in 1..=3 {
        let mut args = vec![format!("--io-devices={devices}")];
        args.extend(configuration.runtime_arguments()?);
        let parsed = Arguments::parse(args, true)?;
        assert_eq!(parsed.io_devices, devices);
        assert_eq!(parsed.configuration, configuration);
    }
    Ok(())
}

#[test]
fn session_and_device_declaration_must_match() -> Result<(), String> {
    let configuration = configuration();
    assert!(Arguments::parse(configuration.runtime_arguments()?, true).is_err());
    for devices in ["", "0", "4", "01", "-1", "network", "3,1"] {
        let mut args = vec![format!("--io-devices={devices}")];
        args.extend(configuration.runtime_arguments()?);
        assert!(Arguments::parse(args, true).is_err());
    }
    let mut declared = vec!["--io-devices=1".into()];
    declared.extend(configuration.runtime_arguments()?);
    assert!(Arguments::parse(declared.clone(), false).is_err());
    declared.insert(0, "--io-devices=2".into());
    assert!(Arguments::parse(declared, true).is_err());
    let mut trailing = configuration.runtime_arguments()?;
    trailing.push("--io-devices=1".into());
    assert!(Arguments::parse(trailing, true).is_err());
    Ok(())
}
