// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exclusive control-lane ownership for frontends sharing one guest grant.

use hyper_os::vm::MmioRequest;
use hyper_vm_support::io_backend::{
    Backend, Completion, ControlTransport, Error, NotificationControl,
};
mod pci;

/// Once initialized, only this owner may enter a backend's control transport.
/// Queue kicks bypass this slow lane through their independent kernel routes.
pub struct IoBackends<T, N> {
    backends: Vec<Backend<T, N>>,
}

impl<T: ControlTransport, N: NotificationControl> IoBackends<T, N> {
    /// Hello exchanges must finish sequentially before publishing the set.
    pub fn new(backends: Vec<Backend<T, N>>) -> Result<Self, Error> {
        if !(1..=2).contains(&backends.len())
            || backends
                .iter()
                .any(|backend| !backend.ready() || backend.busy())
            || backends.len() == 2 && backends[0].device_id() == backends[1].device_id()
        {
            return Err(Error::InvalidState);
        }
        Ok(Self { backends })
    }

    pub fn use_pci(&mut self) -> Result<(), Error> {
        for backend in &mut self.backends {
            backend.use_pci()?;
        }
        Ok(())
    }

    pub fn busy(&self) -> bool {
        self.backends.iter().any(Backend::busy)
    }

    pub fn wants_write(&self) -> bool {
        self.backends.iter().any(Backend::wants_write)
    }

    /// Readiness may outlive the message that produced it. Only an actual
    /// unsolicited record is a protocol error; a pending transaction remains
    /// the sole reader of its reply, even when this check is called again.
    pub fn check_idle_channel(&self) -> Result<(), Error> {
        if self.busy() {
            return Ok(());
        }
        let mut record = [0; hyper_vm_support::io_protocol::MAX_RECORD];
        match self.backends[0].mailbox().receive(&mut record) {
            Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => Ok(()),
            Err(error) => Err(Error::Native(error)),
            Ok(_) => Err(Error::InvalidState),
        }
    }

    /// At most one transaction is live. Never poll a sibling on its reply lane.
    pub fn progress(&mut self) -> Result<Option<Completion>, Error> {
        match self.backends.iter_mut().find(|backend| backend.busy()) {
            Some(backend) => backend.progress(),
            None => Ok(None),
        }
    }

    pub fn mmio(&mut self, vcpu: usize, request: MmioRequest) -> Result<Option<Completion>, Error> {
        let index = self
            .backends
            .iter()
            .position(|backend| backend.device_id() == request.device)
            .ok_or(Error::InvalidState)?;
        if self.busy() {
            return Ok(None);
        }
        self.backends[index].mmio(vcpu, request)
    }
}

#[cfg(test)]
#[path = "../tests/io_backends.rs"]
mod tests;
