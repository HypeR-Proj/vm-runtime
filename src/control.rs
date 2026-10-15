// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded placement control over the runtime's existing vCPU capabilities.

use hyper_service::vm::{VcpuControlReply, VcpuControlRequest};

pub fn handle(
    request: VcpuControlRequest,
    vcpu_count: usize,
    mut set_affinity: impl FnMut(u32, &[u64]) -> hyper_os::Result<()>,
    mut inspect: impl FnMut(u32) -> hyper_os::Result<(Option<u32>, Option<u32>)>,
) -> VcpuControlReply {
    let mut reply = VcpuControlReply {
        sequence: request.sequence,
        vcpu: request.vcpu,
        host_cpu: None,
        pending_host_cpu: None,
        status: hyper_os::Status::OK,
    };
    let result = (|| {
        if request.vcpu as usize >= vcpu_count {
            return Err(hyper_os::Error::Status(hyper_os::Status::INVALID_ARGUMENT));
        }
        if let Some(words) = request.affinity {
            if words.iter().all(|word| *word == 0) {
                return Err(hyper_os::Error::Status(hyper_os::Status::INVALID_ARGUMENT));
            }
            set_affinity(request.vcpu, &words)?;
            // Inspection must not turn an accepted side effect into a rejection.
            if let Ok((current, pending)) = inspect(request.vcpu) {
                reply.host_cpu = current;
                reply.pending_host_cpu = pending;
            }
        } else {
            (reply.host_cpu, reply.pending_host_cpu) = inspect(request.vcpu)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        reply.status = match error {
            hyper_os::Error::Status(status) => status,
            _ => hyper_os::Status::INTERNAL,
        };
    }
    reply
}

#[cfg(test)]
#[path = "../tests/control.rs"]
mod tests;
