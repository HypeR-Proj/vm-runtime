// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::{RETAIN_BYTES, retain};
use std::collections::VecDeque;

#[test]
fn disconnected_or_slow_client_keeps_bounded_latest_output() {
    let mut queue = VecDeque::new();
    retain(&mut queue, &vec![b'a'; RETAIN_BYTES]);
    retain(&mut queue, b"new");
    assert_eq!(queue.len(), RETAIN_BYTES);
    assert_eq!(
        queue.iter().rev().take(3).copied().collect::<Vec<_>>(),
        b"wen"
    );
}
