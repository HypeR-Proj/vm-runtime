// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared guest grant, serialized device configuration and direct notifications.

use crate::error::Error;
use hyper_os::capability_channel::{
    CapabilityChannel, CapabilityDisposition, CapabilityReceiveSlot,
};
use hyper_os::handle::{
    ByteChannelObject, GuestMemoryObject, OwnedHandle, RightsOffer, VirtualCpuObject,
    VirtualMachineObject,
};
use hyper_os::wait::{ObjectSignals, RegistrationId, WaitSet};
use hyper_service::io;
use hyper_vm_runtime::io_backends::IoBackends;
use hyper_vm_support::io_backend::{
    Backend, Error as BackendError, OperationDeadline, RemoteNotification,
};
use std::mem::MaybeUninit;
use std::num::NonZeroU64;
use std::sync::Arc;

type IoBackend = Backend<
    Arc<OwnedHandle<ByteChannelObject>>,
    RemoteNotification<Arc<OwnedHandle<ByteChannelObject>>>,
>;

fn backend_failure(stage: &str, error: BackendError) -> Error {
    eprintln!("HypeR vm-runtime: I/O {stage} failed: {error:?}");
    Error::InvalidControl
}

pub(super) struct IoDevices {
    pci: bool,
    _session: CapabilityChannel,
    channel: Arc<OwnedHandle<ByteChannelObject>>,
    backends: IoBackends<
        Arc<OwnedHandle<ByteChannelObject>>,
        RemoteNotification<Arc<OwnedHandle<ByteChannelObject>>>,
    >,
    registrations: Vec<RegistrationId>,
    deadline: OperationDeadline,
}

pub(super) fn bind(
    session: CapabilityChannel,
    machine: OwnedHandle<VirtualMachineObject>,
    memory: &OwnedHandle<GuestMemoryObject>,
    base: u64,
    length: u64,
    devices: u32,
    pci: bool,
) -> Result<(OwnedHandle<VirtualMachineObject>, IoDevices), Error> {
    #[cfg(feature = "startup-profile")]
    let started = std::time::Instant::now();
    let deadline = hyper_os::time::deadline_after(std::time::Duration::from_secs(60))
        .map_err(Error::OperatingSystem)?
        .as_raw();
    let mut machine = Some(machine);
    let record = (if pci {
        io::encode_pci_memory(base, length)
    } else {
        io::encode_memory(base, length)
    })
    .ok_or(Error::InvalidControl)?;
    let dispositions = &mut [
        CapabilityDisposition::move_handle(&mut machine, RightsOffer::Exact(io::MACHINE_RIGHTS))
            .map_err(Error::OperatingSystem)?,
        CapabilityDisposition::duplicate(
            memory.as_handle_ref(),
            RightsOffer::Exact(io::MEMORY_RIGHTS),
        )
        .map_err(Error::OperatingSystem)?,
    ];
    io::send_capabilities(&session, &record, dispositions, deadline)
        .map_err(Error::OperatingSystem)?;
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: I/O memory sent at {} us",
        started.elapsed().as_micros()
    );
    let mut bytes = [MaybeUninit::uninit(); io::BOUND_BYTES];
    let mut slots = [
        CapabilityReceiveSlot::new::<VirtualMachineObject>(io::MACHINE_RIGHTS),
        CapabilityReceiveSlot::new::<ByteChannelObject>(io::MAILBOX_RIGHTS),
    ];
    let message = session
        .receive(deadline, &mut bytes, &mut slots)
        .map_err(Error::OperatingSystem)?;
    if message.capability_count() != 2 {
        return Err(Error::InvalidControl);
    }
    let binding = io::Binding::decode(message.bytes())
        .filter(|binding| binding.devices == devices)
        .ok_or(Error::InvalidControl)?;
    let generation = NonZeroU64::new(binding.generation).ok_or(Error::InvalidControl)?;
    let machine = slots[0]
        .take::<VirtualMachineObject>()
        .map_err(Error::OperatingSystem)?
        .ok_or(Error::InvalidControl)?;
    let channel = slots[1]
        .take::<ByteChannelObject>()
        .map_err(Error::OperatingSystem)?
        .ok_or(Error::InvalidControl)?;
    let channel = Arc::new(channel);
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: I/O bound at {} us",
        started.elapsed().as_micros()
    );
    let mut backends = Vec::with_capacity(2);
    for device in [io::DISK, io::NETWORK] {
        let Some(identity) = binding.device_id(device) else {
            continue;
        };
        let identity = NonZeroU64::new(identity).ok_or(Error::InvalidControl)?;
        let mut backend = if device == io::DISK {
            Backend::new(
                channel.clone(),
                RemoteNotification::new(channel.clone()),
                (base, length),
                io::FRONTEND_MMIO,
                identity,
            )
        } else {
            Backend::network(
                channel.clone(),
                RemoteNotification::network(channel.clone()),
                (base, length),
                io::FRONTEND_NET_MMIO,
                identity,
                generation,
            )
        }
        .map_err(|error| backend_failure("construction", error))?;
        // The same lane carries both Hello exchanges. Finish one before even
        // constructing a sibling's pending transaction.
        initialize(&mut backend, deadline)?;
        backends.push(backend);
    }
    let mut backends =
        IoBackends::new(backends).map_err(|error| backend_failure("publication", error))?;
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: I/O ready at {} us",
        started.elapsed().as_micros()
    );
    if pci {
        backends
            .use_pci()
            .map_err(|error| backend_failure("PCI transport", error))?;
    }
    Ok((
        machine,
        IoDevices {
            pci,
            _session: session,
            channel,
            backends,
            registrations: Vec::new(),
            deadline: OperationDeadline::default(),
        },
    ))
}

fn initialize(backend: &mut IoBackend, deadline: u64) -> Result<(), Error> {
    while !backend.ready() {
        backend
            .progress()
            .map_err(|error| backend_failure("initialization", error))?;
        if backend.ready() {
            break;
        }
        if hyper_os::time::monotonic_now()
            .map_err(Error::OperatingSystem)?
            .as_nanoseconds()
            >= deadline
        {
            eprintln!("HypeR vm-runtime: I/O initialization deadline expired");
            return Err(Error::InvalidControl);
        }
        let signals = ObjectSignals::<ByteChannelObject>::READABLE
            .union(ObjectSignals::<ByteChannelObject>::PEER_CLOSED);
        let signals = if backend.wants_write() {
            signals.union(ObjectSignals::<ByteChannelObject>::WRITABLE)
        } else {
            signals
        };
        hyper_os::wait::wait_many(
            &[hyper_os::wait::WaitItem::new(
                backend.mailbox().as_handle_ref(),
                signals,
            )],
            deadline,
        )
        .map_err(Error::OperatingSystem)?;
    }
    Ok(())
}

impl IoDevices {
    pub(super) fn service(
        &mut self,
        machine: &OwnedHandle<VirtualMachineObject>,
        cpus: &[OwnedHandle<VirtualCpuObject>],
    ) -> Result<(), Error> {
        if let Some(completion) = self
            .backends
            .progress()
            .map_err(|error| backend_failure("progress", error))?
        {
            hyper_os::vm::complete_mmio(
                cpus.get(completion.vcpu)
                    .ok_or(Error::InvalidControl)?
                    .as_handle_ref(),
                completion.id,
                completion.result,
            )
            .map_err(Error::OperatingSystem)?;
        }
        // Observe completion before enforcing the original operation budget.
        // A readable console or unrelated vCPU cannot extend this deadline.
        self.update_deadline()?;
        for (index, cpu) in cpus.iter().enumerate() {
            if self.backends.busy() {
                break;
            }
            if let Some(request) =
                hyper_os::vm::pending_mmio(cpu.as_handle_ref()).map_err(Error::OperatingSystem)?
                && let Some(completion) = (if self.pci {
                    self.backends.pci_mmio(
                        index,
                        request,
                        |original, base, command| {
                            use hyper_vm_support::pci;
                            hyper_os::guest_io::configure_pci_frontend(
                                machine.as_handle_ref(),
                                original,
                                base,
                                command & pci::MEMORY != 0,
                                command & pci::BUS_MASTER != 0,
                                command & pci::INTX_DISABLE != 0,
                            )
                        },
                        |original, operation| {
                            hyper_os::guest_io::access_frontend(
                                machine.as_handle_ref(),
                                original,
                                operation,
                            )
                        },
                    )
                } else {
                    self.backends.mmio(index, request)
                })
                .map_err(|error| {
                    eprintln!("HypeR vm-runtime: I/O MMIO vCPU={index} request={request:?}");
                    backend_failure("MMIO", error)
                })?
            {
                hyper_os::vm::complete_mmio(cpu.as_handle_ref(), completion.id, completion.result)
                    .map_err(Error::OperatingSystem)?;
            }
        }
        self.update_deadline()?;
        Ok(())
    }

    fn update_deadline(&mut self) -> Result<(), Error> {
        if !self.backends.busy() {
            return self
                .deadline
                .observe(false, 0)
                .map_err(|error| backend_failure("idle deadline", error));
        }
        let now = hyper_os::time::monotonic_now()
            .map_err(Error::OperatingSystem)?
            .as_nanoseconds();
        self.deadline
            .observe(true, now)
            .map_err(|error| backend_failure("operation deadline", error))
    }

    pub(super) fn deadline(&self) -> u64 {
        self.deadline.raw()
    }

    pub(super) fn prepare_wait(
        &mut self,
        waits: &WaitSet,
        cpus: &[OwnedHandle<VirtualCpuObject>],
    ) -> Result<(), Error> {
        for id in self.registrations.drain(..) {
            waits.remove(id).map_err(Error::OperatingSystem)?;
        }
        let signals = ObjectSignals::<ByteChannelObject>::READABLE
            .union(ObjectSignals::<ByteChannelObject>::PEER_CLOSED);
        let signals = if self.backends.wants_write() {
            signals.union(ObjectSignals::<ByteChannelObject>::WRITABLE)
        } else {
            signals
        };
        self.registrations.push(
            waits
                .add(self.channel.as_handle_ref(), signals)
                .map_err(Error::OperatingSystem)?,
        );
        if !self.backends.busy() {
            for cpu in cpus {
                self.registrations.push(
                    waits
                        .add(
                            cpu.as_handle_ref(),
                            ObjectSignals::<VirtualCpuObject>::MMIO_REQUEST,
                        )
                        .map_err(Error::OperatingSystem)?,
                );
            }
        }
        Ok(())
    }
    pub(super) fn observe(
        &self,
        registration: RegistrationId,
        signals: u64,
    ) -> Result<bool, Error> {
        if self.registrations.first() == Some(&registration) {
            if ObjectSignals::<ByteChannelObject>::PEER_CLOSED.is_present_in(signals) {
                // Closure is terminal for both devices. Runtime retirement
                // still owns guest/grant quiescence.
                return Err(backend_failure("channel", BackendError::Disconnected));
            }
            if ObjectSignals::<ByteChannelObject>::READABLE.is_present_in(signals) {
                // Signal publication can lag consumption. Recheck the queue;
                // the backend owner never steals a pending transaction's reply.
                self.backends
                    .check_idle_channel()
                    .map_err(|error| backend_failure("idle channel", error))?;
            }
        }
        Ok(self.registrations.contains(&registration))
    }
}
