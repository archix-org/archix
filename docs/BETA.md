# Beta checks

Run these before the beta starts, and again whenever the condition next to one comes up during it. A check passes only on the exact commit being released, with its logs kept.

| Check | When | How |
| --- | --- | --- |
| Container suite | Every release | `tests/arch/run.sh` |
| VM suites | Every release | `tests/vm/run.sh` for `boot`, `rollback` (also with `FS=btrfs`), `upgrade`, `nvidia` and `cachyos` |
| DKMS behaviour | A lock brings a `dkms` version not listed in `tests/vm/nvidia/dkms-tested` | `tests/vm/dkms-check.sh <archix.lock>`; if it fails, run `TO_DATE=<a snapshot with that dkms> tests/vm/run.sh nvidia`, then add the version to the list |
| CachyOS | Every release | `tests/vm/run.sh cachyos`: the rollback suite on a CachyOS image with its real repos, v3 packages included |

The DKMS check exists because archix's handling of the module copies a rollback restores depends on what `dkms install` and `dkms remove` do with `original_module/`. A new `dkms` release can change that without notice, and the NVIDIA suite is what would notice.
