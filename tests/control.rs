// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::*;

fn request(affinity: Option<[u64; hyper_service::vm::VCPU_AFFINITY_WORDS]>) -> VcpuControlRequest {
    VcpuControlRequest {
        sequence: 7,
        vcpu: 1,
        affinity,
    }
}

#[test]
fn invalid_vcpu_never_reaches_kernel_operations() {
    let reply = handle(
        request(Some([4, 0, 0, 0])),
        1,
        |_, _| panic!("invalid migration"),
        |_| panic!("invalid inspection"),
    );
    assert_eq!(reply.status, hyper_os::Status::INVALID_ARGUMENT);
}

#[test]
fn inspection_is_read_only_and_pending_is_not_completion() {
    let reply = handle(
        request(None),
        2,
        |_, _| panic!("read-only request"),
        |_| Ok((Some(0), Some(2))),
    );
    assert_eq!(reply.host_cpu, Some(0));
    assert_eq!(reply.pending_host_cpu, Some(2));
    let reply = handle(
        request(Some([4, 0, 0, 0])),
        2,
        |vcpu, target| {
            assert_eq!(vcpu, 1);
            assert_eq!(target, [4, 0, 0, 0]);
            Ok(())
        },
        |_| Ok((Some(0), Some(2))),
    );
    assert_eq!(reply.status, hyper_os::Status::OK);
}

#[test]
fn kernel_failure_and_post_migration_inspection_failure_are_distinct() {
    let denied = handle(
        request(Some([4, 0, 0, 0])),
        2,
        |_, _| Err(hyper_os::Error::Status(hyper_os::Status::ACCESS_DENIED)),
        |_| panic!("migration rejected"),
    );
    assert_eq!(denied.status, hyper_os::Status::ACCESS_DENIED);
    let completed = handle(
        request(Some([4, 0, 0, 0])),
        2,
        |_, _| Ok(()),
        |_| Err(hyper_os::Error::InvalidResponse),
    );
    assert_eq!(completed.status, hyper_os::Status::OK);
    assert_eq!(completed.host_cpu, None);
}

#[test]
fn empty_affinity_never_changes_policy() {
    let reply = handle(
        request(Some([0; hyper_service::vm::VCPU_AFFINITY_WORDS])),
        2,
        |_, _| panic!("empty policy"),
        |_| panic!("empty policy"),
    );
    assert_eq!(reply.status, hyper_os::Status::INVALID_ARGUMENT);
}
