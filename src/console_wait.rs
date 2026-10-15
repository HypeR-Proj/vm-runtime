// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One persistent subscription whose actionable signal mask may change.

use hyper_os::Result;
use hyper_os::handle::{HandleRef, ObjectType};
use hyper_os::wait::{ObjectSignals, RegistrationId, WaitSet};

pub(super) struct Subscription {
    current: Option<(RegistrationId, u8)>,
    consumed: bool,
}

impl Subscription {
    pub(super) const fn new() -> Self {
        Self {
            current: None,
            consumed: false,
        }
    }

    pub(super) fn remove(&mut self, waits: &WaitSet) -> Result<()> {
        if let Some((id, _)) = self.current {
            waits.remove(id)?;
            self.current = None;
        }
        self.consumed = false;
        Ok(())
    }

    /// The owner removes a subscription before replacing its source handle.
    /// `key` identifies the complete signal mask for that source.
    pub(super) fn update<T: ObjectType>(
        &mut self,
        waits: &WaitSet,
        source: HandleRef<'_, T>,
        signals: ObjectSignals<T>,
        key: u8,
    ) -> Result<()> {
        if let Some((id, previous)) = self.current
            && previous == key
        {
            if self.consumed {
                waits.rearm(id)?;
                self.consumed = false;
            }
            return Ok(());
        }
        self.remove(waits)?;
        self.current = Some((waits.add(source, signals)?, key));
        Ok(())
    }

    pub(super) fn consume(&mut self, id: RegistrationId) -> bool {
        if self.current.is_some_and(|(current, _)| current == id) {
            self.consumed = true;
            true
        } else {
            false
        }
    }
}
