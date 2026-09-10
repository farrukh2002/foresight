"""Sanity checks for the harness itself, no fr-* modules involved."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "harness"))
from fixtures import RenderTestCase


class HarnessSanity(RenderTestCase):
    def test_plain_bash_echoes_keystrokes(self):
        proc = self.spawn(sources=[])
        proc.send("echo hi")
        self.assertIn("echo hi", proc.screen.row_text(proc.screen.cursor()[0]))

    def test_enter_runs_command_and_prints_output(self):
        proc = self.spawn(sources=[])
        proc.send("echo hi", settle=0.05)
        proc.send_key("Enter", settle=0.3)
        text = proc.screen.full_text()
        self.assertIn("hi", text)

    def test_resize_updates_screen_dimensions(self):
        proc = self.spawn(sources=[])
        self.assertEqual(proc.screen.cols, 80)
        proc.resize(30, 100)
        self.assertEqual(proc.screen.cols, 100)
        self.assertEqual(proc.screen.rows, 30)

    def test_sgr_dim_is_tracked_by_vt_screen(self):
        proc = self.spawn(sources=[])
        proc.send('printf "\\033[2mghost\\033[0m"', settle=0.05)
        proc.send_key("Enter", settle=0.3)
        text = proc.screen.full_text()
        self.assertIn("ghost", text)
        row = next(
            r for r in range(proc.screen.rows)
            if "ghost" in proc.screen.row_text(r) and "printf" not in proc.screen.row_text(r)
        )
        col = proc.screen.row_text(row).index("ghost")
        self.assertTrue(proc.screen.ghost_cells(row)[col])
