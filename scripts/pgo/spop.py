"""Subset of SPOP 2.0 used by this release training workload."""
from io import BytesIO
import socket
import struct


def integer(value):
    if value < 240:
        return bytes([value])
    result = bytearray([240 | (value & 15)])
    value = (value - 240) >> 4
    while value >= 128:
        result.append(128 | (value & 127))
        value = (value - 128) >> 7
    result.append(value)
    return result


def number(stream):
    first = stream.read(1)[0]
    if first < 240:
        return first
    value, shift = first, 4
    while True:
        byte = stream.read(1)[0]
        value += byte << shift
        if byte < 128:
            return value
        shift += 7


def string(data):
    data = data.encode() if isinstance(data, str) else data
    return integer(len(data)) + data


def fields(values):
    payload = bytearray()
    for name, value in values.items():
        payload += string(name)
        if isinstance(value, bool):
            payload += bytes([0x11 if value else 0x01])
        elif isinstance(value, int):
            payload += b'\x03' + integer(value)
        else:
            payload += (b'\x09' if isinstance(value, bytes) else b'\x08') + string(value)
    return payload


def message(name, values):
    return string(name) + bytes([len(values)]) + fields(values)


def frame(kind, stream_id, frame_id, data):
    body = struct.pack('>BI', kind, 1) + integer(stream_id) + integer(frame_id) + data
    return struct.pack('>I', len(body)) + body


def read_exact(sock, size):
    data = bytearray()
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        if not chunk:
            raise EOFError('socket closed within an SPOP frame')
        data.extend(chunk)
    return data


def read_frame(sock):
    size = struct.unpack('>I', read_exact(sock, 4))[0]
    if size > 16384:
        raise ValueError(f'oversized SPOP frame ({size} bytes)')
    return read_exact(sock, size)


class Client:
    def __init__(self, address):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(10)
        try:
            self.sock.connect(str(address))
            self.sock.sendall(frame(1, 0, 0, fields({
                'supported-versions': '2.0', 'max-frame-size': 16384,
                'capabilities': '', 'engine-id': 'pgo-training',
            })))
            if read_frame(self.sock)[0] != 101:
                raise ValueError('hub rejected SPOP handshake')
        except BaseException:
            self.sock.close()
            raise
        self.frame_id = 0

    def notify(self, payload):
        self.frame_id += 1
        request = frame(3, 1, self.frame_id, payload)
        self.sock.sendall(request)
        reply = BytesIO(read_frame(self.sock))
        if (reply.read(5), number(reply), number(reply)) != (b'\x67\0\0\0\x01', 1, self.frame_id):
            raise ValueError('unexpected SPOP acknowledgement')
        variables = {}
        while operation := reply.read(3):
            if len(operation) != 3 or operation[:2] != b'\x01\x03':
                raise ValueError(f'unexpected set-var action or scope {operation!r}')
            name = reply.read(number(reply)).decode()
            kind = reply.read(1)[0]
            if kind in (1, 17):
                value = bool(kind & 16)
            elif kind in (2, 3, 4, 5):
                value = number(reply)
            elif kind == 8:
                value = reply.read(number(reply)).decode()
            else:
                raise ValueError(f'unexpected type {kind} for {name}')
            variables[name] = value
        return variables
