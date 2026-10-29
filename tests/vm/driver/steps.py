import time
from contextlib import contextmanager

from .machine import MachineError


class StepFailed(Exception):
    def __init__(self, step, failures):
        self.step = step
        self.failures = failures
        super().__init__(f"step '{step}' failed, {len(failures)} check(s):\n- " + "\n- ".join(failures))


class Test:
    """A test's named steps: soft checks in a step are collected and fail it together at its end; require and machine errors stop it at once."""

    def __init__(self, timeline):
        self.timeline = timeline
        self.failures = []
        self.passed = []
        self.verdict = None

    @contextmanager
    def step(self, name):
        self.timeline.step = name
        self.failures = []
        self.verdict = None
        began = time.monotonic()
        self.timeline.log("step begins")
        try:
            yield
        except (MachineError, AssertionError) as e:
            self.failures.append(str(e).removeprefix(f"step '{name}': "))
            raise StepFailed(name, self.failures) from e
        except Exception as e:
            self.failures.append(f"{type(e).__name__}: {e}")
            raise StepFailed(name, self.failures) from e
        finally:
            took = time.monotonic() - began
            if not self.failures:
                self.passed.append((name, took))
                self.timeline.log(f"PASS ({took:.1f} s)")
            self.timeline.step = None
        if self.failures:
            raise StepFailed(name, self.failures)

    def check(self, ok, msg):
        if not ok:
            self.timeline.log(f"check failed: {msg}")
            self.failures.append(msg)
        return ok

    def require(self, ok, msg):
        if not ok:
            raise AssertionError(msg)

    def attribute(self, verdict):
        """Logs docs/CI.md's ATTRIBUTED: or NOT ATTRIBUTED: line for this step's failure, which the driver prints after FAIL."""
        self.verdict = verdict
        self.timeline.log(verdict)

    def log(self, msg):
        self.timeline.log(msg)
