#!/usr/bin/env python3
"""Compare the pinned macOS Core on Node and Bun in disposable profiles."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parent.parent
BUN_SHA256 = '0a2237cdcfa0d16ef501d9acca9f0026309ef348bea7393a64e8d1e3609565cc'
SERVER_SHA256 = 'e5a295ffafbd11bcddc0de42b4d3632f1ba5d12f1413e5d9c59174fce4b23553'
TERMINAL_SHA256 = '1eeb6ec476884b533f1648f50d9571eda031b0928f35174c0d74feabc3e03bd6'

# Bun's TTY stream aborts on EAGAIN. Retry on the same fd until bytes arrive,
# EOF is reached, or node-pty destroys the stream. Keep native resize and writes.
BUN_PTY_STREAM = '''fs.createReadStream(null, { fd: term.fd, fs: {
            read(fd, buffer, offset, length, position, callback) {
                function attempt() {
                    if (_this._socket.destroyed) {
                        callback(null, 0, buffer);
                        return;
                    }
                    fs.read(fd, buffer, offset, length, position, (error, count) => {
                        if (error?.code === "EAGAIN") {
                            setTimeout(attempt, 8);
                        } else {
                            callback(error?.code === "EIO" ? null : error, count || 0, buffer);
                        }
                    });
                }
                attempt();
            },
            close: fs.close,
        } })'''


def verify(path, expected):
    with path.open('rb') as stream:
        digest = hashlib.sha256()
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    if digest.hexdigest() != expected:
        raise ValueError(f'SHA-256 mismatch for {path.name}')


def patch_file(path, expected, original, replacement, prefix=''):
    verify(path, expected)
    source = path.read_text()
    if source.count(original) != 1:
        raise ValueError(f'Compatibility patch no longer matches {path.name}')
    path.write_text(prefix + source.replace(original, replacement))


def unpack_core(archive, destination):
    with zipfile.ZipFile(archive) as bundle:
        for file in bundle.infolist():
            path = Path(file.filename)
            if path.is_absolute() or '..' in path.parts:
                raise ValueError('Core archive contains an unsafe path')
            extracted = Path(bundle.extract(file, destination))
            mode = (file.external_attr >> 16) & 0o777
            if mode:
                extracted.chmod(mode)


def runtime_processes(runtime):
    rows = subprocess.check_output(['/bin/ps', '-axww', '-o', 'pid=,command='], text=True)
    prefix = str(runtime) + ' '
    return [
        int(pid)
        for pid, command in (row.split(None, 1) for row in rows.splitlines())
        if command.startswith(prefix)
    ]


def stop(process, runtime):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
    deadline = time.monotonic() + 5
    while children := runtime_processes(runtime):
        for pid in children:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if time.monotonic() >= deadline:
            raise RuntimeError('Temporary Core process tree did not terminate')
        time.sleep(0.05)


def verify_start(runtime, core, manifest, profile):
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    env = {
        'PATH': '/usr/bin:/bin',
        'HOME': str(profile),
        'TMPDIR': str(profile),
        **{key: value.replace('{data_dir}', str(profile))
           for key, value in manifest['environment'].items()},
    }
    ready = queue.Queue()

    def read_events(stream):
        for line in stream:
            try:
                event = json.loads(line)
            except ValueError:
                continue
            if isinstance(event, dict) and event.get('type') == 'orca_server_ready':
                # Pairing credentials never leave the stdout reader.
                ready.put((event.get('boundEndpoint'), event.get('health', {})))
        ready.put(None)

    with subprocess.Popen(
        [str(runtime), str(core / 'orcad/orcad-server.js'), '--bind', '127.0.0.1',
         '--port', str(port), '--web-client-root', str(core / 'web'), '--json'],
        cwd=core / 'orcad', env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        start_new_session=True,
    ) as process:
        reader = threading.Thread(target=read_events, args=(process.stdout,), daemon=True)
        reader.start()
        try:
            try:
                result = ready.get(timeout=30)
            except queue.Empty:
                raise RuntimeError('Core readiness timed out') from None
            if result is None:
                raise RuntimeError(f'Core closed stdout before readiness (exit {process.wait(timeout=5)})')
            endpoint, health = result
            test = health.get('terminalDaemon', {}).get('selfTest', {})
            if (endpoint != f'ws://127.0.0.1:{port}' or health.get('pid') != process.pid
                    or health.get('buildVersion') != manifest['slot_version']
                    or test.get('ok') is not True or test.get('coverage') != 'pty-spawn'):
                raise RuntimeError('Core readiness or real PTY self-test failed')
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(f'http://127.0.0.1:{port}/web-index.html', timeout=3) as response:
                if response.status != 200 or response.headers.get_content_type() != 'text/html':
                    raise RuntimeError('Core did not serve the Web entry')
        finally:
            stop(process, runtime)
            reader.join(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bun', type=Path, required=True, help='Bun 1.4.4 macOS arm64 binary')
    parser.add_argument('--sqlite', type=Path,
                        default=Path('/opt/homebrew/opt/sqlite/lib/libsqlite3.dylib'))
    parser.add_argument('--core-dir', type=Path, default=ROOT / 'dist-core/macos-arm64')
    args = parser.parse_args()

    if platform.system() != 'Darwin' or platform.machine() != 'arm64':
        raise SystemExit('This experiment requires Apple Silicon macOS')
    verify(args.bun, BUN_SHA256)
    sqlite = args.sqlite.resolve(strict=True)
    archive = args.core_dir / 'bootstrap-core.zip'
    seed = json.loads((args.core_dir / 'bootstrap-core.json').read_text())
    verify(archive, seed['sha256'])

    with tempfile.TemporaryDirectory(prefix='orca-bun-smoke-') as directory:
        scratch = Path(directory).resolve()
        core = scratch / 'core'
        unpack_core(archive, core)
        manifest = json.loads((core / 'core-manifest.json').read_text())
        lock = json.loads((ROOT / 'core-version.macos-arm64.json').read_text())
        if (manifest['version'], manifest['target'], manifest['upstream_commit']) != (
            lock['version'], lock['target'], lock['upstream']['commit'],
        ):
            raise ValueError('Core archive differs from the pinned macOS release')
        node = core / f'runtimes/node-{lock["node"]["executable_sha256"]}/bin/node'
        verify(node, lock['node']['executable_sha256'])
        node_profile = scratch / 'node-profile'
        node_profile.mkdir(mode=0o700)
        verify_start(node, core, manifest, node_profile)
        print('Node baseline: readiness, real PTY, HTTP and process cleanup passed')

        runtime = scratch / 'bun'
        shutil.copy2(args.bun, runtime)
        profile = scratch / 'bun-profile'
        profile.mkdir(mode=0o700)

        guard = 'function bla(){'
        allow_pinned_bun = (
            f'if(process.versions.bun==="1.4.4"&&process.execPath==={json.dumps(str(runtime))})return;'
        )
        patch_file(core / 'orcad/orcad-server.js', SERVER_SHA256, guard, guard + allow_pinned_bun,
                   prefix=f'require("bun:sqlite").Database.setCustomSQLite({json.dumps(str(sqlite))});\n')
        patch_file(core / 'orcad/node_modules/node-pty/lib/unixTerminal.js',
                   TERMINAL_SHA256, 'new tty.ReadStream(term.fd)', BUN_PTY_STREAM)
        for stage in ('fresh profile', 'same-profile restart'):
            verify_start(runtime, core, manifest, profile)
            print(f'Bun {stage}: readiness, real PTY, HTTP and process cleanup passed')


if __name__ == '__main__':
    main()
