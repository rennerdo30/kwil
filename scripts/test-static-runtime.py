"""Tiny non-executable PE fixtures, including an independent delay-load witness."""
import importlib.util
import pathlib
import struct
import unittest

spec = importlib.util.spec_from_file_location('gate', pathlib.Path(__file__).with_name('check-static-runtime.py'))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def fixture(dll, delay=False):
    data = bytearray(1024)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 0x3c, 0x80)
    data[0x80:0x84] = b'PE\0\0'
    struct.pack_into('<HH', data, 0x84, 0x8664, 1)
    struct.pack_into('<H', data, 0x94, 240)
    optional = 0x98
    struct.pack_into('<H', data, optional, 0x20b)
    struct.pack_into('<I', data, optional + 108, 16)
    size = 32 if delay else 20
    struct.pack_into('<II', data, optional + 112 + (13 if delay else 1) * 8, 0x1000, size * 2)
    struct.pack_into('<IIII', data, optional + 240 + 8, 512, 0x1000, 512, 512)
    if delay:
        struct.pack_into('<II', data, 512, 1, 0x1100)
    else:
        struct.pack_into('<I', data, 512 + 12, 0x1100)
    encoded = dll.encode('ascii') + b'\0'
    data[768:768 + len(encoded)] = encoded
    return data


class StaticRuntime(unittest.TestCase):
    def test_system_import_remains_allowed(self):
        self.assertEqual(gate.check(fixture('KERNEL32.dll')), ['kernel32.dll'])

    def test_runtime_import_rejected_in_both_tables(self):
        for delay in (False, True):
            for dll in ('VCRUNTIME140.dll', 'ucrtbase.dll', 'msvcp140.dll', 'api-ms-win-crt-runtime-l1-1-0.dll'):
                with self.subTest(delay=delay, dll=dll), self.assertRaisesRegex(ValueError, 'dynamic runtime'):
                    gate.check(fixture(dll, delay))

    def test_truncated_image_cannot_pass(self):
        with self.assertRaises((ValueError, struct.error)):
            gate.check(fixture('KERNEL32.dll')[:800 - 50])


if __name__ == '__main__':
    unittest.main()
