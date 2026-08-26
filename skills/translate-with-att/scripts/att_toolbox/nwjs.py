"""通过 Chrome DevTools Protocol 观察 RPG Maker NW.js 运行时。"""

from __future__ import annotations

import base64
import contextlib
import ctypes
import hashlib
import json
import os
import re
import socket
import struct
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping, Sequence
from ctypes import wintypes
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Self, cast
from urllib.parse import unquote, urlsplit


class CdpUnavailableError(RuntimeError):
    """NW.js 的本地调试入口不可用。"""


class CdpProtocolError(RuntimeError):
    """本地 CDP/WebSocket 响应不满足协议。"""


class CdpEvaluationError(CdpProtocolError):
    """页面表达式本身抛错；CDP 连接仍可供后续场景使用。"""


class _MibTcpRowOwnerPid(ctypes.Structure):
    _fields_ = [
        ("state", wintypes.DWORD),
        ("local_address", wintypes.DWORD),
        ("local_port", wintypes.DWORD),
        ("remote_address", wintypes.DWORD),
        ("remote_port", wintypes.DWORD),
        ("owning_pid", wintypes.DWORD),
    ]


class _ProcessEntry32W(ctypes.Structure):
    _fields_ = [
        ("size", wintypes.DWORD),
        ("usage", wintypes.DWORD),
        ("process_id", wintypes.DWORD),
        ("default_heap_id", ctypes.c_size_t),
        ("module_id", wintypes.DWORD),
        ("thread_count", wintypes.DWORD),
        ("parent_process_id", wintypes.DWORD),
        ("base_priority", wintypes.LONG),
        ("flags", wintypes.DWORD),
        ("executable", wintypes.WCHAR * 260),
    ]


@dataclass(frozen=True, slots=True)
class CdpTarget:
    title: str
    url: str
    websocket_url: str


def normalized_file_target_path(url: str) -> Path | None:
    """只把本机 file URL 规范化为自然 Windows 路径。"""

    parsed = urlsplit(url)
    if parsed.scheme.casefold() != "file" or parsed.hostname not in {None, "", "localhost"}:
        return None
    value = unquote(parsed.path)
    if re.match(r"^/[A-Za-z]:/", value):
        value = value[1:]
    if not value:
        return None
    return Path(value.replace("/", os.sep)).resolve(strict=False)


def matching_content_targets(
    targets: Sequence[CdpTarget],
    *,
    expected_content_root: Path,
    expected_entry: Path,
) -> tuple[CdpTarget, ...]:
    """返回 URL 精确落在期望内容根且等于自然入口的 page/webview。"""

    content_root = expected_content_root.resolve(strict=True)
    entry = expected_entry.resolve(strict=True)
    try:
        entry.relative_to(content_root)
    except ValueError as error:
        raise ValueError("expected_entry 必须位于 expected_content_root 内") from error
    result: list[CdpTarget] = []
    for target in targets:
        path = normalized_file_target_path(target.url)
        if path is None:
            continue
        try:
            path.relative_to(content_root)
        except ValueError:
            continue
        if path == entry:
            result.append(target)
    return tuple(result)


def unique_content_target(
    targets: Sequence[CdpTarget],
    *,
    expected_content_root: Path,
    expected_entry: Path,
) -> CdpTarget:
    """要求页面目标唯一对应期望内容根的自然入口。"""

    matching = matching_content_targets(
        targets,
        expected_content_root=expected_content_root,
        expected_entry=expected_entry,
    )
    if not matching:
        raise CdpUnavailableError("NW.js 尚未出现属于期望游戏入口的页面")
    if len(matching) > 1:
        raise CdpProtocolError("NW.js 返回了多个属于期望游戏入口的页面")
    return next(iter(matching))


def reserve_loopback_port() -> int:
    """取得一个当前可用的 IPv4 loopback 端口。"""

    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def build_nwjs_command(executable: Path, *, port: int, profile: Path) -> tuple[str, ...]:
    if port < 1 or port > 65_535:
        raise ValueError("port 必须位于 1..65535")
    return (
        str(executable),
        "--remote-debugging-address=127.0.0.1",
        f"--remote-debugging-port={port}",
        f"--user-data-dir={profile}",
    )


def loopback_listener_pids(port: int) -> frozenset[int]:
    """用 Windows TCP owner 表返回精确监听 127.0.0.1:port 的 PID。"""

    if os.name != "nt":
        raise CdpUnavailableError("NW.js 监听进程归属检查只支持 Windows")
    if port < 1 or port > 65_535:
        raise ValueError("port 必须位于 1..65535")
    library = ctypes.WinDLL("iphlpapi.dll", use_last_error=True)
    get_table = library.GetExtendedTcpTable
    get_table.argtypes = (
        ctypes.c_void_p,
        ctypes.POINTER(wintypes.DWORD),
        wintypes.BOOL,
        wintypes.ULONG,
        ctypes.c_int,
        wintypes.ULONG,
    )
    get_table.restype = wintypes.DWORD
    size = wintypes.DWORD(0)
    while True:
        buffer = ctypes.create_string_buffer(size.value) if size.value else None
        result = int(
            get_table(
                buffer,
                ctypes.byref(size),
                True,
                socket.AF_INET,
                3,  # TCP_TABLE_OWNER_PID_LISTENER
                0,
            )
        )
        if result == 122:  # ERROR_INSUFFICIENT_BUFFER；表增长时按系统返回大小重读。
            continue
        if result != 0 or buffer is None:
            raise CdpUnavailableError("无法读取 Windows TCP 监听进程归属")
        break
    count = int(wintypes.DWORD.from_buffer_copy(buffer.raw, 0).value)
    row_size = ctypes.sizeof(_MibTcpRowOwnerPid)
    expected_size = ctypes.sizeof(wintypes.DWORD) + count * row_size
    if len(buffer.raw) < expected_size:
        raise CdpProtocolError("Windows TCP 监听进程表长度无效")
    result_pids: set[int] = set()
    for index in range(count):
        row = _MibTcpRowOwnerPid.from_buffer_copy(
            buffer.raw,
            ctypes.sizeof(wintypes.DWORD) + index * row_size,
        )
        address = socket.inet_ntoa(int(row.local_address).to_bytes(4, byteorder="little"))
        local_port = socket.ntohs(int(row.local_port) & 0xFFFF)
        if address == "127.0.0.1" and local_port == port:
            result_pids.add(int(row.owning_pid))
    return frozenset(result_pids)


def process_tree_pids(root_pid: int, *, require_root: bool = True) -> frozenset[int]:
    """用 Toolhelp32 当前快照返回 root 及其递归后代 PID。"""

    if os.name != "nt":
        raise CdpUnavailableError("NW.js 进程树归属检查只支持 Windows")
    if root_pid <= 0:
        raise ValueError("root_pid 必须是正整数")
    library = ctypes.WinDLL("kernel32.dll", use_last_error=True)
    create_snapshot = library.CreateToolhelp32Snapshot
    create_snapshot.argtypes = (wintypes.DWORD, wintypes.DWORD)
    create_snapshot.restype = wintypes.HANDLE
    process_first = library.Process32FirstW
    process_first.argtypes = (wintypes.HANDLE, ctypes.POINTER(_ProcessEntry32W))
    process_first.restype = wintypes.BOOL
    process_next = library.Process32NextW
    process_next.argtypes = (wintypes.HANDLE, ctypes.POINTER(_ProcessEntry32W))
    process_next.restype = wintypes.BOOL
    close_handle = library.CloseHandle
    close_handle.argtypes = (wintypes.HANDLE,)
    close_handle.restype = wintypes.BOOL
    snapshot = create_snapshot(0x00000002, 0)  # TH32CS_SNAPPROCESS
    if snapshot == wintypes.HANDLE(-1).value:
        raise CdpUnavailableError("无法建立 Windows 进程快照")
    parents: dict[int, int] = {}
    try:
        entry = _ProcessEntry32W()
        entry.size = ctypes.sizeof(_ProcessEntry32W)
        if not process_first(snapshot, ctypes.byref(entry)):
            raise CdpUnavailableError("Windows 进程快照没有可读取条目")
        while True:
            parents[int(entry.process_id)] = int(entry.parent_process_id)
            entry.size = ctypes.sizeof(_ProcessEntry32W)
            if not process_next(snapshot, ctypes.byref(entry)):
                break
    finally:
        _ = close_handle(snapshot)
    if require_root and root_pid not in parents:
        return frozenset()
    owned = {root_pid}
    changed = True
    while changed:
        changed = False
        for process_id, parent_id in parents.items():
            if process_id not in owned and parent_id in owned:
                owned.add(process_id)
                changed = True
    return frozenset(owned)


def owned_loopback_listener_pid(port: int, root_pid: int) -> int | None:
    """证明唯一 loopback listener 属于 root 或其当前后代；尚未监听返回 None。"""

    listeners = loopback_listener_pids(port)
    if not listeners:
        return None
    owned = process_tree_pids(root_pid)
    if not owned:
        raise CdpUnavailableError("本工具启动的 NW.js 根进程已不在当前进程快照")
    unrelated = listeners - owned
    matching = listeners & owned
    if unrelated:
        raise CdpUnavailableError("调试端口已被无关进程监听")
    if len(matching) != 1:
        raise CdpUnavailableError("调试端口没有唯一的自有监听进程")
    return next(iter(matching))


def wait_for_owned_loopback_listener(
    port: int,
    *,
    root_pid: int,
    timeout: float,
    process_exited: Callable[[], bool] | None = None,
) -> int:
    """等待自有 NW.js 根进程或其后代取得调试端口。"""

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process_exited is not None and process_exited():
            raise CdpUnavailableError("NW.js 在建立自有调试监听前已经退出")
        owner = owned_loopback_listener_pid(port, root_pid)
        if owner is not None:
            return owner
        time.sleep(0.05)
    raise CdpUnavailableError("等待 NW.js 自有调试监听超时")


def _read_targets(port: int, timeout: float) -> tuple[CdpTarget, ...]:
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/json/list",
        headers={"Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
    except (OSError, urllib.error.URLError) as error:
        raise CdpUnavailableError("本地 CDP 目标列表暂不可用") from error
    try:
        value = cast(object, json.loads(raw))
    except (UnicodeError, json.JSONDecodeError) as error:
        raise CdpProtocolError("本地 CDP 目标列表不是有效 JSON") from error
    if not isinstance(value, list):
        raise CdpProtocolError("本地 CDP 目标列表根值不是 array")
    targets: list[CdpTarget] = []
    for raw_item in cast(list[object], value):
        if not isinstance(raw_item, dict):
            continue
        item = cast(dict[object, object], raw_item)
        if item.get("type") not in {"page", "webview"}:
            continue
        title = item.get("title")
        url = item.get("url")
        websocket_url = item.get("webSocketDebuggerUrl")
        if not isinstance(title, str) or not isinstance(url, str) or not isinstance(websocket_url, str):
            continue
        parsed = urlsplit(websocket_url)
        if parsed.scheme != "ws" or parsed.hostname != "127.0.0.1" or parsed.port != port:
            raise CdpProtocolError("CDP 返回了非 loopback WebSocket 地址")
        targets.append(CdpTarget(title=title, url=url, websocket_url=websocket_url))
    return tuple(targets)


def wait_for_page_target(
    port: int,
    *,
    timeout: float,
    expected_content_root: Path,
    expected_entry: Path,
    process_exited: Callable[[], bool] | None = None,
) -> CdpTarget:
    """等待唯一属于期望内容根和自然入口的可调试页面。"""

    deadline = time.monotonic() + timeout
    last_problem: BaseException | None = None
    while time.monotonic() < deadline:
        if process_exited is not None and process_exited():
            raise CdpUnavailableError("NW.js 在调试页面建立前已经退出")
        try:
            targets = _read_targets(port, min(1.0, max(0.1, deadline - time.monotonic())))
            if targets:
                return unique_content_target(
                    targets,
                    expected_content_root=expected_content_root,
                    expected_entry=expected_entry,
                )
        except CdpProtocolError:
            raise
        except CdpUnavailableError as error:
            last_problem = error
        time.sleep(0.05)
    raise CdpUnavailableError("等待属于期望游戏入口的 NW.js 调试页面超时") from last_problem


class CdpConnection:
    """不依赖第三方库的最小 CDP WebSocket 客户端。"""

    def __init__(self, websocket_url: str, *, timeout: float = 10.0) -> None:
        parsed = urlsplit(websocket_url)
        if parsed.scheme != "ws" or parsed.hostname != "127.0.0.1" or parsed.port is None:
            raise CdpProtocolError("只允许连接精确的 IPv4 loopback CDP 地址")
        self._socket: socket.socket | None = socket.create_connection(
            (parsed.hostname, parsed.port), timeout=timeout
        )
        self._active_socket().settimeout(timeout)
        self._receive_buffer = bytearray()
        self._next_id = 1
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{parsed.port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode("ascii")
        self._active_socket().sendall(request)
        response = self._receive_http_headers()
        status_line, *header_lines = response.decode("latin-1").split("\r\n")
        if " 101 " not in f" {status_line} ":
            self.close()
            raise CdpProtocolError("CDP WebSocket upgrade 没有返回 101")
        headers: dict[str, str] = {}
        for line in header_lines:
            if ":" not in line:
                continue
            name, value = line.split(":", 1)
            headers[name.strip().casefold()] = value.strip()
        expected = base64.b64encode(
            hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")).digest(),
        ).decode("ascii")
        if headers.get("sec-websocket-accept") != expected:
            self.close()
            raise CdpProtocolError("CDP WebSocket 握手摘要无效")

    def __enter__(self) -> Self:
        return self

    def __exit__(self, _kind: object, _value: object, _traceback: object) -> None:
        self.close()

    def close(self) -> None:
        sock = self._socket
        if sock is None:
            return
        with contextlib.suppress(OSError):
            self._send_frame(b"", opcode=0x8)
        try:
            sock.close()
        finally:
            self._socket = None

    def _active_socket(self) -> socket.socket:
        if self._socket is None:
            raise CdpUnavailableError("CDP WebSocket 已关闭")
        return self._socket

    def _receive_http_headers(self) -> bytes:
        data = bytearray()
        while b"\r\n\r\n" not in data:
            chunk = self._active_socket().recv(4096)
            if not chunk:
                raise CdpProtocolError("CDP WebSocket 握手提前结束")
            data.extend(chunk)
        headers, extra = bytes(data).split(b"\r\n\r\n", 1)
        self._receive_buffer.extend(extra)
        return headers

    def _receive_exact(self, size: int) -> bytes:
        chunks = bytearray()
        if self._receive_buffer:
            take = min(size, len(self._receive_buffer))
            chunks.extend(self._receive_buffer[:take])
            del self._receive_buffer[:take]
        while len(chunks) < size:
            chunk = self._active_socket().recv(size - len(chunks))
            if not chunk:
                raise CdpProtocolError("CDP WebSocket 连接提前结束")
            chunks.extend(chunk)
        return bytes(chunks)

    def _send_frame(self, payload: bytes, *, opcode: int) -> None:
        first = 0x80 | opcode
        length = len(payload)
        if length < 126:
            header = bytes((first, 0x80 | length))
        elif length <= 0xFFFF:
            header = bytes((first, 0x80 | 126)) + struct.pack("!H", length)
        else:
            header = bytes((first, 0x80 | 127)) + struct.pack("!Q", length)
        mask = os.urandom(4)
        masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        self._active_socket().sendall(header + mask + masked)

    def _receive_message(self) -> bytes:
        fragments = bytearray()
        initial_opcode: int | None = None
        while True:
            first, second = self._receive_exact(2)
            final = bool(first & 0x80)
            opcode = first & 0x0F
            masked = bool(second & 0x80)
            length = second & 0x7F
            if length == 126:
                length = struct.unpack("!H", self._receive_exact(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", self._receive_exact(8))[0]
            mask = self._receive_exact(4) if masked else b""
            payload = self._receive_exact(length)
            if masked:
                payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
            if opcode == 0x8:
                raise CdpUnavailableError("CDP WebSocket 已关闭")
            if opcode == 0x9:
                self._send_frame(payload, opcode=0xA)
                continue
            if opcode == 0xA:
                continue
            if opcode in {0x1, 0x2}:
                if initial_opcode is not None:
                    raise CdpProtocolError("CDP WebSocket 收到交错数据消息")
                initial_opcode = opcode
            elif opcode != 0x0 or initial_opcode is None:
                raise CdpProtocolError("CDP WebSocket 收到无效帧序列")
            fragments.extend(payload)
            if final:
                if initial_opcode != 0x1:
                    raise CdpProtocolError("CDP WebSocket 返回了非文本数据消息")
                return bytes(fragments)

    def call(
        self,
        method: str,
        params: Mapping[str, object] | None = None,
    ) -> dict[str, Any]:
        request_id = self._next_id
        self._next_id += 1
        request: dict[str, object] = {"id": request_id, "method": method}
        if params is not None:
            request["params"] = dict(params)
        self._send_frame(json.dumps(request, separators=(",", ":")).encode("utf-8"), opcode=0x1)
        while True:
            try:
                raw = self._receive_message()
                value = cast(object, json.loads(raw))
            except (UnicodeError, json.JSONDecodeError) as error:
                raise CdpProtocolError("CDP WebSocket 返回了无效 JSON") from error
            if not isinstance(value, dict):
                raise CdpProtocolError("CDP WebSocket 消息根值不是 object")
            typed = {str(key): member for key, member in cast(dict[object, object], value).items()}
            if "method" in typed and "id" not in typed:
                continue
            if typed.get("id") != request_id:
                raise CdpProtocolError("CDP WebSocket 响应编号与当前请求不一致")
            if "error" in typed:
                raise CdpProtocolError(f"CDP 拒绝方法 {method}")
            result = typed.get("result")
            if not isinstance(result, dict):
                raise CdpProtocolError(f"CDP 方法 {method} 缺少 object 结果")
            return cast(dict[str, Any], result)

    def evaluate(self, expression: str, *, await_promise: bool = False) -> Any:
        result = self.call(
            "Runtime.evaluate",
            {
                "expression": expression,
                "returnByValue": True,
                "awaitPromise": await_promise,
            },
        )
        exception = result.get("exceptionDetails")
        if exception is not None:
            raise CdpEvaluationError("NW.js 页面执行观察表达式失败")
        remote = result.get("result")
        if not isinstance(remote, dict):
            raise CdpProtocolError("Runtime.evaluate 缺少远端结果")
        typed_remote = cast(dict[object, object], remote)
        if typed_remote.get("subtype") == "error":
            raise CdpEvaluationError("NW.js 页面返回脚本错误")
        return typed_remote.get("value")


OBSERVER_SCRIPT = r"""
(() => {
  if (window.__ATT_NW_OBSERVER__) return {installed: true, reused: true};
  const state = {
    events: [], runtimeErrors: [], runtimeErrorKeys: Object.create(null),
    sequence: 0, installedAt: Date.now(), installed: {},
    pageLoadFinished: document.readyState === "complete", pollTicks: 0,
    installFinished: false, installTimer: null
  };
  const clean = value => String(value == null ? "" : value).replace(/[\u0000-\u001f\u007f]/g, " ");
  const sceneName = () => {
    const scene = window.SceneManager && SceneManager._scene;
    return scene && scene.constructor ? clean(scene.constructor.name) : "";
  };
  const recordRuntimeError = (kind, message, source, line, column, stack) => {
    const item = {
      timestampMs: Date.now(), kind: clean(kind), message: clean(message),
      source: clean(source), line: Number(line || 0), column: Number(column || 0),
      stack: clean(stack), scene: sceneName()
    };
    const key = [item.message, item.source, item.line, item.column].join("|");
    if (!item.message || state.runtimeErrorKeys[key]) return;
    state.runtimeErrorKeys[key] = true;
    state.runtimeErrors.push(item);
  };
  const captureErrorPrinter = () => {
    const node = document.getElementById("ErrorPrinter");
    if (!node) return;
    const text = clean(node.innerText || node.textContent || "").trim();
    if (text) recordRuntimeError("error_printer", text, "", 0, 0, "");
  };
  window.addEventListener("error", event => {
    const error = event && event.error;
    recordRuntimeError(
      "uncaught_error", event && event.message, event && event.filename,
      event && event.lineno, event && event.colno, error && error.stack
    );
  });
  window.addEventListener("unhandledrejection", event => {
    const reason = event && event.reason;
    recordRuntimeError(
      "unhandled_rejection",
      reason && (reason.message || reason.stack) || reason,
      "", 0, 0, reason && reason.stack
    );
  });
  const fontEvidence = owner => {
    const face = clean(owner && owner.fontFace || "");
    const size = Number(owner && owner.fontSize || 0);
    let requestedLoaded = null;
    try {
      requestedLoaded = !!(document.fonts && face && size && document.fonts.check(size + 'px "' + face.replace(/"/g, '\\"') + '"'));
    } catch (_) {}
    return {requestedFontFace: face, requestedFontSize: size, requestedFontLoaded: requestedLoaded, glyphFallback: "unverified"};
  };
  const record = (kind, text, geometry, context, font) => {
    const value = clean(text);
    const item = {
      sequence: ++state.sequence,
      timestampMs: Date.now(),
      kind,
      text: value,
      scene: sceneName(),
      context: clean(context),
      geometry: geometry || {},
      font: font || {requestedFontFace:"", requestedFontSize:0, requestedFontLoaded:null, glyphFallback:"unverified"}
    };
    state.events.push(item);
  };
  const patch = (prototype, name, wrapper) => {
    if (!prototype || typeof prototype[name] !== "function") return false;
    const original = prototype[name];
    if (original.__attObserverWrapped) return true;
    const wrapped = wrapper(original);
    Object.defineProperty(wrapped, "__attObserverWrapped", {value: true});
    prototype[name] = wrapped;
    return true;
  };
  const hookRequirements = () => ({
    bitmapDrawText: state.installed.bitmapDrawText === true,
    windowDrawText: state.installed.windowDrawText === true,
    windowDrawTextEx: state.installed.windowDrawTextEx === true,
    addCommand: state.installed.addCommand === true,
    loadFont: !window.Graphics || typeof Graphics.loadFont !== "function" || state.installed.loadFont === true,
    fontManagerLoad: !window.FontManager || typeof FontManager.load !== "function" || state.installed.fontManagerLoad === true,
    graphicsPrintError: !window.Graphics || typeof Graphics.printError !== "function" || state.installed.graphicsPrintError === true,
    graphicsPrintLoadingError: !window.Graphics || typeof Graphics.printLoadingError !== "function" || state.installed.graphicsPrintLoadingError === true
  });
  const snapshot = () => {
    const requirements = hookRequirements();
    return {
      installed: true,
      hooks: Object.assign({}, state.installed),
      hookRequirements: requirements,
      requiredHooksInstalled: Object.keys(requirements).every(name => requirements[name] === true),
      pageLoadFinished: state.pageLoadFinished === true,
      pollingObserved: state.pollTicks > 0,
      pollingActive: state.installTimer !== null,
      installationFinished: state.installFinished === true,
      sequence: state.sequence,
      scene: sceneName()
    };
  };
  const installHooks = () => {
  const installed = state.installed;
  installed.bitmapDrawText = patch(window.Bitmap && Bitmap.prototype, "drawText", original => function(text, x, y, maxWidth, lineHeight, align) {
    let measuredWidth = null;
    try {
      const measured = Number(this.measureTextWidth(String(text)));
      if (Number.isFinite(measured)) measuredWidth = measured;
    } catch (_) {}
    const width = Number(this.width || 0);
    const height = Number(this.height || 0);
    const px = Number(x || 0);
    const py = Number(y || 0);
    const allowed = Number(maxWidth == null ? width - px : maxWidth);
    const clipRight = Number.isFinite(width) && Number.isFinite(px) && Number.isFinite(allowed)
      ? Math.min(width, px + allowed) : null;
    const overflowLeft = Number.isFinite(px) ? px < 0 : null;
    const overflowRight = measuredWidth == null || clipRight == null
      ? null : px + measuredWidth > clipRight;
    record("Bitmap.drawText", text, {
      x: px, y: py, maxWidth: allowed, lineHeight: Number(lineHeight || 0),
      measuredWidth, bitmapWidth: width, bitmapHeight: height, clipRight, align: clean(align),
      measurementStatus: measuredWidth == null ? "unverified_measurement_unavailable" : "measured",
      overflowLeft, overflowRight,
      clippingOverflow: overflowLeft === true || overflowRight === true
        ? true : (overflowLeft === false && overflowRight === false ? false : null),
      overflowBottom: Number(lineHeight || 0) > 0 ? py + Number(lineHeight) > height : null
    }, this.fontFace || "", fontEvidence(this));
    return original.apply(this, arguments);
  });
  installed.windowDrawText = patch(window.Window_Base && Window_Base.prototype, "drawText", original => function(text, x, y, maxWidth, align) {
    const contentsWidth = this.contentsWidth ? Number(this.contentsWidth()) : Number(this.contents && this.contents.width || 0);
    record("Window_Base.drawText", text, {
      x: Number(x || 0), y: Number(y || 0), maxWidth: Number(maxWidth == null ? contentsWidth - Number(x || 0) : maxWidth),
      contentsWidth, windowX: Number(this.x || 0), windowY: Number(this.y || 0),
      windowWidth: Number(this.width || 0), windowHeight: Number(this.height || 0)
    }, this.constructor && this.constructor.name || "", fontEvidence(this.contents));
    return original.apply(this, arguments);
  });
  installed.windowDrawTextEx = patch(window.Window_Base && Window_Base.prototype, "drawTextEx", original => function(text, x, y, width) {
    const contentsWidth = this.contentsWidth ? Number(this.contentsWidth()) : Number(this.contents && this.contents.width || 0);
    const px = Number(x || 0);
    const rawText = String(text == null ? "" : text);
    let measuredWidth = null;
    let measurementStatus = "unverified_control_or_multiline";
    if (!/[\\\r\n\x1b]/.test(rawText)) {
      measurementStatus = "unverified_measurement_unavailable";
      try {
        const measured = typeof this.textWidth === "function"
          ? Number(this.textWidth(rawText))
          : Number(this.contents && this.contents.measureTextWidth(rawText));
        if (Number.isFinite(measured)) {
          measuredWidth = measured;
          measurementStatus = "measured_plain_text";
        }
      } catch (_) {}
    }
    const overflowLeft = Number.isFinite(px) ? px < 0 : null;
    const overflowRight = measuredWidth == null || !Number.isFinite(contentsWidth)
      ? null : px + measuredWidth > contentsWidth;
    record("Window_Base.drawTextEx", text, {
      x: px, y: Number(y || 0), maxWidth: Number(width == null ? contentsWidth - px : width),
      measuredWidth, measurementStatus, overflowLeft, overflowRight,
      clippingOverflow: overflowLeft === true || overflowRight === true
        ? true : (overflowLeft === false && overflowRight === false ? false : null),
      contentsWidth, windowX: Number(this.x || 0), windowY: Number(this.y || 0),
      windowWidth: Number(this.width || 0), windowHeight: Number(this.height || 0)
    }, this.constructor && this.constructor.name || "", fontEvidence(this.contents));
    return original.apply(this, arguments);
  });
  installed.addCommand = patch(window.Window_Command && Window_Command.prototype, "addCommand", original => function(name, symbol, enabled, ext) {
    record("Window_Command.addCommand", name, {}, (this.constructor && this.constructor.name || "") + ":" + clean(symbol), fontEvidence(this.contents));
    return original.apply(this, arguments);
  });
  installed.loadFont = patch(window.Graphics, "loadFont", original => function(name, url) {
    record("Graphics.loadFont", url, {}, name, {requestedFontFace:clean(name), requestedFontSize:0, requestedFontLoaded:null, glyphFallback:"unverified"});
    return original.apply(this, arguments);
  });
  installed.fontManagerLoad = patch(window.FontManager, "load", original => function(name, url) {
    record("FontManager.load", url, {}, name, {requestedFontFace:clean(name), requestedFontSize:0, requestedFontLoaded:null, glyphFallback:"unverified"});
    return original.apply(this, arguments);
  });
  installed.graphicsPrintError = patch(window.Graphics, "printError", original => function(name, message) {
    recordRuntimeError("graphics_print_error", clean(name) + ": " + clean(message), "", 0, 0, "");
    return original.apply(this, arguments);
  });
  installed.graphicsPrintLoadingError = patch(window.Graphics, "printLoadingError", original => function(url) {
    recordRuntimeError("graphics_loading_error", "Failed to load " + clean(url), clean(url), 0, 0, "");
    return original.apply(this, arguments);
  });
  const requirements = hookRequirements();
  const ready = Object.keys(requirements).every(name => requirements[name] === true);
  state.installFinished = ready && state.pageLoadFinished && state.pollTicks > 0;
  if (state.installFinished && state.installTimer) {
    clearInterval(state.installTimer);
    state.installTimer = null;
  }
  };
  state.take = () => state.events.splice(0, state.events.length);
  state.takeErrors = () => {
    captureErrorPrinter();
    return state.runtimeErrors.splice(0, state.runtimeErrors.length);
  };
  state.scene = sceneName;
  state.snapshot = snapshot;
  window.__ATT_NW_OBSERVER__ = state;
  installHooks();
  state.installTimer = setInterval(() => {
    state.pollTicks += 1;
    installHooks();
  }, 10);
  const finishInstall = () => {
    state.pageLoadFinished = true;
    installHooks();
  };
  if (document.readyState === "complete") {
    setTimeout(finishInstall, 0);
  } else {
    window.addEventListener("load", () => setTimeout(finishInstall, 0), {once: true});
  }
  return Object.assign({reused: false}, snapshot());
})()
"""


def scenario_expression(name: str) -> str:
    """返回不使用键盘事件的场景切换表达式。"""

    expressions = {
        "title": (
            "(() => { if (!window.SceneManager || !window.Scene_Title) return {supported:false}; "
            "SceneManager.goto(Scene_Title); return {supported:true}; })()"
        ),
        "new_game": (
            "(() => { if (!window.DataManager || !window.SceneManager || !window.Scene_Map) "
            "return {supported:false}; DataManager.setupNewGame(); SceneManager.goto(Scene_Map); "
            "return {supported:true}; })()"
        ),
        "menu": (
            "(() => { if (!window.SceneManager || !window.Scene_Menu) return {supported:false}; "
            "SceneManager.push(Scene_Menu); return {supported:true}; })()"
        ),
        "options": (
            "(() => { if (!window.SceneManager || !window.Scene_Options) return {supported:false}; "
            "SceneManager.push(Scene_Options); return {supported:true}; })()"
        ),
        "save": (
            "(() => { if (!window.SceneManager || !window.Scene_Save) return {supported:false}; "
            "SceneManager.push(Scene_Save); return {supported:true}; })()"
        ),
        "quest_log": r"""(() => {
          if (!window.SceneManager || !window.Scene_Menu || !(SceneManager._scene instanceof Scene_Menu)) {
            return {supported:false, reason:"menu_not_active"};
          }
          const commandWindow = SceneManager._scene._commandWindow;
          if (!commandWindow || !Array.isArray(commandWindow._list)) {
            return {supported:false, reason:"menu_command_list_missing"};
          }
          const candidates = commandWindow._list.map((item, index) => ({item, index})).filter(candidate => {
            const item = candidate.item || {};
            return /(?:quest|journal|mission|task|log)/i.test(String(item.name || "") + " " + String(item.symbol || ""));
          }).filter(candidate => {
            const symbol = candidate.item && candidate.item.symbol;
            return !!symbol && commandWindow._handlers && typeof commandWindow._handlers[symbol] === "function";
          });
          if (candidates.length !== 1) {
            return {supported:false, reason:"menu_handler_not_unique", candidates:candidates.map(candidate => ({name:candidate.item.name, symbol:candidate.item.symbol}))};
          }
          const candidate = candidates[0];
          commandWindow.select(candidate.index);
          commandWindow.callHandler(candidate.item.symbol);
          return {supported:true, commandName:String(candidate.item.name || ""), symbol:String(candidate.item.symbol || "")};
        })()""",
        "dialogue": (
            "({supported: !!(window.Window_Message && window.$gameMessage), "
            "reason: 'observes map-driven dialogue without synthetic text'})"
        ),
    }
    try:
        return expressions[name]
    except KeyError as error:
        raise ValueError(f"未知场景：{name}") from error


def event_sequence(value: object) -> int:
    if not isinstance(value, Mapping):
        return 0
    sequence = cast(Mapping[object, object], value).get("sequence")
    return sequence if isinstance(sequence, int) and not isinstance(sequence, bool) else 0


def summarize_draws(events: Sequence[Mapping[str, object]]) -> dict[str, object]:
    """从真实 draw 调用中汇总英文候选和可证明的像素越界。"""

    english: list[dict[str, object]] = []
    overflow: list[dict[str, object]] = []
    for event in events:
        text = event.get("text")
        if not isinstance(text, str):
            continue
        geometry = event.get("geometry")
        typed_geometry: Mapping[object, object] = (
            cast(Mapping[object, object], geometry) if isinstance(geometry, Mapping) else {}
        )
        if any(
            typed_geometry.get(field) is True
            for field in ("clippingOverflow", "overflowLeft", "overflowRight", "overflowBottom")
        ):
            overflow.append(dict(event))
        if any("A" <= character <= "Z" or "a" <= character <= "z" for character in text):
            english.append(dict(event))
    return {
        "draw_count": len(events),
        "english_candidate_count": len(english),
        "english_candidates": english,
        "pixel_overflow_count": len(overflow),
        "pixel_overflows": overflow,
        "measurement_unverified_count": sum(
            1
            for event in events
            if isinstance(event.get("geometry"), Mapping)
            and str(cast(Mapping[object, object], event["geometry"]).get("measurementStatus", "")).startswith(
                "unverified_"
            )
        ),
    }
