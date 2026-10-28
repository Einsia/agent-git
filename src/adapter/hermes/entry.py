"""Run AGIT_SCRIPT where a Hermes launcher runs its entry module, after its bootstrap has made the
CLI's dependencies importable. The requested module does not exist, so a bootstrap that stops
running its entry through runpy fails the launch instead of skipping the script."""
import runpy

_run_module = runpy.run_module


def _entry(name, *args, **kwargs):
    if name != "agit_hermes_entry":
        return _run_module(name, *args, **kwargs)
    exec(compile(AGIT_SCRIPT, "agit", "exec"), {"__name__": "__main__"})


runpy.run_module = _entry
