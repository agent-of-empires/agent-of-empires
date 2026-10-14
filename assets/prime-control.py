import os
import select
import shutil
import signal
import sys
import time
import stat
import tarfile


def capture(pid, expected):
    try:
        fd = os.pidfd_open(pid)
    except ProcessLookupError:
        return None
    accepted = False
    try:
        poll = select.poll()
        poll.register(fd, select.POLLIN)
        if poll.poll(0):
            return None
        try:
            with open(f"/proc/{pid}/stat", "rb") as source:
                fields = source.read().rsplit(b") ", 1)[1].split()
        except FileNotFoundError:
            if poll.poll(0):
                return None
            raise
        if fields[0] == b"Z" or b"proc:" + fields[19] != expected.encode() or poll.poll(0):
            return None
        accepted = True
        return fd
    finally:
        if not accepted:
            os.close(fd)


def fence_many(pairs):
    handles = []
    try:
        for pid, expected in pairs:
            fd = capture(pid, expected)
            if fd is not None:
                handles.append(fd)
        active = set(handles)
        poll = select.poll()
        for fd in handles:
            poll.register(fd, select.POLLIN)
        for sig in (signal.SIGTERM, signal.SIGKILL):
            for fd in tuple(active):
                try:
                    signal.pidfd_send_signal(fd, sig)
                except ProcessLookupError:
                    active.discard(fd)
            deadline = time.monotonic() + 0.5
            while active:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                for fd, event in poll.poll(max(1, int(remaining * 1000))):
                    if event & select.POLLIN:
                        active.discard(fd)
                        poll.unregister(fd)
            if not active:
                return
        raise RuntimeError("Prime process termination unproven")
    finally:
        for fd in handles:
            os.close(fd)


def fence(pid, expected):
    fence_many([(pid, expected)])


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

def descriptor_archive(directory):
    if not stat.S_ISDIR(os.lstat(directory).st_mode):
        raise ValueError("Prime descriptor namespace is not a directory")
    with tarfile.open(fileobj=sys.stdout.buffer, mode="w|", dereference=False) as archive:
        with os.scandir(directory) as entries:
            for entry in entries:
                if entry.name.endswith(".json"):
                    archive.add(entry.path, arcname=entry.name, recursive=False)


if sys.argv[1] == "fence":
    fence(int(sys.argv[2]), sys.argv[3])
    print("settled", end="", flush=True)
elif sys.argv[1] == "fence-many":
    args = sys.argv[2:]
    if len(args) % 2:
        raise ValueError("Invalid process identity pairs")
    fence_many((int(args[index]), args[index + 1]) for index in range(0, len(args), 2))
    print("settled", end="", flush=True)
elif sys.argv[1] == "epoch":
    with open("/proc/sys/kernel/random/boot_id") as source:
        boot = source.read().strip()
    with open("/proc/1/stat", "rb") as source:
        start = source.read().rsplit(b") ", 1)[1].split()[19].decode()
    print(boot + "|" + os.readlink("/proc/self/ns/pid") + "|" + start, end="", flush=True)
elif sys.argv[1] == "prepare":
    prepare(*sys.argv[2:])
elif sys.argv[1] == "rollback":
    rollback(*sys.argv[2:])
elif sys.argv[1] == "descriptor-archive":
    descriptor_archive(sys.argv[2])
else:
    raise ValueError("Unknown Prime control operation")
