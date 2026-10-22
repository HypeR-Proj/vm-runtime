# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

$(eval $(call native-rust-app,svc/vm-runtime,hyper-vm-runtime,vm-runtime,svc/vm-runtime,development))
