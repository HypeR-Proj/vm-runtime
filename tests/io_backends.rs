// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::IoBackends;
use hyper_os::vm::{MmioCompletion, MmioOperation, MmioRequest};
use hyper_service::io;
use hyper_vm_support::io_backend::{Backend, ControlTransport, Error, RemoteNotification};
use hyper_vm_support::io_protocol::{self, Command, Request};
use hyper_vm_support::virtio::{self, DeviceKind};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::rc::Rc;

#[derive(Default)]
struct State {
    sent: Vec<Vec<u8>>,
    replies: VecDeque<Vec<u8>>,
    receives: usize,
    closed: bool,
}

#[derive(Clone, Default)]
struct Transport(Rc<RefCell<State>>);

impl ControlTransport for Transport {
    fn send(&self, bytes: &[u8]) -> hyper_os::Result<()> {
        self.0.borrow_mut().sent.push(bytes.to_vec());
        Ok(())
    }

    fn receive(&self, bytes: &mut [u8]) -> hyper_os::Result<usize> {
        let mut state = self.0.borrow_mut();
        state.receives += 1;
        let reply = state
            .replies
            .pop_front()
            .ok_or(hyper_os::Error::Status(if state.closed {
                hyper_os::Status::PEER_CLOSED
            } else {
                hyper_os::Status::WOULD_BLOCK
            }))?;
        bytes
            .get_mut(..reply.len())
            .ok_or(hyper_os::Error::InvalidResponse)?
            .copy_from_slice(&reply);
        Ok(reply.len())
    }
}

impl Transport {
    fn notification_reply(&self, kind: DeviceKind) {
        let mut reply = b"HIONOTR1".to_vec();
        reply.extend_from_slice(&7u32.to_le_bytes());
        reply.extend_from_slice(&(kind as u32).to_le_bytes());
        self.0.borrow_mut().replies.push_back(reply);
    }

    fn protocol_reply(&self) -> Result<Request, Error> {
        let mut state = self.0.borrow_mut();
        let request = Request::decode(state.sent.last().ok_or(Error::InvalidState)?)
            .map_err(Error::Protocol)?;
        let mut bytes = [0; io_protocol::MAX_RECORD];
        request.encode(&mut bytes).map_err(Error::Protocol)?;
        let length: u32 = match request.command {
            Command::Hello => 64,
            Command::NetworkHello => 80,
            _ => 48,
        };
        bytes[8..12].copy_from_slice(&length.to_le_bytes());
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        bytes[40..48].fill(0);
        if matches!(request.command, Command::Hello | Command::NetworkHello) {
            bytes[48..56].copy_from_slice(&virtio::VERSION_1.to_le_bytes());
            bytes[56..60].copy_from_slice(
                &(request.command.device_kind().queue_count() as u32).to_le_bytes(),
            );
            bytes[60..64].copy_from_slice(&virtio::QUEUE_MAX.to_le_bytes());
        }
        if request.command == Command::NetworkHello {
            bytes[64..70].copy_from_slice(&[2, 0x48, 0x59, 0, 0, 1]);
            bytes[70..72].copy_from_slice(&1500u16.to_le_bytes());
        }
        state.replies.push_back(bytes[..length as usize].to_vec());
        Ok(request)
    }
}

type TestBackend = Backend<Transport, RemoteNotification<Transport>>;

fn backend(transport: &Transport, kind: DeviceKind) -> Result<TestBackend, Error> {
    match kind {
        DeviceKind::Console => Err(Error::InvalidState),
        DeviceKind::Scsi => Backend::new(
            transport.clone(),
            RemoteNotification::new(transport.clone()),
            (0x4000_0000, 0x20_0000),
            io::FRONTEND_MMIO,
            NonZeroU64::MIN,
        ),
        DeviceKind::Network => Backend::network(
            transport.clone(),
            RemoteNotification::network(transport.clone()),
            (0x4000_0000, 0x20_0000),
            io::FRONTEND_NET_MMIO,
            NonZeroU64::new(io::NETWORK_DEVICE_ID_BIT | 1).ok_or(Error::InvalidState)?,
            NonZeroU64::MIN,
        ),
    }
}

fn initialized(transport: &Transport, kind: DeviceKind) -> Result<TestBackend, Error> {
    let mut backend = backend(transport, kind)?;
    assert!(backend.progress()?.is_none());
    transport.notification_reply(kind);
    assert!(backend.progress()?.is_none());
    let request = transport.protocol_reply()?;
    // Both devices use the same binding generation, despite distinct MMIO IDs.
    assert_eq!(request.binding, 1);
    assert_eq!(request.command.device_kind(), kind);
    assert!(backend.progress()?.is_none());
    assert!(backend.ready());
    Ok(backend)
}

fn mmio(kind: DeviceKind, offset: u64, operation: MmioOperation) -> Result<MmioRequest, Error> {
    let (id, address) = match kind {
        DeviceKind::Console => return Err(Error::InvalidState),
        DeviceKind::Scsi => (1, io::FRONTEND_MMIO),
        DeviceKind::Network => (io::NETWORK_DEVICE_ID_BIT | 1, io::FRONTEND_NET_MMIO),
    };
    Ok(MmioRequest {
        id: NonZeroU64::new(17).ok_or(Error::InvalidState)?,
        device: NonZeroU64::new(id).ok_or(Error::InvalidState)?,
        address: address + offset,
        width: 4,
        operation,
    })
}

#[test]
fn disk_and_network_share_lane_without_reply_theft_or_duplicate_completion() -> Result<(), Error> {
    for (active, sibling) in [
        (DeviceKind::Scsi, DeviceKind::Network),
        (DeviceKind::Network, DeviceKind::Scsi),
    ] {
        let transport = Transport::default();
        let disk = initialized(&transport, DeviceKind::Scsi)?;
        let network = initialized(&transport, DeviceKind::Network)?;
        let mut devices = IoBackends::new(vec![disk, network])?;
        let reset = mmio(active, 0x70, MmioOperation::Write(0))?;
        let read = mmio(sibling, 8, MmioOperation::Read)?;
        assert!(devices.mmio(2, reset)?.is_none());
        assert!(devices.busy());
        assert!(devices.progress()?.is_none()); // Disable sends once.
        let sent = transport.0.borrow().sent.len();
        let received = transport.0.borrow().receives;
        for _ in 0..8 {
            assert!(devices.mmio(3, read)?.is_none());
        }
        assert_eq!(transport.0.borrow().sent.len(), sent);
        assert_eq!(transport.0.borrow().receives, received);
        transport.notification_reply(active);
        assert!(devices.progress()?.is_none()); // Only active RESET reaches the lane.
        let request = transport.protocol_reply()?;
        assert_eq!(request.command.device_kind(), active);
        let completion = devices.progress()?.ok_or(Error::InvalidState)?;
        assert_eq!(
            (completion.vcpu, completion.id, completion.result),
            (2, reset.id, MmioCompletion::Write)
        );
        assert!(!devices.busy());
        let received = transport.0.borrow().receives;
        assert!(devices.progress()?.is_none());
        assert_eq!(transport.0.borrow().receives, received);
        let completion = devices.mmio(3, read)?.ok_or(Error::InvalidState)?;
        let identity = if sibling == DeviceKind::Scsi { 8 } else { 1 };
        assert_eq!(completion.result, MmioCompletion::Read(identity));
        assert_eq!(completion.vcpu, 3);
    }
    Ok(())
}

#[test]
fn owner_rejects_uninitialized_duplicate_and_unknown_devices() -> Result<(), Error> {
    let transport = Transport::default();
    assert!(IoBackends::<Transport, RemoteNotification<Transport>>::new(vec![]).is_err());
    assert!(IoBackends::new(vec![backend(&transport, DeviceKind::Scsi)?]).is_err());
    let first = initialized(&transport, DeviceKind::Scsi)?;
    let duplicate = initialized(&transport, DeviceKind::Scsi)?;
    assert!(IoBackends::new(vec![first, duplicate]).is_err());
    let network = initialized(&transport, DeviceKind::Network)?;
    let mut devices = IoBackends::new(vec![network])?;
    let absent = mmio(DeviceKind::Scsi, 8, MmioOperation::Read)?;
    assert!(matches!(devices.mmio(0, absent), Err(Error::InvalidState)));
    assert!(
        devices
            .mmio(0, mmio(DeviceKind::Network, 0x70, MmioOperation::Write(0))?)?
            .is_none()
    );
    assert!(matches!(devices.mmio(0, absent), Err(Error::InvalidState)));
    Ok(())
}

#[test]
fn stale_idle_readiness_is_rechecked_without_stealing_live_replies() -> Result<(), Error> {
    let transport = Transport::default();
    let network = initialized(&transport, DeviceKind::Network)?;
    let mut devices = IoBackends::new(vec![network])?;
    let reset = mmio(DeviceKind::Network, 0x70, MmioOperation::Write(0))?;
    assert!(devices.mmio(0, reset)?.is_none());
    assert!(devices.progress()?.is_none());
    transport.notification_reply(DeviceKind::Network);
    let receives = transport.0.borrow().receives;
    // A queued notification reply belongs exclusively to the active backend.
    devices.check_idle_channel()?;
    assert_eq!(transport.0.borrow().receives, receives);
    assert!(devices.progress()?.is_none());
    transport.protocol_reply()?;
    let receives = transport.0.borrow().receives;
    devices.check_idle_channel()?;
    assert_eq!(transport.0.borrow().receives, receives);
    assert!(devices.progress()?.is_some());
    assert!(!devices.busy());
    // The reply was consumed, but a newly registered wait may still observe
    // READABLE before the other CPU finishes publishing the channel level.
    devices.check_idle_channel()?;
    assert_eq!(transport.0.borrow().receives, receives + 2);
    let completion = devices
        .mmio(0, mmio(DeviceKind::Network, 8, MmioOperation::Read)?)?
        .ok_or(Error::InvalidState)?;
    assert_eq!(completion.result, MmioCompletion::Read(1));
    Ok(())
}

#[test]
fn actual_unsolicited_reply_or_closed_idle_channel_is_terminal() -> Result<(), Error> {
    for reply in [vec![], b"unsolicited".to_vec()] {
        let transport = Transport::default();
        let network = initialized(&transport, DeviceKind::Network)?;
        let devices = IoBackends::new(vec![network])?;
        transport.0.borrow_mut().replies.push_back(reply);
        assert!(matches!(
            devices.check_idle_channel(),
            Err(Error::InvalidState)
        ));
    }
    let transport = Transport::default();
    let network = initialized(&transport, DeviceKind::Network)?;
    let devices = IoBackends::new(vec![network])?;
    transport.0.borrow_mut().closed = true;
    assert!(matches!(
        devices.check_idle_channel(),
        Err(Error::Native(hyper_os::Error::Status(
            hyper_os::Status::PEER_CLOSED
        )))
    ));
    Ok(())
}

#[test]
fn pci_ecam_enumeration_and_indirect_access_share_live_state() -> Result<(), Error> {
    use hyper_os::guest_io::FrontendOperation;
    use hyper_vm_support::virtio_pci::CONFIG_DATA;
    let transport = Transport::default();
    let disk = initialized(&transport, DeviceKind::Scsi)?;
    let network = initialized(&transport, DeviceKind::Network)?;
    let mut devices = IoBackends::new(vec![disk, network])?;
    devices.use_pci()?;
    let accesses = RefCell::new(Vec::new());
    let routes = RefCell::new(Vec::new());
    let mut call = |slot: u64, offset, width, operation| {
        let request = MmioRequest {
            address: io::PCI_ECAM + (slot << 15) + offset,
            width,
            ..mmio(DeviceKind::Scsi, 0, operation)?
        };
        devices
            .pci_mmio(
                0,
                request,
                |original, base, command| {
                    routes.borrow_mut().push((original, base, command));
                    Ok(())
                },
                |original, operation| {
                    accesses.borrow_mut().push((original, operation));
                    Ok(3)
                },
            )?
            .map(|completion| completion.result)
            .ok_or(Error::InvalidState)
    };
    assert_eq!(
        call(1, 0, 4, MmioOperation::Read)?,
        MmioCompletion::Read(0x1048_1af4)
    );
    assert_eq!(
        call(2, 0, 4, MmioOperation::Read)?,
        MmioCompletion::Read(0x1041_1af4)
    );
    assert_eq!(
        call(3, 0, 4, MmioOperation::Read)?,
        MmioCompletion::Read(0xffff_ffff)
    );
    assert_eq!(
        call(1, 0x1000, 4, MmioOperation::Read)?,
        MmioCompletion::Read(0xffff_ffff)
    );
    assert_eq!(
        call(1, CONFIG_DATA, 4, MmioOperation::Read)?,
        MmioCompletion::Read(0xffff_ffff)
    );
    call(1, 0x8c, 4, MmioOperation::Write(4))?;
    call(1, 0x90, 4, MmioOperation::Write(4))?;
    assert_eq!(
        call(1, CONFIG_DATA, 4, MmioOperation::Read)?,
        MmioCompletion::Read(0)
    );
    // Select feature word 1 through PCI_CFG; it changes the same core as BAR.
    call(1, 0x8c, 4, MmioOperation::Write(0))?;
    call(1, CONFIG_DATA, 4, MmioOperation::Write(1))?;
    call(1, 0x8c, 4, MmioOperation::Write(4))?;
    assert_eq!(
        call(1, CONFIG_DATA, 4, MmioOperation::Read)?,
        MmioCompletion::Read(1)
    );
    call(1, 0x8c, 4, MmioOperation::Write(0x60))?;
    call(1, 0x90, 4, MmioOperation::Write(1))?;
    assert_eq!(
        call(1, CONFIG_DATA, 4, MmioOperation::Read)?,
        MmioCompletion::Read(3)
    );
    call(2, 0x8c, 4, MmioOperation::Write(0x50))?;
    call(2, 0x90, 4, MmioOperation::Write(2))?;
    assert_eq!(
        call(2, CONFIG_DATA, 4, MmioOperation::Write(0xffff_0001))?,
        MmioCompletion::Write
    );
    assert_eq!(
        call(1, 6, 2, MmioOperation::Read)?,
        MmioCompletion::Read(0x18)
    );
    assert_eq!(
        *accesses.borrow(),
        vec![
            (io::FRONTEND_MMIO, FrontendOperation::ReadAndClearInterrupt),
            (io::FRONTEND_NET_MMIO, FrontendOperation::NotifyQueue(1)),
            (io::FRONTEND_MMIO, FrontendOperation::InterruptStatus),
        ]
    );
    call(1, 0x10, 4, MmioOperation::Write(io::PCI_MEMORY + 0x4000))?;
    call(1, 4, 2, MmioOperation::Write(6))?;
    assert_eq!(
        routes.borrow().last(),
        Some(&(io::FRONTEND_MMIO, io::PCI_MEMORY + 0x4000, 6))
    );
    Ok(())
}

#[test]
fn pci_indirect_reset_retires_only_after_backend_reply() -> Result<(), Error> {
    use hyper_vm_support::virtio_pci::CONFIG_DATA;
    let transport = Transport::default();
    let mut disk = initialized(&transport, DeviceKind::Scsi)?;
    disk.use_pci()?;
    disk.write_pci_configuration(0x8c, 4, 20)?;
    disk.write_pci_configuration(0x90, 4, 1)?;
    let mut devices = IoBackends::new(vec![disk])?;
    let request = MmioRequest {
        address: io::PCI_ECAM + (1 << 15) + CONFIG_DATA,
        ..mmio(DeviceKind::Scsi, 0, MmioOperation::Write(0xffff_ff00))?
    };
    // A DWORD PCI write requests a BYTE device reset; high bytes are data
    // latch contents, never status bits. Neither the write nor a sibling read
    // may retire before the backend acknowledges the reset.
    let no_hot_access = |_, _| Err(hyper_os::Error::InvalidResponse);
    assert!(
        devices
            .pci_mmio(2, request, |_, _, _| Ok(()), no_hot_access)?
            .is_none()
    );
    assert!(devices.busy());
    assert!(devices.progress()?.is_none());
    transport.notification_reply(DeviceKind::Scsi);
    assert!(devices.progress()?.is_none());
    assert_eq!(
        transport.protocol_reply()?.command,
        Command::Device(virtio::BackendOperation::Reset)
    );
    let completion = devices.progress()?.ok_or(Error::InvalidState)?;
    assert_eq!(
        (completion.vcpu, completion.id, completion.result),
        (2, request.id, MmioCompletion::Write)
    );
    assert!(!devices.busy());
    assert!(devices.progress()?.is_none());
    let read = MmioRequest {
        operation: MmioOperation::Read,
        ..request
    };
    let completion = devices
        .pci_mmio(3, read, |_, _, _| Ok(()), no_hot_access)?
        .ok_or(Error::InvalidState)?;
    assert_eq!(completion.result, MmioCompletion::Read(0xffff_ff00));
    Ok(())
}
