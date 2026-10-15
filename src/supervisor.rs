// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded guest service, control requests, and quiescent terminal reporting.

use crate::{error::Error, io_devices, runtime::publish_status};
use hyper_os::handle::{ByteChannelObject, VirtualCpuObject, VirtualMachineObject};
use hyper_os::wait::{ObjectSignals, WaitSet};
use hyper_service::vm as vm_contract;

pub(super) fn supervise_guest(
    machine: &hyper_os::OwnedHandle<hyper_os::handle::VirtualMachineObject>,
    vcpus: &[hyper_os::OwnedHandle<VirtualCpuObject>],
    control: &hyper_os::channel::ByteChannel<'_>,
    console: &mut hyper_vm_runtime::console::Console,
    mut devices: Option<&mut io_devices::IoDevices>,
) -> Result<(), Error> {
    let waits = WaitSet::new(6 + vcpus.len()).map_err(Error::OperatingSystem)?;
    let control_wait = waits
        .add(
            control.as_handle_ref(),
            ObjectSignals::<ByteChannelObject>::READABLE
                .union(ObjectSignals::<ByteChannelObject>::PEER_CLOSED),
        )
        .map_err(Error::OperatingSystem)?;
    let machine_wait = waits
        .add(
            machine.as_handle_ref(),
            ObjectSignals::<VirtualMachineObject>::POWER_REQUEST
                .union(ObjectSignals::<VirtualMachineObject>::VCPU_TERMINATED),
        )
        .map_err(Error::OperatingSystem)?;
    let mut control_consumed = false;
    loop {
        if let Some(devices) = devices.as_mut() {
            devices.service(machine, vcpus)?;
            devices.prepare_wait(&waits, vcpus)?;
        }
        console.service().map_err(Error::OperatingSystem)?;
        console
            .prepare_wait(&waits)
            .map_err(Error::OperatingSystem)?;
        if control_consumed {
            waits.rearm(control_wait).map_err(Error::OperatingSystem)?;
            control_consumed = false;
        }
        let observation = waits
            .wait(
                devices
                    .as_ref()
                    .map_or(hyper_os::DEADLINE_INFINITE, |devices| devices.deadline()),
            )
            .map_err(Error::OperatingSystem)?;
        if let Some(devices) = devices.as_ref()
            && devices.observe(observation.registration, observation.signals)?
        {
            continue;
        }
        if observation.registration != control_wait && observation.registration != machine_wait {
            console.observe(observation.registration, observation.signals);
            continue;
        }
        if observation.registration == control_wait {
            control_consumed = true;
            match handle_control(
                machine,
                vcpus,
                control,
                console,
                &waits,
                observation.signals,
            )? {
                ServiceAction::Continue => continue,
                ServiceAction::Stopped => return Ok(()),
            }
        }
        if observation.registration != machine_wait {
            return Err(Error::InvalidControl);
        }
        if ObjectSignals::<VirtualMachineObject>::VCPU_TERMINATED.is_present_in(observation.signals)
        {
            return Err(Error::Guest(stop_and_retire(machine, vcpus)?));
        }
        if let ServiceAction::Stopped = service_power_requests(machine, vcpus, control)? {
            return Ok(());
        }
        waits.rearm(machine_wait).map_err(Error::OperatingSystem)?;
    }
}

/// Whether this service turn completed the guest lifetime.
enum ServiceAction {
    Continue,
    Stopped,
}

fn handle_control(
    machine: &hyper_os::OwnedHandle<VirtualMachineObject>,
    vcpus: &[hyper_os::OwnedHandle<VirtualCpuObject>],
    control: &hyper_os::channel::ByteChannel<'_>,
    console: &mut hyper_vm_runtime::console::Console,
    waits: &WaitSet,
    signals: u64,
) -> Result<ServiceAction, Error> {
    if !ObjectSignals::<ByteChannelObject>::READABLE.is_present_in(signals) {
        let _terminal = stop_and_retire(machine, vcpus)?;
        return Err(Error::InvalidControl);
    }
    let mut message = [0u8; vm_contract::CONTROL_BYTES];
    let length = match control.try_receive(&mut message) {
        Ok(length) => length,
        Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => {
            return Ok(ServiceAction::Continue);
        }
        Err(error) => return Err(Error::OperatingSystem(error)),
    };
    if let Some(request) = vm_contract::ObservationRequest::decode(&message[..length]) {
        let info =
            hyper_os::vm::machine_info(machine.as_handle_ref()).map_err(Error::OperatingSystem)?;
        let reply = vm_contract::Observation {
            request,
            vcpus: info.vcpu_count,
            capacity_bytes: info.memory_size,
            resident_bytes: info.resident_memory_bytes,
        }
        .encode();
        // Inspection must never block guest service or retirement if a
        // manager times out or stops consuming replies.
        match control.try_send(&reply) {
            Ok(()) | Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => {}
            Err(error) => return Err(Error::OperatingSystem(error)),
        }
        return Ok(ServiceAction::Continue);
    }
    if let Some(request) = vm_contract::VcpuControlRequest::decode(&message[..length]) {
        let reply = hyper_vm_runtime::control::handle(
            request,
            vcpus.len(),
            |vcpu, words| {
                hyper_os::vm::set_vcpu_affinity(vcpus[vcpu as usize].as_handle_ref(), words)
            },
            |vcpu| {
                hyper_os::vm::vcpu_info(vcpus[vcpu as usize].as_handle_ref())
                    .map(|info| (info.host_cpu, info.migration_target))
            },
        );
        match control.try_send(&reply.encode()) {
            Ok(()) | Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => {}
            Err(error) => return Err(Error::OperatingSystem(error)),
        }
        return Ok(ServiceAction::Continue);
    }
    let command = message
        .get(..length)
        .and_then(vm_contract::InstanceCommand::decode)
        .ok_or(Error::InvalidControl)?;
    if command == vm_contract::InstanceCommand::AttachConsole {
        console.attach(waits).map_err(Error::OperatingSystem)?;
        return Ok(ServiceAction::Continue);
    }
    let terminal = stop_and_retire(machine, vcpus)?;
    if terminal != hyper_os::vm::VirtualCpuTermination::Administrative {
        return Err(Error::Guest(terminal));
    }
    publish_status(control, vm_contract::InstanceStatus::Stopped)?;
    Ok(ServiceAction::Stopped)
}

fn service_power_requests(
    machine: &hyper_os::OwnedHandle<VirtualMachineObject>,
    vcpus: &[hyper_os::OwnedHandle<VirtualCpuObject>],
    control: &hyper_os::channel::ByteChannel<'_>,
) -> Result<ServiceAction, Error> {
    // A guest may submit another request immediately after completion.
    // Bound each drain so control/console observations remain serviceable.
    for _ in 0..vcpus.len() {
        let Some(request) = hyper_os::vm::pending_power_request(machine.as_handle_ref())
            .map_err(Error::OperatingSystem)?
        else {
            break;
        };
        use hyper_os::vm::PowerOperation;
        match request.operation {
            PowerOperation::CpuOn | PowerOperation::CpuOff => {
                #[cfg(feature = "test-power-crash")]
                if request.operation == PowerOperation::CpuOn {
                    crate::power_crash::at("pending");
                }
                hyper_os::vm::complete_power_request(machine.as_handle_ref(), request.id, true)
                    .map_err(Error::OperatingSystem)?;
                #[cfg(feature = "test-power-crash")]
                if request.operation == PowerOperation::CpuOff {
                    crate::power_crash::at("powered-off");
                }
            }
            PowerOperation::SystemOff | PowerOperation::SystemReset => {
                // Never resume the requesting CPU after a successful system
                // power operation. Retire every CPU before reporting terminal
                // status; the manager owns authority for a fresh reboot lease.
                let terminal = stop_and_retire(machine, vcpus)?;
                if terminal != hyper_os::vm::VirtualCpuTermination::Administrative {
                    return Err(Error::Guest(terminal));
                }
                let status = if request.operation == PowerOperation::SystemReset {
                    vm_contract::InstanceStatus::RebootRequested
                } else {
                    vm_contract::InstanceStatus::Stopped
                };
                publish_status(control, status)?;
                return Ok(ServiceAction::Stopped);
            }
        }
    }
    Ok(ServiceAction::Continue)
}

fn stop_and_retire(
    machine: &hyper_os::OwnedHandle<VirtualMachineObject>,
    vcpus: &[hyper_os::OwnedHandle<VirtualCpuObject>],
) -> Result<hyper_os::vm::VirtualCpuTermination, Error> {
    hyper_os::vm::request_stop(machine.as_handle_ref()).map_err(Error::OperatingSystem)?;
    hyper_os::vm::wait_terminated(machine.as_handle_ref(), hyper_os::DEADLINE_INFINITE)
        .map_err(Error::OperatingSystem)?;
    let mut result = hyper_os::vm::VirtualCpuTermination::Administrative;
    for cpu in vcpus {
        let terminal = terminated_vcpu_reason(cpu)?;
        if terminal != hyper_os::vm::VirtualCpuTermination::Administrative {
            result = terminal;
        }
    }
    Ok(result)
}

fn terminated_vcpu_reason(
    vcpu: &hyper_os::OwnedHandle<VirtualCpuObject>,
) -> Result<hyper_os::vm::VirtualCpuTermination, Error> {
    hyper_os::vm::vcpu_info(vcpu.as_handle_ref())
        .map_err(Error::OperatingSystem)?
        .terminal
        .ok_or(Error::InvalidControl)
}
