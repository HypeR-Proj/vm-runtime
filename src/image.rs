// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read-authorized guest images, payload copies, and device-tree construction.

use crate::error::Error;
use hyper_os::memory::WritableVmo;
use hyper_vm_image::guest_fdt::{self, GuestHardwareMetadata};
use hyper_vm_image::{Payload, ReadAt, linux};
use std::fs::File;
#[cfg(target_os = "hyper")]
use std::os::hyper::fs::FileExt;
#[cfg(unix)]
use std::os::unix::fs::FileExt;

pub(super) struct ImageSource {
    file: File,
    length: u64,
}

impl ImageSource {
    pub(super) fn new(file: hyper_os::fs::File) -> Result<Self, Error> {
        // Image delegation grants READ, not INSPECT. Query length using that
        // existing authority before transferring ownership to std for I/O.
        let length = file.size().map_err(Error::OperatingSystem)?;
        Ok(Self {
            file: file.into_std(),
            length,
        })
    }
}

impl ReadAt for ImageSource {
    type Error = std::io::Error;

    fn length(&self) -> Result<u64, Self::Error> {
        Ok(self.length)
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<(), Self::Error> {
        self.file.read_exact_at(output, offset)
    }
}

#[inline(never)]
fn copy_payload(
    source: &ImageSource,
    memory: &WritableVmo,
    memory_base: u64,
    payload: Payload,
) -> Result<(), Error> {
    let destination = payload
        .load_address
        .checked_sub(memory_base)
        .ok_or(Error::InvalidImage)?;
    let statistics = hyper_vm_support::image_io::copy(
        payload.file_offset,
        payload.length,
        |offset, bytes| source.read_exact_at(offset, bytes),
        |offset, bytes| {
            let offset = destination.checked_add(offset).ok_or(Error::InvalidImage)?;
            memory
                .write_all_at(offset, bytes)
                .map_err(Error::OperatingSystem)
        },
    )
    .map_err(|error| match error {
        hyper_vm_support::image_io::Error::Read(error)
        | hyper_vm_support::image_io::Error::Thread(error) => Error::Io(error.kind()),
        hyper_vm_support::image_io::Error::Write(error) => error,
        hyper_vm_support::image_io::Error::Allocation => Error::Io(std::io::ErrorKind::OutOfMemory),
        hyper_vm_support::image_io::Error::InvalidRange => Error::InvalidImage,
        hyper_vm_support::image_io::Error::WorkerStopped => Error::Io(std::io::ErrorKind::Other),
    })?;
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: payload {} bytes read={} us write={} us",
        payload.length,
        statistics.read.as_micros(),
        statistics.write.as_micros()
    );
    #[cfg(not(feature = "startup-profile"))]
    let _ = statistics;
    Ok(())
}

#[inline(never)]
fn build_device_tree(
    memory: &WritableVmo,
    plan: &linux::BootPlan,
    boot_arguments: &str,
    metadata: GuestHardwareMetadata,
    devices: u32,
    pci: bool,
) -> Result<(), Error> {
    let mut structure = [0u8; 8192];
    let mut strings = [0u8; 2048];
    let mut output = [0u8; 12 * 1024];
    let length = if devices != 0 {
        let GuestHardwareMetadata::Aarch64 { gic_version } = metadata else {
            return Err(Error::UnsupportedConfiguration);
        };
        guest_fdt::build_aarch64_linux_with_io(
            guest_fdt::Aarch64LinuxBoot {
                memory_base: plan.memory_base(),
                memory_size: plan.memory_size(),
                vcpu_count: plan.vcpu_count(),
                gic_version,
                initramfs: plan.initramfs().map(|range| (range.start(), range.end())),
                boot_arguments,
            },
            guest_fdt::io::IoDevices {
                virtual_pci: pci.then_some(guest_fdt::io::VirtualPci {
                    ecam: guest_fdt::io::MmioWindow {
                        base: hyper_service::io::PCI_ECAM,
                        size: hyper_service::io::PCI_ECAM_BYTES,
                    },
                    memory: guest_fdt::io::MmioWindow {
                        base: hyper_service::io::PCI_MEMORY,
                        size: hyper_service::io::PCI_MEMORY_BYTES,
                    },
                    interrupts: &[
                        (1, hyper_service::io::FRONTEND_IRQ),
                        (2, hyper_service::io::FRONTEND_NET_IRQ),
                    ],
                }),
                virtio: (!pci && devices & hyper_service::io::DISK != 0).then_some(
                    guest_fdt::io::MmioDevice {
                        base: hyper_service::io::FRONTEND_MMIO,
                        size: 4096,
                        irq: hyper_service::io::FRONTEND_IRQ,
                    },
                ),
                network: (!pci && devices & hyper_service::io::NETWORK != 0).then_some(
                    guest_fdt::io::MmioDevice {
                        base: hyper_service::io::FRONTEND_NET_MMIO,
                        size: 4096,
                        irq: hyper_service::io::FRONTEND_NET_IRQ,
                    },
                ),
                ..guest_fdt::io::IoDevices::empty()
            },
            &mut structure,
            &mut strings,
            &mut output,
        )
    } else {
        guest_fdt::build_linux(
            plan,
            boot_arguments,
            metadata,
            &mut structure,
            &mut strings,
            &mut output,
        )
    }
    .map_err(|_| Error::InvalidImage)?;
    let offset = plan
        .device_tree()
        .start()
        .checked_sub(plan.memory_base())
        .ok_or(Error::InvalidImage)?;
    memory
        .write_all_at(offset, output.get(..length).ok_or(Error::InvalidImage)?)
        .map_err(Error::OperatingSystem)
}

#[inline(never)]
pub(super) fn prepare_guest_memory(
    source: &ImageSource,
    image: hyper_vm_image::ConfiguredImage,
    plan: &linux::BootPlan,
    metadata: GuestHardwareMetadata,
    devices: u32,
    pci: bool,
    started: std::time::Instant,
) -> Result<WritableVmo, Error> {
    let memory = WritableVmo::create(plan.memory_size()).map_err(Error::OperatingSystem)?;
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: RAM allocated at {} us",
        started.elapsed().as_micros()
    );
    copy_payload(source, &memory, plan.memory_base(), image.kernel)?;
    if let Some(initramfs) = image.initramfs {
        copy_payload(source, &memory, plan.memory_base(), initramfs)?;
    }
    build_device_tree(
        &memory,
        plan,
        image.boot_arguments.as_str(),
        metadata,
        devices,
        pci,
    )?;
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: memory prepared at {} us",
        started.elapsed().as_micros()
    );
    #[cfg(not(feature = "startup-profile"))]
    let _ = started;
    Ok(memory)
}
