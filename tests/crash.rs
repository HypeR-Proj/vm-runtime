// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Explicit non-default acceptance fixture: exit without running destructors
//! while the guest is producing output and a console client is attached.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static COLLECTED: AtomicUsize = AtomicUsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

fn maybe_crash() {
    if ARMED.load(Ordering::Relaxed) && COLLECTED.load(Ordering::Relaxed) >= 512 {
        std::process::exit(93);
    }
}

/// The probe sends RS after vmm reports a connected console. Without this
/// handshake buffered boot output can kill the runtime before the manager has
/// transferred the client endpoint, testing admission instead of disconnect.
pub(super) fn after_input(bytes: &[u8]) {
    if bytes.contains(&0x1e) {
        ARMED.store(true, Ordering::Relaxed);
        maybe_crash();
    }
}

pub(super) fn after_output(count: usize) {
    COLLECTED.fetch_add(count, Ordering::Relaxed);
    maybe_crash();
}
