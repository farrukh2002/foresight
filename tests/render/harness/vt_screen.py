"""Minimal VT100/xterm screen model.

Not a full terminal emulator: just enough state (grid, cursor, SGR
attributes, scrolling) to let render tests assert on text, cursor
position, and which cells are drawn with "ghost" attributes. Stdlib only
(no pyte dependency) so the harness runs anywhere the repo's CI does.
"""
from __future__ import annotations

import codecs
import unicodedata
from dataclasses import dataclass, field


@dataclass(frozen=True)
class Attrs:
    bold: bool = False
    dim: bool = False
    fg: int | None = None
    bg: int | None = None


BLANK = Attrs()


@dataclass
class Cell:
    ch: str = " "
    attrs: Attrs = field(default_factory=lambda: BLANK)


class VTScreen:
    def __init__(self, rows: int = 24, cols: int = 80):
        self.rows = rows
        self.cols = cols
        self.grid: list[list[Cell]] = [[Cell() for _ in range(cols)] for _ in range(rows)]
        self.cur_row = 0
        self.cur_col = 0
        self.pending_wrap = False
        self.cursor_visible = True
        self._attrs = BLANK
        self._decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self._state = "NORMAL"
        self._csi_buf = ""

    def feed(self, data: bytes) -> None:
        text = self._decoder.decode(data)
        for ch in text:
            self._feed_char(ch)

    def resize(self, rows: int, cols: int) -> None:
        new_grid = [[Cell() for _ in range(cols)] for _ in range(rows)]
        for r in range(min(rows, self.rows)):
            for c in range(min(cols, self.cols)):
                new_grid[r][c] = self.grid[r][c]
        self.grid = new_grid
        self.rows, self.cols = rows, cols
        self.cur_row = min(self.cur_row, rows - 1)
        self.cur_col = min(self.cur_col, cols - 1)
        self.pending_wrap = False

    def row_text(self, row: int) -> str:
        return "".join(c.ch for c in self.grid[row]).rstrip(" ")

    def full_text(self) -> str:
        return "\n".join(self.row_text(r) for r in range(self.rows)).rstrip("\n")

    def ghost_cells(self, row: int) -> list[bool]:
        """Cells in `row` whose attrs mark them as ghost (dim) text."""
        return [c.attrs.dim for c in self.grid[row]]

    def cursor(self) -> tuple[int, int]:
        return (self.cur_row, self.cur_col)

    def _feed_char(self, ch: str) -> None:
        if self._state == "NORMAL":
            self._feed_normal(ch)
        elif self._state == "ESC":
            self._feed_esc(ch)
        elif self._state == "CSI":
            self._feed_csi(ch)
        elif self._state == "OSC":
            self._feed_osc(ch)

    def _feed_normal(self, ch: str) -> None:
        if ch == "\x1b":
            self._state = "ESC"
            self._csi_buf = ""
        elif ch == "\r":
            self.cur_col = 0
            self.pending_wrap = False
        elif ch == "\n":
            self._newline()
            self.pending_wrap = False
        elif ch == "\b":
            self.cur_col = max(0, self.cur_col - 1)
            self.pending_wrap = False
        elif ch == "\t":
            self.cur_col = min(self.cols - 1, (self.cur_col // 8 + 1) * 8)
            self.pending_wrap = False
        elif ch in ("\x07",):
            pass
        else:
            self._put_char(ch)

    def _feed_esc(self, ch: str) -> None:
        if ch == "[":
            self._state = "CSI"
            self._csi_buf = ""
        elif ch == "]":
            self._state = "OSC"
        elif ch == "D":
            self._newline()
            self._state = "NORMAL"
        elif ch == "M":
            if self.cur_row == 0:
                self._scroll_down()
            else:
                self.cur_row -= 1
            self._state = "NORMAL"
        else:
            self._state = "NORMAL"

    def _feed_osc(self, ch: str) -> None:
        if ch == "\x07":
            self._state = "NORMAL"
        elif ch == "\\":
            self._state = "NORMAL"

    def _feed_csi(self, ch: str) -> None:
        if "@" <= ch <= "~":
            self._dispatch_csi(self._csi_buf, ch)
            self._state = "NORMAL"
        else:
            self._csi_buf += ch

    def _params(self, buf: str) -> list[int]:
        buf = buf.lstrip("?")
        if not buf:
            return []
        out = []
        for p in buf.split(";"):
            try:
                out.append(int(p))
            except ValueError:
                out.append(0)
        return out

    def _dispatch_csi(self, buf: str, final: str) -> None:
        private = buf.startswith("?")
        params = self._params(buf)

        def p(i: int, default: int = 1) -> int:
            return params[i] if i < len(params) and params[i] != 0 else default

        if private and final in ("h", "l"):
            if params[:1] == [25]:
                self.cursor_visible = final == "h"
            return

        if final in ("A", "B", "C", "D", "E", "F", "G", "H", "f"):
            self.pending_wrap = False

        if final == "A":
            self.cur_row = max(0, self.cur_row - p(0))
        elif final == "B":
            self.cur_row = min(self.rows - 1, self.cur_row + p(0))
        elif final == "C":
            self.cur_col = min(self.cols - 1, self.cur_col + p(0))
        elif final == "D":
            self.cur_col = max(0, self.cur_col - p(0))
        elif final == "E":
            self.cur_row = min(self.rows - 1, self.cur_row + p(0))
            self.cur_col = 0
        elif final == "F":
            self.cur_row = max(0, self.cur_row - p(0))
            self.cur_col = 0
        elif final == "G":
            self.cur_col = min(self.cols - 1, p(0) - 1)
        elif final in ("H", "f"):
            row = p(0) if params[:1] else 1
            col = p(1) if len(params) > 1 else 1
            self.cur_row = min(self.rows - 1, max(0, row - 1))
            self.cur_col = min(self.cols - 1, max(0, col - 1))
        elif final == "J":
            self._erase_display(params[0] if params else 0)
        elif final == "K":
            self._erase_line(params[0] if params else 0)
        elif final == "m":
            self._sgr(params)

    def _sgr(self, params: list[int]) -> None:
        if not params:
            params = [0]
        a = self._attrs
        i = 0
        while i < len(params):
            code = params[i]
            if code == 0:
                a = BLANK
            elif code == 1:
                a = Attrs(True, a.dim, a.fg, a.bg)
            elif code == 2:
                a = Attrs(a.bold, True, a.fg, a.bg)
            elif code == 22:
                a = Attrs(False, False, a.fg, a.bg)
            elif code == 39:
                a = Attrs(a.bold, a.dim, None, a.bg)
            elif code == 49:
                a = Attrs(a.bold, a.dim, a.fg, None)
            elif 30 <= code <= 37:
                a = Attrs(a.bold, a.dim, code - 30, a.bg)
            elif 90 <= code <= 97:
                a = Attrs(a.bold, a.dim, code - 90 + 8, a.bg)
            elif 40 <= code <= 47:
                a = Attrs(a.bold, a.dim, a.fg, code - 40)
            elif 100 <= code <= 107:
                a = Attrs(a.bold, a.dim, a.fg, code - 100 + 8)
            elif code == 38 and i + 2 < len(params) and params[i + 1] == 5:
                a = Attrs(a.bold, a.dim, params[i + 2], a.bg)
                i += 2
            elif code == 48 and i + 2 < len(params) and params[i + 1] == 5:
                a = Attrs(a.bold, a.dim, a.fg, params[i + 2])
                i += 2
            i += 1
        self._attrs = a

    def _erase_line(self, mode: int) -> None:
        row = self.grid[self.cur_row]
        if mode == 0:
            for c in range(self.cur_col, self.cols):
                row[c] = Cell()
        elif mode == 1:
            for c in range(0, self.cur_col + 1):
                row[c] = Cell()
        elif mode == 2:
            self.grid[self.cur_row] = [Cell() for _ in range(self.cols)]

    def _erase_display(self, mode: int) -> None:
        if mode == 2:
            self.grid = [[Cell() for _ in range(self.cols)] for _ in range(self.rows)]
            return
        if mode == 0:
            self._erase_line(0)
            for r in range(self.cur_row + 1, self.rows):
                self.grid[r] = [Cell() for _ in range(self.cols)]
        elif mode == 1:
            self._erase_line(1)
            for r in range(0, self.cur_row):
                self.grid[r] = [Cell() for _ in range(self.cols)]

    def _put_char(self, ch: str) -> None:
        if self.pending_wrap:
            self.cur_col = 0
            self._newline()
            self.pending_wrap = False
        w = self._char_width(ch)
        self.grid[self.cur_row][self.cur_col] = Cell(ch, self._attrs)
        if w == 2 and self.cur_col + 1 < self.cols:
            self.grid[self.cur_row][self.cur_col + 1] = Cell("", self._attrs)
        if w == 2:
            if self.cur_col == self.cols - 1:
                pass
            elif self.cur_col + 1 == self.cols - 1:
                self.cur_col = self.cols - 1
                self.pending_wrap = True
            else:
                self.cur_col += 2
        else:
            if self.cur_col == self.cols - 1:
                self.pending_wrap = True
            else:
                self.cur_col += 1

    @staticmethod
    def _char_width(ch: str) -> int:
        """East-Asian-Width for one printable char. Returns 0 (combining),
        1, or 2 (CJK / wide). Matches fr-render's fr_char_width ranges
        closely enough for the assertions in this test suite — adding
        the real wcwidth() dependency just for one function is overkill
        and would force a non-stdlib install on the CI host."""
        if not ch or ch == " ":
            return 1
        cp = ord(ch)
        if cp < 32 or ch == "\x7f":
            return 0
        if 0x0300 <= cp <= 0x036F:
            return 0
        if cp in (0x200B, 0x200C, 0x200D, 0x200E, 0x200F, 0xFEFF):
            return 0
        if 0xFE00 <= cp <= 0xFE0F:
            return 0
        if 0x1100 <= cp <= 0x115F:
            return 2
        if 0x2E80 <= cp <= 0x303E:
            return 2
        if 0x3041 <= cp <= 0x33FF:
            return 2
        if 0x3400 <= cp <= 0x4DBF:
            return 2
        if 0x4E00 <= cp <= 0x9FFF:
            return 2
        if 0xA000 <= cp <= 0xA4CF:
            return 2
        if 0xAC00 <= cp <= 0xD7A3:
            return 2
        if 0xF900 <= cp <= 0xFAFF:
            return 2
        if 0xFF00 <= cp <= 0xFF60:
            return 2
        if 0xFFE0 <= cp <= 0xFFE6:
            return 2
        if 0x1F300 <= cp <= 0x1FAFF:
            return 2
        if 0x20000 <= cp <= 0x3FFFD:
            return 2
        try:
            if unicodedata.east_asian_width(ch) in ("W", "F"):
                return 2
        except Exception:
            pass
        return 1

    def _newline(self) -> None:
        if self.cur_row == self.rows - 1:
            self._scroll_up()
        else:
            self.cur_row += 1

    def _scroll_up(self) -> None:
        self.grid.pop(0)
        self.grid.append([Cell() for _ in range(self.cols)])

    def _scroll_down(self) -> None:
        self.grid.pop()
        self.grid.insert(0, [Cell() for _ in range(self.cols)])
