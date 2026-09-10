#!/usr/bin/env python3
"""Test runner for the foresight-render pty harness. Stdlib unittest only."""
import os
import sys
import unittest

ROOT = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(ROOT, "harness"))
sys.path.insert(0, ROOT)


def main() -> int:
    loader = unittest.TestLoader()
    suite = loader.discover(os.path.join(ROOT, "cases"), pattern="test_*.py", top_level_dir=ROOT)
    pty_ok = unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful()
    return 0 if pty_ok else 1


if __name__ == "__main__":
    sys.exit(main())
