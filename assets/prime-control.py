import os
import select
import shutil
import signal
import sys
import time


def fence(pid, expected):
    try:
        fd = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    try:
        poll = select.poll()
        poll.register(fd, select.POLLIN)
        if poll.poll(0):
            return
        try:
            with open(f"/proc/{pid}/stat", "rb") as stat:
                fields = stat.read().rsplit(b") ", 1)[1].split()
        except FileNotFoundError:
            if poll.poll(0):
                return
            raise
        if fields[0] == b"Z" or b"proc:" + fields[19] != expected.encode():
            return
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                signal.pidfd_send_signal(fd, sig)
            except ProcessLookupError:
                return
            if poll.poll(500):
                return
        raise RuntimeError("Prime launch termination unproven")
    finally:
        os.close(fd)


def prepare(root, workers, token):
    fd = os.pidfd_open(os.getpid())
    try:
        signal.pidfd_send_signal(fd, 0)
    finally:
        os.close(fd)
    os.mkdir(root, 0o700)
    committed = False
    try:
        with open(os.path.join(root, ".prepare-token"), "x") as marker:
            marker.write(token)
        cleanup = os.path.join(root, "cleanup")
        os.mkdir(cleanup, 0o700)
        os.symlink(workers, os.path.join(cleanup, "daemon-workers"))
        print("ready", flush=True)
        deadline = time.monotonic() + 5
        commit = b""
        while not commit.endswith(b"\n"):
            remaining = max(0, deadline - time.monotonic())
            if not select.select([sys.stdin], [], [], remaining)[0]:
                raise TimeoutError("Prime preparation commit timed out")
            chunk = os.read(sys.stdin.fileno(), 128 - len(commit))
            if not chunk or len(commit) + len(chunk) >= 128:
                raise RuntimeError("Prime preparation commit refused")
            commit += chunk
        if commit != (token + "\n").encode():
            raise RuntimeError("Prime preparation commit refused")
        print("committed", flush=True)
        committed = True
    finally:
        if not committed:
            shutil.rmtree(root)


def rollback(root, token):
    try:
        with open(os.path.join(root, ".prepare-token")) as marker:
            actual = marker.read(128)
    except FileNotFoundError:
        if not os.path.lexists(root):
            return
        raise
    if actual != token:
        raise RuntimeError("Prime preparation ownership mismatch")
    shutil.rmtree(root)


if sys.argv[1] == "fence":
    fence(int(sys.argv[2]), sys.argv[3])
    print("settled", end="", flush=True)
elif sys.argv[1] == "prepare":
    prepare(*sys.argv[2:])
elif sys.argv[1] == "rollback":
    rollback(*sys.argv[2:])
else:
    raise ValueError("Unknown Prime control operation")
