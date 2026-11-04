//! UPD-T01..T22 (matrix/tests.json, audits/updater.json) and the unit tests. Host tests run in temp dirs only; the
//! parts that need a VM guest (real /Applications, Gatekeeper, Keychain, TCC, login items, real ENOSPC or SIGKILL)
//! are `#[ignore = "VM: ..."]` and scripted in tests/vm/upd_vm.sh.

mod fixture;
mod unit;
mod upd;
