# A reset (QMP system_reset, not a power cut: that is tests/vm/powercut's) a second into a switch's staging leaves the old deployment booting and current, and the next switch clears the staging tree.
import time

from boot.common import HOST, bind, boot, current, deployments, edit_config, listing, number, ok


def run(m, t):
    bind(m, t)
    with t.step("5: a reset mid-switch"):
        boot()
        before, kept = current(), deployments()
        m.succeed(f"{HOST} install jq --noconfirm > /dev/console 2>&1 &")
        m.wait_until_succeeds("ls -d /archix/deploy/.staging-*", timeout=600, interval=0.2, what="the switch never started staging")
        time.sleep(1)
        m.crash()

    with t.step("6: the reset switch left the old deployment; the next switch clears its staging tree"):
        _, booted, _ = boot()
        t.check(booted == before and current() == before, f"after a reset mid-switch, deployment {before} should still boot and be current")
        t.check(deployments() == kept, f"the reset switch committed a deployment: {listing()}; deployments before {kept}, now {deployments()}")
        m.succeed(f"{HOST} install jq --noconfirm", what="the switch after the reset")
        t.check(number(current()) > number(before), "installing jq should make a newer deployment")
        t.check(not ok("ls -d /archix/deploy/.staging-*"), f"the next switch should clear the reset switch's staging tree: {listing()}")
        # boot2 btrfs once failed here with ENOENT, building from the pending deployment the switch after the reset made.
        edit_config("'s|\"console=ttyS0\" }|\"console=ttyS0\", \"archix.second\" }|'")
        m.succeed(f"{HOST} switch --noconfirm", what="a switch from the pending deployment the switch after the reset made")
