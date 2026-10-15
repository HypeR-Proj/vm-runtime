// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! ECAM/BAR decoding and the PCI configuration access fallback.

use super::*;
use hyper_os::guest_io::FrontendOperation;
use hyper_os::vm::{MmioCompletion, MmioOperation};
use hyper_service::io;
use hyper_vm_support::{pci, virtio_pci};

impl<T: ControlTransport, N: NotificationControl> IoBackends<T, N> {
    /// Queue notifications and ISR accesses use the same admitted kernel source
    /// for direct BAR access and for the mandatory `PCI_CFG` capability.
    pub fn pci_mmio(
        &mut self,
        vcpu: usize,
        request: MmioRequest,
        configure: impl FnMut(u64, u64, u16) -> hyper_os::Result<()>,
        access: impl FnMut(u64, FrontendOperation) -> hyper_os::Result<u32>,
    ) -> Result<Option<Completion>, Error> {
        if self.busy() {
            return Ok(None);
        }
        let width = u8::try_from(request.width).map_err(|_| Error::InvalidState)?;
        if (io::PCI_ECAM..io::PCI_ECAM + io::PCI_ECAM_BYTES).contains(&request.address) {
            let address = request.address - io::PCI_ECAM;
            let slot = (address >> 15) as u8;
            if address & 0x7000 == 0
                && let Some(backend) = self
                    .backends
                    .iter_mut()
                    .find(|backend| backend.pci_slot() == slot)
            {
                return configuration(backend, vcpu, request, width, configure, access);
            }
        } else if (io::PCI_MEMORY..io::PCI_MEMORY + io::PCI_MEMORY_BYTES).contains(&request.address)
        {
            if let Some(backend) = self.backends.iter_mut().find(|backend| {
                backend.pci_function().is_some_and(|function| {
                    function.command() & pci::MEMORY != 0
                        && request.address >= function.bar()
                        && request
                            .address
                            .checked_add(u64::from(width))
                            .is_some_and(|end| end <= function.bar() + pci::BAR_SIZE)
                })
            }) {
                return backend.mmio(
                    vcpu,
                    MmioRequest {
                        device: backend.device_id(),
                        ..request
                    },
                );
            }
        } else {
            return Err(Error::InvalidState);
        }
        absent(vcpu, request, width)
    }
}

fn absent(vcpu: usize, request: MmioRequest, width: u8) -> Result<Option<Completion>, Error> {
    let result = match request.operation {
        MmioOperation::Write(_) => MmioCompletion::Write,
        MmioOperation::Read => MmioCompletion::Read(match width {
            1 => 0xff,
            2 => 0xffff,
            4 => 0xffff_ffff,
            8 => u64::MAX,
            _ => return Err(Error::InvalidState),
        }),
    };
    Ok(Some(Completion {
        vcpu,
        id: request.id,
        result,
    }))
}

fn configuration<T: ControlTransport, N: NotificationControl>(
    backend: &mut Backend<T, N>,
    vcpu: usize,
    request: MmioRequest,
    width: u8,
    mut configure: impl FnMut(u64, u64, u16) -> hyper_os::Result<()>,
    mut access: impl FnMut(u64, FrontendOperation) -> hyper_os::Result<u32>,
) -> Result<Option<Completion>, Error> {
    let offset = request.address & 0xfff;
    let original = backend.original_base();
    match backend.pci_configuration_window(offset, width) {
        Ok(Some(window)) => {
            return indirect(backend, vcpu, request, window, access);
        }
        // Inactive/invalid configuration windows read as absent; e.g. a config
        // space dump must not terminate the VM before a driver selects a BAR.
        Err(Error::Device(hyper_vm_support::virtio::Error::InvalidAccess)) => {
            return absent(vcpu, request, width);
        }
        Err(error) => return Err(error),
        Ok(None) => {}
    }
    let mut completion = Completion {
        vcpu,
        id: request.id,
        result: MmioCompletion::Write,
    };
    match request.operation {
        MmioOperation::Read => {
            let interrupt = offset <= 6
                && offset + u64::from(width) > 6
                && access(original, FrontendOperation::InterruptStatus).map_err(Error::Native)?
                    != 0;
            completion.result =
                MmioCompletion::Read(backend.read_pci_configuration(offset, width, interrupt)?);
        }
        MmioOperation::Write(value) => {
            let transaction = backend.write_pci_configuration(offset, width, value)?;
            let function = backend.pci_function().ok_or(Error::InvalidState)?;
            // An unmapped BAR retains its config value but never gains access
            // outside the preinstalled aperture. Configuration access still works.
            let bar = function.bar();
            let mapped = (io::PCI_MEMORY..io::PCI_MEMORY + io::PCI_MEMORY_BYTES).contains(&bar);
            configure(
                original,
                if mapped { bar } else { original },
                if mapped {
                    function.command()
                } else {
                    function.command() & !pci::MEMORY
                },
            )
            .map_err(Error::Native)?;
            if let Some(transaction) = transaction {
                backend.begin_transaction(transaction, completion)?;
                return Ok(None);
            }
        }
    }
    Ok(Some(completion))
}

fn indirect<T: ControlTransport, N: NotificationControl>(
    backend: &mut Backend<T, N>,
    vcpu: usize,
    request: MmioRequest,
    window: virtio_pci::ConfigurationWindow,
    mut access: impl FnMut(u64, FrontendOperation) -> hyper_os::Result<u32>,
) -> Result<Option<Completion>, Error> {
    let offset = request.address & 0xfff;
    let width = u8::try_from(request.width).map_err(|_| Error::InvalidState)?;
    let operation = match request.operation {
        MmioOperation::Read => MmioOperation::Read,
        MmioOperation::Write(value) => {
            MmioOperation::Write(backend.pci_configuration_write_value(offset, width, value)?)
        }
    };
    let hot = match (window.offset, window.width, operation) {
        (virtio_pci::ISR_OFFSET, 1, MmioOperation::Read) => {
            Some(FrontendOperation::ReadAndClearInterrupt)
        }
        (virtio_pci::NOTIFY_OFFSET, 2, MmioOperation::Write(queue)) => {
            Some(FrontendOperation::NotifyQueue(queue as u16))
        }
        _ => None,
    };
    let mut completion = if let Some(hot) = hot {
        let value = access(backend.original_base(), hot).map_err(Error::Native)?;
        Some(Completion {
            vcpu,
            id: request.id,
            result: match operation {
                MmioOperation::Read => MmioCompletion::Read(u64::from(value)),
                MmioOperation::Write(_) => MmioCompletion::Write,
            },
        })
    } else {
        let bar = backend.pci_function().ok_or(Error::InvalidState)?.bar();
        backend.mmio(
            vcpu,
            MmioRequest {
                device: backend.device_id(),
                address: bar + window.offset,
                width: u32::from(window.width),
                operation,
                ..request
            },
        )?
    };
    // Reads complete locally. A parked backend transaction is a write and
    // retains its original instruction ID until backend quiescence/activation.
    if let Some(completion) = &mut completion
        && let MmioCompletion::Read(value) = completion.result
    {
        completion.result =
            MmioCompletion::Read(backend.pci_configuration_read_result(offset, width, value)?);
    }
    Ok(completion)
}
