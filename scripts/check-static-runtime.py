"""Reject Windows release entry points importing a dynamic C/C++ runtime."""
import pathlib
import struct
import sys


def imports(data):
    def unpack(fmt, offset):
        return struct.unpack_from('<' + fmt, data, offset)

    if data[:2] != b'MZ':
        raise ValueError('not a PE image')
    pe, = unpack('I', 0x3c)
    if data[pe:pe + 4] != b'PE\0\0':
        raise ValueError('invalid PE signature')
    machine, sections = unpack('HH', pe + 4)
    if machine not in (0x8664, 0xaa64):
        raise ValueError('expected x64 or ARM64 image')
    optional_size, = unpack('H', pe + 20)
    optional = pe + 24
    if unpack('H', optional)[0] != 0x20b or optional_size < 240:
        raise ValueError('expected PE32+ optional header')
    image_base, = unpack('Q', optional + 24)
    directory_count, = unpack('I', optional + 108)
    if directory_count < 14:
        raise ValueError('missing import directory declarations')
    section_table = optional + optional_size

    def offset(rva, size=1):
        for i in range(sections):
            virtual_size, address, raw_size, raw = unpack('IIII', section_table + i * 40 + 8)
            if address <= rva and rva + size <= address + min(virtual_size, raw_size):
                result = raw + rva - address
                if result + size <= len(data):
                    return result
        raise ValueError('import outside backed section')

    def name(rva):
        start = offset(rva)
        for length in range(1, 257):
            current = offset(rva, length)
            if data[current + length - 1] == 0:
                return data[start:start + length - 1].decode('ascii').lower()
        raise ValueError('unterminated DLL name')

    result = []
    for directory, record_size in ((1, 20), (13, 32)):
        rva, size = unpack('II', optional + 112 + directory * 8)
        if not rva and not size:
            continue
        if not rva or size < record_size:
            raise ValueError('invalid import directory')
        terminated = False
        for position in range(0, size - record_size + 1, record_size):
            fields = unpack('I' * (record_size // 4), offset(rva + position, record_size))
            if not any(fields):
                terminated = True
                break
            name_rva = fields[3] if directory == 1 else fields[1]
            if directory == 13 and not fields[0] & 1:
                name_rva -= image_base
            result.append(name(name_rva))
        if not terminated:
            raise ValueError('unterminated import directory')
    if not result:
        raise ValueError('expected system imports')
    return result


def check(data):
    names = imports(data)
    forbidden = [name for name in names if name.startswith(
        ('vcruntime', 'msvcp', 'msvcr', 'ucrtbase', 'api-ms-win-crt-'))]
    if forbidden:
        raise ValueError('dynamic runtime imports: ' + ', '.join(forbidden))
    return names


if __name__ == '__main__':
    if len(sys.argv) < 2:
        raise SystemExit('usage: check-static-runtime.py <exe/com> [...]')
    for arg in sys.argv[1:]:
        path = pathlib.Path(arg)
        print(f'{path.name}: static-runtime check passed; OS imports: {", ".join(check(path.read_bytes()))}')
