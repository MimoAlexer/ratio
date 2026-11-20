"""Check the installed launcher and its native popup in an isolated X11 session."""

import os
import subprocess
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Atspi, Gdk, Gio, GLib


def open_x11_display():
    name = os.environ.get("DISPLAY")
    if not name:
        raise RuntimeError("X11 preflight failed: DISPLAY is unset")
    Gdk.set_allowed_backends("x11")
    display = Gdk.Display.open(name)
    if display is None:
        raise RuntimeError(f"X11 preflight failed: could not open {name!r}. Check Xvfb and XAUTHORITY.")
    return display


def descendants(node):
    yield node
    for index in range(node.get_child_count()):
        child = node.get_child_at_index(index)
        if child:
            yield from descendants(child)


def is_no_reply(error):
    # Jammy's libatspi wraps the D-Bus NoReply error in its own error domain.
    if error.domain == "atspi_error" and error.code == 1:
        return error.message.startswith("Did not receive a reply.")
    return error.matches(Gio.io_error_quark(), Gio.IOErrorEnum.TIMED_OUT) or any(
        error.matches(Gio.dbus_error_quark(), code)
        for code in (Gio.DBusError.NO_REPLY, Gio.DBusError.TIMEOUT, Gio.DBusError.TIMED_OUT)
    )


def wait_for(process, description, probe):
    deadline = time.monotonic() + 30
    last_error = None
    while True:
        status = process.poll()
        if status is not None:
            raise RuntimeError(
                f"Ratio (PID {process.pid}) exited with status {status} while waiting for {description}"
            )
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Timed out waiting for {description} from Ratio (PID {process.pid})") from last_error
        try:
            result = probe()
        except GLib.Error as error:
            if not is_no_reply(error) and process.poll() is None:
                raise
            last_error = error
        else:
            if result:
                return result
        time.sleep(0.15)


def smoke(process):
    Atspi.set_timeout(1000, 2000)

    def ratio_app():
        desktop = Atspi.get_desktop(0)
        for index in range(desktop.get_child_count()):
            app = desktop.get_child_at_index(index)
            if app and app.get_process_id() == process.pid:
                return app
        return None

    app = wait_for(process, "Ratio's accessibility tree", ratio_app)

    def named(role, name):
        return next((node for node in descendants(app)
                     if node.get_role() == role and node.get_name() == name), None)

    def connection_form():
        if not named(Atspi.Role.PUSH_BUTTON, "Connect"):
            return False
        if any(node.get_role() == Atspi.Role.MENU_BAR for node in descendants(app)):
            raise RuntimeError("Found a persistent menu bar in the launcher")
        return True

    wait_for(process, "the connection form", connection_form)

    # WebKitGTK versions expose a button with aria-haspopup as a button or combo box.
    controls = wait_for(process, "Desktop controls", lambda: next(
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

    popup = wait_for(process, "the five visible native commands", popup_items)
    if not popup["Quit"].do_action(0):
        raise RuntimeError("Could not activate Quit")


if __name__ == "__main__":
    # CI installs the just-built package before starting its own Xvfb and D-Bus.
    # Hold an authenticated connection so Xvfb cannot reset between preflight and launch.
    display = open_x11_display()
    with subprocess.Popen([sys.argv[1] if len(sys.argv) > 1 else "ratio"],
                          env={**os.environ, "GDK_BACKEND": "x11"}) as process:
        try:
            smoke(process)
            status = process.wait(timeout=10)
            if status != 0:
                raise RuntimeError(f"Ratio (PID {process.pid}) exited with status {status} after Quit")
        finally:
            if process.poll() is None:
                process.kill()
    print("Verified the connection screen, the menu-free window and all five desktop controls.")
