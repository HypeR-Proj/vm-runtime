// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Translate image, platform, and guest failures into the manager protocol.

use hyper_service::vm as vm_contract;
use hyper_vm_image::{aarch64_linux, linux, riscv64_linux};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    OperatingSystem(hyper_os::Error),
    Io(std::io::ErrorKind),
    InvalidImage,
    UnsupportedConfiguration,
    InvalidControl,
    Guest(hyper_os::vm::VirtualCpuTermination),
}

impl Error {
    pub(super) const fn failure(self) -> vm_contract::InstanceFailure {
        match self {
            Self::InvalidImage => vm_contract::InstanceFailure::InvalidImage,
            Self::UnsupportedConfiguration => {
                vm_contract::InstanceFailure::UnsupportedConfiguration
            }
            Self::InvalidControl => vm_contract::InstanceFailure::InvalidControlProtocol,
            Self::Guest(hyper_os::vm::VirtualCpuTermination::MemoryFault) => {
                vm_contract::InstanceFailure::GuestMemoryFault
            }
            Self::Guest(hyper_os::vm::VirtualCpuTermination::Mmio) => {
                vm_contract::InstanceFailure::GuestMmio
            }
            Self::Guest(hyper_os::vm::VirtualCpuTermination::Synchronous) => {
                vm_contract::InstanceFailure::GuestSynchronous
            }
            Self::Guest(hyper_os::vm::VirtualCpuTermination::Administrative) => {
                vm_contract::InstanceFailure::UnexpectedAdministrativeStop
            }
            Self::OperatingSystem(_) | Self::Io(_) => vm_contract::InstanceFailure::Runtime,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl From<hyper_vm_policy::image::CachedReadError<std::io::Error>> for Error {
    fn from(error: hyper_vm_policy::image::CachedReadError<std::io::Error>) -> Self {
        use hyper_vm_policy::image::CachedReadError;
        match error {
            CachedReadError::Source(error) => error.into(),
            CachedReadError::Allocation => Self::Io(std::io::ErrorKind::OutOfMemory),
            CachedReadError::InvalidRange => Self::InvalidImage,
            CachedReadError::ReentrantRead => Self::Io(std::io::ErrorKind::Other),
        }
    }
}

pub(super) fn classify_image_error<E: Into<Error>>(error: hyper_vm_image::Error<E>) -> Error {
    match error {
        hyper_vm_image::Error::Source(error) => error.into(),
        hyper_vm_image::Error::UnsupportedImage => Error::UnsupportedConfiguration,
        _ => Error::InvalidImage,
    }
}

pub(super) fn classify_platform_error(error: hyper_os::Error) -> Error {
    if error == hyper_os::Error::Status(hyper_os::Status::NOT_SUPPORTED) {
        Error::UnsupportedConfiguration
    } else {
        Error::OperatingSystem(error)
    }
}

pub(super) fn classify_reference_error<E: Into<Error>>(error: linux::Error<E>) -> Error {
    match error {
        linux::Error::Aarch64(error) => classify_aarch64_reference_error(error),
        linux::Error::Riscv64(error) => classify_riscv64_reference_error(error),
        linux::Error::UnsupportedArchitecture | linux::Error::UnsupportedPlatformProfile => {
            Error::UnsupportedConfiguration
        }
    }
}

fn classify_riscv64_reference_error<E: Into<Error>>(
    error: riscv64_linux::ReferenceLayoutError<E>,
) -> Error {
    use riscv64_linux::{Error as KernelError, ReferenceLayoutError};
    match error {
        ReferenceLayoutError::Source(error)
        | ReferenceLayoutError::Kernel(KernelError::Source(error)) => error.into(),
        ReferenceLayoutError::UnsupportedArchitecture
        | ReferenceLayoutError::UnsupportedPlatformProfile
        | ReferenceLayoutError::UnsupportedVcpuCount
        | ReferenceLayoutError::Kernel(
            KernelError::CompressedPayload
            | KernelError::UnsupportedVersion
            | KernelError::UnsupportedFlags,
        ) => Error::UnsupportedConfiguration,
        _ => Error::InvalidImage,
    }
}

fn classify_aarch64_reference_error<E: Into<Error>>(
    error: aarch64_linux::ReferenceLayoutError<E>,
) -> Error {
    use aarch64_linux::{Error as KernelError, ReferenceLayoutError};

    match error {
        ReferenceLayoutError::Source(error)
        | ReferenceLayoutError::Kernel(KernelError::Source(error)) => error.into(),
        ReferenceLayoutError::UnsupportedArchitecture
        | ReferenceLayoutError::UnsupportedPlatformProfile
        | ReferenceLayoutError::UnsupportedVcpuCount
        | ReferenceLayoutError::Kernel(KernelError::CompressedPayload) => {
            Error::UnsupportedConfiguration
        }
        _ => Error::InvalidImage,
    }
}
