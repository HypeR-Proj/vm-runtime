// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime-owned console retention, client transport, and batched input.

use hyper_os::capability_channel::{CapabilityChannel, CapabilityReceiveSlot};
use hyper_os::handle::{ByteChannelObject, OwnedHandle, Rights, VirtualSerialObject};
use hyper_os::{Error, Result, Status};
use hyper_service::vm;
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::time::Duration;

#[cfg(feature = "test-runtime-crash")]
#[path = "../tests/crash.rs"]
mod crash;

#[path = "console_wait.rs"]
mod readiness;
use hyper_os::wait::{ObjectSignals, RegistrationId, WaitSet};
use readiness::Subscription;

const RETAIN_BYTES: usize = 64 * 1024;
const BATCH: usize = hyper_os::channel::MAX_MESSAGE_BYTES;

pub struct Console {
    connection: CapabilityChannel,
    serial: OwnedHandle<VirtualSerialObject>,
    output: hyper_os::virtual_serial::Output,
    client: Option<OwnedHandle<ByteChannelObject>>,
    serial_wait: Subscription,
    client_wait: Subscription,
    serial_closed: bool,
    client_output_closed: bool,
    retained: VecDeque<u8>,
    input: VecDeque<u8>,
}

impl Console {
    #[must_use]
    pub fn new(
        connection: CapabilityChannel,
        serial: OwnedHandle<VirtualSerialObject>,
        output: hyper_os::virtual_serial::Output,
    ) -> Self {
        Self {
            connection,
            serial,
            output,
            client: None,
            serial_wait: Subscription::new(),
            client_wait: Subscription::new(),
            serial_closed: false,
            client_output_closed: false,
            retained: VecDeque::with_capacity(RETAIN_BYTES),
            input: VecDeque::with_capacity(BATCH),
        }
    }

    pub fn binding(&self) -> Result<OwnedHandle<VirtualSerialObject>> {
        self.serial
            .duplicate(Rights::ASSIGN_DEVICE.union(Rights::TRANSFER))
    }

    pub fn attach(&mut self, waits: &WaitSet) -> Result<()> {
        let mut bytes = [MaybeUninit::uninit(); vm::MESSAGE_BYTES];
        let mut slots = [CapabilityReceiveSlot::new::<ByteChannelObject>(
            vm::CONSOLE_SESSION_RIGHTS,
        )];
        let deadline = hyper_os::time::deadline_after(Duration::from_millis(100))?.as_raw();
        let message = match self.connection.receive(deadline, &mut bytes, &mut slots) {
            Ok(message) => message,
            // A disappearing manager/client cannot strand the VM runtime.
            Err(Error::Status(Status::TIMED_OUT | Status::PEER_CLOSED)) => return Ok(()),
            Err(error) => return Err(error),
        };
        if vm::ConsoleCapability::decode(message.bytes()).is_none()
            || message.capability_count() != 1
        {
            return Err(Error::InvalidResponse);
        }
        self.client_wait.remove(waits)?;
        self.client = slots[0].take::<ByteChannelObject>()?;
        self.client_output_closed = false;
        self.input.clear();
        Ok(())
    }

    /// Keep persistent subscriptions only for work the next service pass can
    /// perform. Full guest input must not spin on a readable client channel.
    pub fn prepare_wait(&mut self, waits: &WaitSet) -> Result<()> {
        if self.serial_closed {
            self.serial_wait.remove(waits)?;
        } else {
            let mut signals = ObjectSignals::<VirtualSerialObject>::READABLE
                .union(ObjectSignals::<VirtualSerialObject>::PEER_CLOSED);
            let input_pending = !self.input.is_empty();
            if input_pending {
                signals = signals.union(ObjectSignals::<VirtualSerialObject>::WRITABLE);
            }
            self.serial_wait.update(
                waits,
                self.serial.as_handle_ref(),
                signals,
                u8::from(input_pending),
            )?;
        }
        let readable = self.input.is_empty();
        let writable = !self.retained.is_empty() && !self.client_output_closed;
        let signals = match (readable, writable) {
            (true, true) => Some(
                ObjectSignals::<ByteChannelObject>::READABLE
                    .union(ObjectSignals::<ByteChannelObject>::WRITABLE),
            ),
            (true, false) => Some(ObjectSignals::<ByteChannelObject>::READABLE),
            (false, true) => Some(ObjectSignals::<ByteChannelObject>::WRITABLE),
            (false, false) => None,
        };
        if let (Some(client), Some(signals)) = (&self.client, signals) {
            self.client_wait.update(
                waits,
                client.as_handle_ref(),
                signals.union(ObjectSignals::<ByteChannelObject>::PEER_CLOSED),
                u8::from(readable) | (u8::from(writable) << 1),
            )?;
        } else {
            self.client_wait.remove(waits)?;
        }
        Ok(())
    }

    pub fn observe(&mut self, registration: RegistrationId, signals: u64) {
        if self.serial_wait.consume(registration)
            && ObjectSignals::<VirtualSerialObject>::PEER_CLOSED.is_present_in(signals)
        {
            self.serial_closed = true;
            self.input.clear();
        }
        self.client_wait.consume(registration);
    }

    /// Bounded nonblocking work after readiness. A full client channel never blocks
    /// collection or VM lifecycle control; only successfully sent bytes drain.
    pub fn service(&mut self) -> Result<()> {
        let mut bytes = [0; BATCH];
        // Bound the work even if the guest continuously produces output.
        for _ in 0..8 {
            let count = self.output.try_read(&mut bytes)?;
            retain(&mut self.retained, &bytes[..count]);
            if count < BATCH {
                break;
            }
        }
        if !self.client_output_closed
            && let Some(client) = &self.client
        {
            for _ in 0..8 {
                let count = self.retained.len().min(BATCH);
                if count == 0 {
                    break;
                }
                for (out, byte) in bytes[..count].iter_mut().zip(self.retained.iter()) {
                    *out = *byte;
                }
                match client.as_byte_channel().try_send(&bytes[..count]) {
                    Ok(()) => {
                        self.retained.drain(..count);
                        #[cfg(feature = "test-runtime-crash")]
                        crash::after_output(count);
                    }
                    Err(Error::Status(Status::WOULD_BLOCK)) => break,
                    // The peer may have closed after queuing input. Keep its
                    // endpoint until receive drains that prefix; output-peer
                    // closure must not discard already accepted keyboard data.
                    Err(Error::Status(Status::PEER_CLOSED)) => {
                        self.client_output_closed = true;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if self.input.is_empty()
            && let Some(client) = &self.client
        {
            match client.as_byte_channel().try_receive(&mut bytes) {
                Ok(count) => {
                    #[cfg(feature = "test-runtime-crash")]
                    crash::after_input(&bytes[..count]);
                    self.input.extend(&bytes[..count]);
                }
                Err(Error::Status(Status::WOULD_BLOCK)) => {}
                Err(Error::Status(Status::PEER_CLOSED)) => {
                    self.client = None;
                }
                Err(error) => return Err(error),
            }
        }
        if !self.input.is_empty() {
            match hyper_os::virtual_serial::try_write(
                self.serial.as_handle_ref(),
                self.input.make_contiguous(),
            ) {
                Ok(count) => {
                    self.input.drain(..count);
                }
                Err(Error::Status(Status::BUSY)) => {}
                Err(Error::Status(Status::BAD_STATE)) => self.input.clear(),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn retain(queue: &mut VecDeque<u8>, bytes: &[u8]) {
    let excess = queue
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(RETAIN_BYTES);
    queue.drain(..excess.min(queue.len()));
    queue.extend(bytes.iter().rev().take(RETAIN_BYTES).rev().copied());
}

#[cfg(test)]
#[path = "../tests/console.rs"]
mod tests;
