"""Check the installed launcher and its native popup in an isolated X11 session."""

import subprocess
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi


def descendants(node):
    yield node
    for index in range(node.get_child_count()):
        child = node.get_child_at_index(index)
        if child:
            yield from descendants(child)


def wait_for(description, probe):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        time.sleep(0.15)
    raise RuntimeError(f"Timed out waiting for {description}")


def smoke(pid):
    desktop = Atspi.get_desktop(0)

    def ratio_app():
        for index in range(desktop.get_child_count()):
            app = desktop.get_child_at_index(index)
            if app and app.get_process_id() == pid:
                return app
        return None

    app = wait_for("Ratio's accessibility tree", ratio_app)

    def named(role, name):
        return next((node for node in descendants(app)
                     if node.get_role() == role and node.get_name() == name), None)

    wait_for("the connection form", lambda: named(Atspi.Role.PUSH_BUTTON, "Connect"))
    if any(node.get_role() == Atspi.Role.MENU_BAR for node in descendants(app)):
        raise RuntimeError("Found a persistent menu bar in the launcher")

    # WebKitGTK versions expose a button with aria-haspopup as a button or combo box.
    controls = wait_for("Desktop controls", lambda: next(
        (node for node in descendants(app)
         if node.get_name() == "Desktop controls" and node.get_n_actions() > 0), None))
    if not controls.do_action(0):
        raise RuntimeError("Could not activate Desktop controls")
    expected = {"Back", "Forward", "Reload", "Connection settings…", "Quit"}

    def popup_items():
        items = {node.get_name(): node for node in descendants(app)
                 if node.get_role() == Atspi.Role.MENU_ITEM
                 and node.get_state_set().contains(Atspi.StateType.SHOWING)}
        return items if items.keys() >= expected else None

    popup = wait_for("the five visible native commands", popup_items)
    if not popup["Quit"].do_action(0):
        raise RuntimeError("Could not activate Quit")


if __name__ == "__main__":
    # CI installs the just-built package before starting its own Xvfb and D-Bus.
    with subprocess.Popen([sys.argv[1] if len(sys.argv) > 1 else "ratio"]) as process:
        try:
            smoke(process.pid)
            if process.wait(timeout=10) != 0:
                raise RuntimeError("The Quit command failed")
        finally:
            if process.poll() is None:
                process.kill()
    print("Verified the connection screen, the menu-free window and all five desktop controls.")
