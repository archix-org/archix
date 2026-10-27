import argparse
import code
import importlib.util
import os
import sys
import time

from . import Machine, MachineError, StepFailed, Test, Timeline


def load(path):
    spec = importlib.util.spec_from_file_location("vmtest", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(prog="python -m driver", description="Runs a VM test script's run(machine, t) against a QEMU+OVMF VM booting /work/disk.img.")
    parser.add_argument("test")
    parser.add_argument("--interactive", action="store_true", help="a Python REPL with machine and t bound; test_script() runs the test")
    parser.add_argument("--work", default=os.environ.get("WORK", "/work"))
    args = parser.parse_args()
    module = load(args.test)
    timeline = Timeline(os.path.join(args.work, "timeline.log"))
    machine = Machine(timeline, work=args.work, **getattr(module, "MACHINE", {}))
    t = Test(timeline)
    if args.interactive:
        import readline
        import rlcompleter

        readline.parse_and_bind("tab: complete")
        names = {"machine": machine, "t": t, "test_script": lambda: module.run(machine, t)}
        try:
            code.interact(banner="machine (not started: machine.start(), or any command), t; test_script() runs the whole test", local=names, exitmsg="stopping the VM")
        finally:
            machine.stop()
        return 0
    began = time.monotonic()
    try:
        module.run(machine, t)
    except StepFailed as e:
        print(f"FAIL: {e}", flush=True)
        return 1
    except MachineError as e:
        print(f"FAIL: {e}", flush=True)
        return 1
    finally:
        machine.stop()
        took = time.monotonic() - began
        print(f"steps passed: {len(t.passed)}, {took:.0f} s in all", flush=True)
        for name, step_took in t.passed:
            print(f"  {step_took:6.1f} s  {name}", flush=True)
    print("OK", flush=True)
    return 0


sys.exit(main())
