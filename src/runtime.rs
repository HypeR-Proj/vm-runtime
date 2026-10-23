// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Prepare and publish one guest, then supervise its installed lifetime.

use crate::{
    error::{Error, classify_image_error, classify_platform_error, classify_reference_error},
    image::{ImageSource, prepare_guest_memory},
    io_devices,
    supervisor::supervise_guest,
};
use hyper_os::memory::WritableVmo;
use hyper_os::startup::Startup;
use hyper_service::vm as vm_contract;
use hyper_vm_image::linux;
use std::time::Instant;

/// Revalidates the image, installs one guest, and supervises it until termination.
///
/// `Installed` opens I/O broker admission after kernel installation. `Running`
/// is sent only after device binding and boot-vCPU submission; it does not
/// certify that guest code has executed or that guest userspace is ready.
#[inline(never)]
pub(super) fn run(
    startup: &mut Startup<'_>,
    control: &hyper_os::channel::ByteChannel<'_>,
    started: Instant,
) -> Result<(), Error> {
    let io_session = startup
        .take_optional(hyper_service::io::SESSION)
        .map_err(Error::OperatingSystem)?
        .map(hyper_os::capability_channel::CapabilityChannel::from_handle);
    let source = hyper_vm_policy::image::CachedSource::new(ImageSource::new(
        hyper_os::fs::File::from_handle(
            startup
                .take(vm_contract::IMAGE)
                .map_err(Error::OperatingSystem)?,
        ),
    )?)
    .map_err(Error::from)?;
    let lease = startup
        .take(vm_contract::CREATION_LEASE)
        .map_err(Error::OperatingSystem)?;
    let arguments = hyper_vm_runtime::arguments::Arguments::parse(
        std::env::args().skip(1),
        io_session.is_some(),
    )
    .map_err(|_| Error::UnsupportedConfiguration)?;
    let config = arguments.configuration;
    let image = hyper_vm_image::parse(&source).map_err(classify_image_error)?;
    // Recheck at every start in case the image changed after manager admission.
    let image = hyper_vm_policy::image::configure(image, &config).map_err(|error| {
        eprintln!("HypeR vm-runtime: {error}");
        Error::UnsupportedConfiguration
    })?;
    let plan = linux::validate_reference(&source, image).map_err(classify_reference_error)?;
    // Drop this validation view before parallel payload reads. Each start
    // still revalidates the current file; no cache survives the VM lifetime.
    let source = source.into_inner();
    let profile = hyper_vm_support::profile::native_profile(plan.platform_profile())
        .map_err(|_| Error::UnsupportedConfiguration)?;
    let platform_info = hyper_os::vm::platform_info(lease.as_handle_ref(), profile)
        .map_err(classify_platform_error)?;
    let metadata = hyper_vm_support::profile::validate_metadata(
        plan.architecture(),
        plan.platform_profile(),
        platform_info,
    )
    .map_err(|_| Error::UnsupportedConfiguration)?;
    metadata
        .validate_for(&plan)
        .map_err(|_| Error::UnsupportedConfiguration)?;
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: image validated at {} us",
        started.elapsed().as_micros()
    );
    publish_status(control, vm_contract::InstanceStatus::ImageValidated)?;

    let connection = hyper_os::capability_channel::CapabilityChannel::from_handle(
        startup
            .take(vm_contract::CONSOLE_CONNECTION)
            .map_err(Error::OperatingSystem)?,
    );
    let root = startup
        .take(hyper_os::startup::ROOT_VMAR)
        .map_err(Error::OperatingSystem)?;
    let virtual_serial = hyper_os::virtual_serial::create().map_err(Error::OperatingSystem)?;
    let serial_memory = WritableVmo::create(hyper_os::virtual_serial::BUFFER_BYTES)
        .map_err(Error::OperatingSystem)?;
    let output = hyper_os::virtual_serial::Output::register(
        &virtual_serial,
        root.as_handle_ref(),
        0xd000_0000,
        serial_memory,
    )
    .map_err(Error::OperatingSystem)?;
    let mut console = hyper_vm_runtime::console::Console::new(connection, virtual_serial, output);
    let serial_binding = console.binding().map_err(Error::OperatingSystem)?;
    let memory = prepare_guest_memory(
        &source,
        image,
        &plan,
        metadata,
        arguments.io_devices,
        config.virtio_transport == hyper_vm_policy::fleet::VirtioTransport::Pci,
        started,
    )?;
    publish_status(control, vm_contract::InstanceStatus::MemoryPrepared)?;

    let InstalledGuest {
        shared_memory,
        machine,
        vcpus,
    } = install_guest(
        lease,
        &memory,
        &plan,
        hyper_os::vm::Configuration {
            guest_physical_base: plan.memory_base(),
            memory_size: plan.memory_size(),
            vcpu_count: plan.vcpu_count(),
            architecture: platform_info.architecture,
            platform_profile: profile,
        },
        serial_binding,
        io_session.is_some(),
    )?;
    hyper_vm_policy::affinity::apply(&config.affinity, plan.vcpu_count(), |index, words| {
        let cpu = vcpus
            .get(index as usize)
            .ok_or(hyper_os::Error::InvalidResponse)?;
        hyper_os::vm::set_vcpu_affinity(cpu.as_handle_ref(), words)
    })
    .map_err(|error| {
        eprintln!("HypeR vm-runtime: {error}");
        Error::UnsupportedConfiguration
    })?;
    if let Some(scheduling) = config.scheduling {
        for cpu in &vcpus {
            scheduling
                .apply(cpu.as_handle_ref())
                .map_err(Error::OperatingSystem)?;
        }
    }
    // Installation, not image loading, opens the broker admission window.
    // The manager retains the session endpoint until this status is observed.
    #[cfg(feature = "broker-test")]
    if io_session.is_some() {
        // Exceed the broker's 60-second handshake budget before publishing
        // readiness. Ordinary runtime artifacts never include this delay.
        std::thread::sleep(std::time::Duration::from_secs(65));
    }
    #[cfg(feature = "startup-profile")]
    eprintln!(
        "HypeR startup profile: installed at {} us",
        started.elapsed().as_micros()
    );
    publish_status(control, vm_contract::InstanceStatus::Installed)?;
    let (machine, mut devices) = if let Some(session) = io_session {
        let grant = shared_memory.as_ref().ok_or(Error::InvalidControl)?;
        let (machine, devices) = io_devices::bind(
            session,
            machine,
            grant,
            plan.memory_base(),
            plan.memory_size(),
            arguments.io_devices,
            config.virtio_transport == hyper_vm_policy::fleet::VirtioTransport::Pci,
        )?;
        (machine, Some(devices))
    } else {
        (machine, None)
    };
    #[cfg(feature = "test-power-crash")]
    crate::power_crash::at("dormant");
    hyper_os::vm::start_vcpu(vcpus[0].as_handle_ref()).map_err(Error::OperatingSystem)?;
    // This ends at the successful start request, not the first guest entry:
    // scheduling and the EL1/VS transition happen asynchronously in the kernel.
    let elapsed = started.elapsed();
    eprintln!(
        "HypeR vm-runtime: vCPU start submitted in {}.{:03} ms (from main)",
        elapsed.as_millis(),
        elapsed.subsec_micros() % 1_000,
    );
    publish_status(control, vm_contract::InstanceStatus::Running)?;
    supervise_guest(&machine, &vcpus, control, &mut console, devices.as_mut())
}

pub(super) fn publish_status(
    control: &hyper_os::channel::ByteChannel<'_>,
    status: vm_contract::InstanceStatus,
) -> Result<(), Error> {
    control
        .send(&status.encode())
        .map_err(Error::OperatingSystem)
}

struct InstalledGuest {
    shared_memory: Option<hyper_os::OwnedHandle<hyper_os::handle::GuestMemoryObject>>,
    machine: hyper_os::OwnedHandle<hyper_os::handle::VirtualMachineObject>,
    vcpus: Vec<hyper_os::OwnedHandle<hyper_os::handle::VirtualCpuObject>>,
}

/// Builds an installed, dormant guest and retains handles for every vCPU.
///
/// I/O guests use a stable memory grant that can later be shared with the
/// backend. The returned vCPU handles also support stop/retirement observation;
/// opening secondary handles here does not start those CPUs.
fn install_guest(
    lease: hyper_os::OwnedHandle<hyper_os::handle::VirtualMachineCreationLeaseObject>,
    memory: &WritableVmo,
    plan: &linux::BootPlan,
    configuration: hyper_os::vm::Configuration,
    serial_binding: hyper_os::OwnedHandle<hyper_os::handle::VirtualSerialObject>,
    with_io: bool,
) -> Result<InstalledGuest, Error> {
    let pending = hyper_os::vm::create(lease, configuration)
        .map_err(|failure| Error::OperatingSystem(failure.error()))?;
    let shared_memory = if with_io {
        let grant = hyper_os::vm::create_guest_memory(memory.as_handle_ref())
            .map_err(Error::OperatingSystem)?;
        hyper_os::vm::map_guest_memory(
            pending.as_handle_ref(),
            grant.as_handle_ref(),
            0,
            0,
            plan.memory_size(),
        )
        .map_err(Error::OperatingSystem)?;
        Some(grant)
    } else {
        hyper_os::vm::set_memory(pending.as_handle_ref(), memory.as_handle_ref())
            .map_err(Error::OperatingSystem)?;
        None
    };
    hyper_os::vm::set_bootstrap(
        pending.as_handle_ref(),
        hyper_os::vm::VirtualCpuBootstrap {
            entry: plan.kernel_entry(),
            stack: 0,
            arguments: plan.bootstrap_arguments(),
        },
    )
    .map_err(Error::OperatingSystem)?;
    hyper_os::vm::set_virtual_serial(pending.as_handle_ref(), serial_binding)
        .map_err(|failure| Error::OperatingSystem(failure.error()))?;
    hyper_os::vm::seal(pending.as_handle_ref()).map_err(Error::OperatingSystem)?;
    let (machine, vcpu) = hyper_os::vm::install(pending)
        .map_err(|failure| Error::OperatingSystem(failure.error()))?;
    // Keep inspection capabilities through retirement, including secondary CPU
    // failures. No registry lookup or allocation is needed on the stop path.
    let mut vcpus = Vec::with_capacity(plan.vcpu_count() as usize);
    vcpus.push(vcpu);
    for index in 1..plan.vcpu_count() {
        vcpus.push(
            hyper_os::vm::open_vcpu(machine.as_handle_ref(), index)
                .map_err(Error::OperatingSystem)?,
        );
    }
    Ok(InstalledGuest {
        shared_memory,
        machine,
        vcpus,
    })
}
