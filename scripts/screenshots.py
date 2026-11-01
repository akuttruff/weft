#!/usr/bin/env python3
"""The README's screenshots, from a real run.

Clones a demo repository, runs weft on the clone in a tmux window with a
task, answers its questions the way a person would (enter on the plan, `a`
on an edit), and renders two moments as terminal windows through headless
Chrome:

  approval.png  the plan, waiting for a word
  weft.png      the whole task: plan, work, checks, review

  scripts/screenshots.py [--repo ~/Code/barometer] [--out docs] [--work DIR]

It needs tmux, git, the demo repository's node_modules (copied from it),
Google Chrome, and LM Studio with weft's models loaded. A run takes as long
as the models do; allow half an hour. The clone, its config and weft's
session files for it are removed at the end unless --keep is given.
"""

import argparse
import html
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# Given with /plan, so the screenshots always show a plan to approve; left to
# triage, a task this size may go straight to work. One task: a second makes
# the session image too long to read on the page.
TASKS = [
    "/plan sort the monitor tiles so anything that's alerting or warning comes first in its group",
]
CHECKS = "npx tsc --noEmit && npx oxlint && npx vitest run"
SESSION = "weft-shots"
COLS, ROWS = 100, 52
CHROME = os.environ.get("CHROME", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
# Each task may take this long, models included.
TASK_TIMEOUT_S = 45 * 60

ANSI = re.compile(r"\x1b\[([0-9;]*)m")
PALETTE = {
    0: "#1c1d21", 1: "#f0605a", 2: "#5fd27a", 3: "#e8b04b", 4: "#79a8ff", 5: "#c792ea", 6: "#7cc5ff", 7: "#e6e6e6",
    8: "#5c6370", 9: "#ff7b72", 10: "#7ee787", 11: "#f2cc60", 12: "#a5c8ff", 13: "#d2a8ff", 14: "#a5d6ff", 15: "#ffffff",
}
FG, BG = "#e6e6e6", "#1e1f24"


def sh(*args: str, **kw) -> str:
    return subprocess.run(args, check=True, capture_output=True, text=True, **kw).stdout


def tmux(*args: str) -> str:
    return sh("tmux", *args)


def plain(text: str) -> str:
    return ANSI.sub("", text)


def visible() -> str:
    return tmux("capture-pane", "-p", "-e", "-t", SESSION)


def history() -> str:
    return tmux("capture-pane", "-p", "-e", "-S", "-", "-t", SESSION)


def type_line(text: str) -> None:
    tmux("send-keys", "-t", SESSION, "-l", text)
    tmux("send-keys", "-t", SESSION, "Enter")


def key(k: str) -> None:
    tmux("send-keys", "-t", SESSION, k)


def bottom(text: str) -> list[str]:
    """The live area: the last dozen non-blank lines, without colour."""
    lines = [l for l in plain(text).splitlines() if l.strip()]
    return lines[-12:]


def run_task(task: str, on_plan) -> None:
    """Give weft one task and see it through: a plan gets enter (after
    `on_plan` has seen it), an approval gets `a`, until weft is idle again."""
    type_line(task)
    began, busy, answered = time.time(), False, set()
    while time.time() - began < TASK_TIMEOUT_S:
        time.sleep(2)
        screen = visible()
        live = bottom(screen)
        status = "\n".join(live[-8:])
        if "Follow this plan?" in status:
            if "plan" not in answered:
                on_plan(screen)
                answered.add("plan")
                print("  plan: go", flush=True)
            time.sleep(1)
            key("Enter")
            continue
        answered.discard("plan")
        if "Allow weft to" in status:
            what = next(l for l in live if "Allow weft to" in l).strip()
            print(f"  {what} → a", flush=True)
            key("a")
            continue
        footer = live[-1] if live else ""
        idle = " · idle · " in footer
        if not idle:
            busy = True
        elif busy:
            print(f"  done in {int(time.time() - began)}s", flush=True)
            time.sleep(2)
            return
    raise SystemExit(f"weft did not finish {task!r} within {TASK_TIMEOUT_S // 60} minutes")


# ---- Rendering ---------------------------------------------------------------


def color(params: list[int], i: int) -> tuple[str | None, int]:
    """A 38/48 colour starting at params[i] (just after the 38 or 48)."""
    if i < len(params) and params[i] == 5 and i + 1 < len(params):
        n = params[i + 1]
        if n < 16:
            return PALETTE[n], i + 2
        if n < 232:
            n -= 16
            steps = [0, 95, 135, 175, 215, 255]
            return "#%02x%02x%02x" % (steps[n // 36], steps[n // 6 % 6], steps[n % 6]), i + 2
        g = 8 + (n - 232) * 10
        return "#%02x%02x%02x" % (g, g, g), i + 2
    if i < len(params) and params[i] == 2 and i + 3 < len(params):
        return "#%02x%02x%02x" % tuple(params[i + 1 : i + 4]), i + 4
    return None, i + 1


def plain_style() -> dict:
    return {"fg": None, "bg": None, "bold": False, "dim": False, "strike": False}


def to_html(line: str, style: dict) -> str:
    """One line of tmux's coloured capture as spans. tmux writes a style only
    when it changes, so `style` carries over from the line before, and is
    left as this line ends it."""
    out = []
    pos = 0

    def flush(text: str) -> None:
        if not text:
            return
        css = []
        if style["fg"]:
            css.append(f"color:{style['fg']}")
        if style["bg"]:
            css.append(f"background:{style['bg']}")
        if style["bold"]:
            css.append("font-weight:700")
        if style["dim"]:
            css.append("opacity:.55")
        if style["strike"]:
            css.append("text-decoration:line-through")
        body = html.escape(text)
        out.append(f'<span style="{";".join(css)}">{body}</span>' if css else body)

    for m in ANSI.finditer(line):
        flush(line[pos : m.start()])
        pos = m.end()
        params = [int(p) if p else 0 for p in m.group(1).split(";")] if m.group(1) else [0]
        i = 0
        while i < len(params):
            p = params[i]
            if p == 0:
                style.update(fg=None, bg=None, bold=False, dim=False, strike=False)
            elif p == 1:
                style["bold"] = True
            elif p == 2:
                style["dim"] = True
            elif p == 22:
                style["bold"] = style["dim"] = False
            elif p == 9:
                style["strike"] = True
            elif p == 29:
                style["strike"] = False
            elif 30 <= p <= 37:
                style["fg"] = PALETTE[p - 30]
            elif 90 <= p <= 97:
                style["fg"] = PALETTE[p - 90 + 8]
            elif p == 39:
                style["fg"] = None
            elif 40 <= p <= 47:
                style["bg"] = PALETTE[p - 40]
            elif p == 49:
                style["bg"] = None
            elif p in (38, 48):
                c, i = color(params, i + 1)
                style["fg" if p == 38 else "bg"] = c
                continue
            i += 1
    flush(line[pos:])
    return "".join(out) or " "


def render(capture: str, out: Path, work: Path) -> None:
    """A terminal window holding `capture`, as a PNG at twice the size."""
    lines = capture.rstrip("\n").splitlines()
    while lines and not plain(lines[-1]).strip():
        lines.pop()
    style = plain_style()
    body = "\n".join(to_html(l, style) for l in lines)
    font, line_h = 15, 22
    width = int(COLS * font * 0.6) + 48
    height = 38 + 20 + len(lines) * line_h + 24
    page = work / f"{out.stem}.html"
    page.write_text(f"""<!doctype html><meta charset="utf-8">
<style>
html,body{{margin:0;background:{BG};}}
.bar{{height:38px;background:#2b2d33;display:flex;align-items:center;position:relative;}}
.dot{{width:13px;height:13px;border-radius:50%;margin-left:8px;}}
.title{{position:absolute;left:0;right:0;text-align:center;color:#9aa0a6;font:13px -apple-system,sans-serif;}}
pre{{margin:0;padding:20px 24px 24px;color:{FG};font:{font}px/{line_h}px Menlo,"SF Mono",monospace;white-space:pre;}}
</style>
<div class="bar"><span class="dot" style="background:#ff5f57;margin-left:14px"></span><span class="dot" style="background:#febc2e"></span><span class="dot" style="background:#28c840"></span><span class="title">weft</span></div>
<pre>{body}</pre>""")
    subprocess.run(
        [CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=2",
         f"--window-size={width},{height}", f"--screenshot={out}", page.as_uri()],
        check=True, capture_output=True,
    )
    print(f"  wrote {out}", flush=True)


# ---- The run -----------------------------------------------------------------


def slug(path: Path) -> str:
    return str(path).replace("/", "-")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", default="~/Code/barometer", help="the demo repository, cloned and left alone")
    ap.add_argument("--out", default=str(Path(__file__).resolve().parent.parent / "docs"), help="where the PNGs go")
    ap.add_argument("--work", help="a folder for the clone and the pages (default: a new temporary one)")
    ap.add_argument("--weft", default="weft", help="the weft to run")
    ap.add_argument("--keep", action="store_true", help="keep the clone and weft's session files for it")
    args = ap.parse_args()

    repo = Path(args.repo).expanduser().resolve()
    work = Path(args.work).resolve() if args.work else Path(tempfile.mkdtemp(prefix="weft-shots-")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    out = Path(args.out).expanduser().resolve()
    out.mkdir(parents=True, exist_ok=True)
    demo = work / repo.name
    if demo.exists():
        shutil.rmtree(demo)
    sh("git", "clone", "-q", str(repo), str(demo))
    if (repo / "node_modules").exists():
        # A copy-on-write clone on APFS: quick, and no room taken until it differs.
        sh("cp", "-c", "-R", str(repo / "node_modules"), str(demo / "node_modules"))
    demo = demo.resolve()

    config = work / "config.toml"
    base = (Path.home() / ".config/weft/config.toml").read_text()
    config.write_text(f'{base.rstrip()}\n\n[[project]]\npath = "{demo}"\nverify = "{CHECKS}"\n')

    subprocess.run(["tmux", "kill-session", "-t", SESSION], capture_output=True)
    tmux("new-session", "-d", "-s", SESSION, "-x", str(COLS), "-y", str(ROWS), "-c", str(demo),
         f"{args.weft} --config {config}")
    tmux("set-option", "-t", SESSION, "remain-on-exit", "on")
    try:
        time.sleep(4)
        shots: dict[str, str] = {}
        for n, task in enumerate(TASKS):
            print(f"task {n + 1}: {task}", flush=True)
            run_task(task, lambda screen, n=n: shots.setdefault("approval", screen) if n == 0 else None)
        shots["weft"] = history()
        (work / "weft.ansi").write_text(shots["weft"])
        for name, capture in shots.items():
            render(capture, out / f"{name}.png", work)
    finally:
        subprocess.run(["tmux", "kill-session", "-t", SESSION], capture_output=True)
        if not args.keep:
            for kind in ("sessions", "checkpoints"):
                shutil.rmtree(Path.home() / ".local/share/weft" / kind / slug(demo), ignore_errors=True)
            memory = Path.home() / ".local/share/weft/memory" / f"{slug(demo)}.md"
            memory.unlink(missing_ok=True)
            shutil.rmtree(demo, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
