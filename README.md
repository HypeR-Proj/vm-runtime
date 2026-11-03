<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# vm-runtime

Per-VM Native runtime process.

Loads the selected guest image, prepares virtual devices and owns execution/control for
one VM instance. Uses `lib-vm-image`, `lib-vm-support` and `lib-vm-policy`; vm-manager
supervises its process lifecycle.

See the [implementation entry](src/main.rs) and [component declaration](component.json)
for the public interfaces, build dependencies and delivered artifacts.

## Boundaries

The runtime requires the instance-specific startup capabilities and control channel. It
is not launched as a standalone host hypervisor; backing memory and device ownership
must survive until guest execution and hardware use retire.

## Build and test

Use this checkout inside the [repo-managed workspace](https://github.com/HypeR-Proj/HypeR).
From this repository directory:

```sh
make
make test
```

An absolute `OUT=/path/to/out` selects a shared output directory. The module
entry delegates to build/system, which prepares the installed SDK and declared
dependencies. Native binaries require HypeR; host module checks cover the tests
selected by component.json. A standalone clone does not contain the workspace’s
other source repositories or Native SDK.
