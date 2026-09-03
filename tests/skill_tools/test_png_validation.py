from __future__ import annotations

import binascii
import struct
import sys
import unittest
import zlib
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "skills" / "translate-with-att" / "scripts"))

from att_toolbox.png import decode_png_size

_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def _chunk(kind: bytes, payload: bytes) -> bytes:
    checksum = binascii.crc32(payload, binascii.crc32(kind)) & 0xFFFFFFFF
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", checksum)


def _png(
    color_type: int,
    chunks: tuple[bytes, ...],
    *,
    width: int = 1,
    height: int = 1,
    bit_depth: int = 8,
) -> bytes:
    ihdr = struct.pack(">IIBBBBB", width, height, bit_depth, color_type, 0, 0, 0)
    return _SIGNATURE + _chunk(b"IHDR", ihdr) + b"".join(chunks) + _chunk(b"IEND", b"")


def _pack_palette_indices(indices: tuple[int, ...], bit_depth: int) -> bytes:
    packed = bytearray((len(indices) * bit_depth + 7) // 8)
    pixels_per_byte = 8 // bit_depth
    for pixel_index, palette_index in enumerate(indices):
        shift = 8 - bit_depth * (pixel_index % pixels_per_byte + 1)
        packed[pixel_index // pixels_per_byte] |= palette_index << shift
    return bytes(packed)


class PngValidationTests(unittest.TestCase):
    def test_chunk_type_uses_ascii_letters_and_accepts_future_reserved_bit(self) -> None:
        image = _chunk(b"IDAT", zlib.compress(b"\x00\x00"))
        with self.assertRaises(ValueError):
            decode_png_size(_png(0, (_chunk(b"1bCd", b""), image)))
        self.assertEqual(decode_png_size(_png(0, (_chunk(b"abcd", b""), image))), (1, 1))
        with self.assertRaises(ValueError):
            decode_png_size(_png(0, (_chunk(b"ABcD", b""), image)))

    def test_indexed_png_requires_one_palette_before_image_data(self) -> None:
        palette = _chunk(b"PLTE", b"\x00\x00\x00")
        image = _chunk(b"IDAT", zlib.compress(b"\x00\x00"))

        self.assertEqual(decode_png_size(_png(3, (palette, image))), (1, 1))
        invalid_cases = (
            ("missing", (image,)),
            ("late", (image, palette)),
            ("duplicate", (palette, palette, image)),
        )
        for name, chunks in invalid_cases:
            with self.subTest(name=name), self.assertRaises(ValueError):
                decode_png_size(_png(3, chunks))

    def test_plte_dependent_chunks_follow_palette_and_precede_image_data(self) -> None:
        palette = _chunk(b"PLTE", b"\x00\x00\x00")
        image = _chunk(b"IDAT", zlib.compress(b"\x00\x00"))
        dependent_chunks = (
            ("transparency", _chunk(b"tRNS", b"\xff")),
            ("background", _chunk(b"bKGD", b"\x00")),
            ("histogram", _chunk(b"hIST", b"\x00\x01")),
        )

        for name, dependent in dependent_chunks:
            with self.subTest(name=name, order="valid"):
                self.assertEqual(decode_png_size(_png(3, (palette, dependent, image))), (1, 1))
            for order, chunks in (
                ("before_palette", (dependent, palette, image)),
                ("after_image", (palette, image, dependent)),
            ):
                with self.subTest(name=name, order=order), self.assertRaises(ValueError):
                    decode_png_size(_png(3, chunks))

    def test_truecolor_transparency_can_omit_palette_but_cannot_precede_one(self) -> None:
        palette = _chunk(b"PLTE", b"\x00\x00\x00")
        transparency = _chunk(b"tRNS", b"\x00" * 6)
        image = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00\x00"))

        self.assertEqual(decode_png_size(_png(2, (transparency, image))), (1, 1))
        self.assertEqual(decode_png_size(_png(2, (palette, transparency, image))), (1, 1))
        with self.assertRaises(ValueError):
            decode_png_size(_png(2, (transparency, palette, image)))

    def test_transparency_background_and_histogram_payload_contracts(self) -> None:
        grayscale_image = _chunk(b"IDAT", zlib.compress(b"\x00\x00"))
        truecolor_image = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00\x00"))
        grayscale_alpha_image = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00"))
        truecolor_alpha_image = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00\x00\x00"))
        palette = _chunk(b"PLTE", bytes(6))
        indexed_image = _chunk(b"IDAT", zlib.compress(b"\x00\x00"))

        invalid = (
            ("grayscale-trns-length", _png(0, (_chunk(b"tRNS", b"\x00"), grayscale_image))),
            ("truecolor-trns-length", _png(2, (_chunk(b"tRNS", b"\x00\x00"), truecolor_image))),
            (
                "indexed-trns-length",
                _png(3, (palette, _chunk(b"tRNS", b"\xff\xff\xff"), indexed_image)),
            ),
            (
                "grayscale-alpha-trns",
                _png(4, (_chunk(b"tRNS", b"\x00\x00"), grayscale_alpha_image)),
            ),
            (
                "truecolor-alpha-trns",
                _png(6, (_chunk(b"tRNS", bytes(6)), truecolor_alpha_image)),
            ),
            (
                "duplicate-trns",
                _png(
                    0,
                    (
                        _chunk(b"tRNS", b"\x00\x00"),
                        _chunk(b"tRNS", b"\x00\x00"),
                        grayscale_image,
                    ),
                ),
            ),
            ("grayscale-bkgd-length", _png(0, (_chunk(b"bKGD", b"\x00"), grayscale_image))),
            ("truecolor-bkgd-length", _png(2, (_chunk(b"bKGD", b"\x00\x00"), truecolor_image))),
            (
                "indexed-bkgd-index",
                _png(3, (palette, _chunk(b"bKGD", b"\x02"), indexed_image)),
            ),
            (
                "duplicate-bkgd",
                _png(
                    0,
                    (
                        _chunk(b"bKGD", b"\x00\x00"),
                        _chunk(b"bKGD", b"\x00\x00"),
                        grayscale_image,
                    ),
                ),
            ),
            (
                "hist-length",
                _png(3, (palette, _chunk(b"hIST", b"\x00\x01"), indexed_image)),
            ),
            (
                "duplicate-hist",
                _png(
                    3,
                    (
                        palette,
                        _chunk(b"hIST", bytes(4)),
                        _chunk(b"hIST", bytes(4)),
                        indexed_image,
                    ),
                ),
            ),
        )
        for name, image in invalid:
            with self.subTest(name=name), self.assertRaises(ValueError):
                decode_png_size(image)

        legal_high_bits = (
            _png(0, (_chunk(b"tRNS", b"\xff\x01"), grayscale_image), bit_depth=1),
            _png(0, (_chunk(b"bKGD", b"\xff\x01"), grayscale_image), bit_depth=1),
            _png(3, (palette, _chunk(b"tRNS", b""), indexed_image)),
        )
        for image in legal_high_bits:
            self.assertEqual(decode_png_size(image), (1, 1))

    def test_indexed_pixels_reference_existing_palette_entries_at_every_bit_depth(self) -> None:
        cases = (
            (1, 1, (0, 0, 0), 1),
            (2, 3, (0, 2, 1), 3),
            (4, 3, (0, 2, 1), 3),
            (8, 3, (0, 2, 1), 3),
        )
        for bit_depth, palette_entries, valid_indices, invalid_index in cases:
            palette = _chunk(b"PLTE", bytes(palette_entries * 3))
            valid_scanline = b"\x00" + _pack_palette_indices(valid_indices, bit_depth)
            invalid_indices = (*valid_indices[:-1], invalid_index)
            invalid_scanline = b"\x00" + _pack_palette_indices(invalid_indices, bit_depth)

            with self.subTest(bit_depth=bit_depth, result="valid"):
                image = _chunk(b"IDAT", zlib.compress(valid_scanline))
                self.assertEqual(
                    decode_png_size(_png(3, (palette, image), width=len(valid_indices), bit_depth=bit_depth)),
                    (len(valid_indices), 1),
                )
            with self.subTest(bit_depth=bit_depth, result="out_of_range"), self.assertRaises(ValueError):
                image = _chunk(b"IDAT", zlib.compress(invalid_scanline))
                decode_png_size(_png(3, (palette, image), width=len(invalid_indices), bit_depth=bit_depth))

    def test_indexed_palette_validation_uses_reconstructed_scanlines(self) -> None:
        palette = _chunk(b"PLTE", b"\x00\x00\x00\xff\xff\xff")
        valid_image = _chunk(b"IDAT", zlib.compress(b"\x00\x01\x02\x00"))
        invalid_image = _chunk(b"IDAT", zlib.compress(b"\x00\x01\x02\x01"))

        self.assertEqual(decode_png_size(_png(3, (palette, valid_image), height=2)), (1, 2))
        with self.assertRaises(ValueError):
            decode_png_size(_png(3, (palette, invalid_image), height=2))

    def test_non_indexed_scanlines_validate_filters_without_pixel_reconstruction(self) -> None:
        width = 4096
        image = _chunk(b"IDAT", zlib.compress(b"\x04" + bytes(width * 3)))
        with patch(
            "att_toolbox.png._reconstruct_scanline",
            side_effect=AssertionError("非 indexed 路径不应逐字节重建"),
        ):
            self.assertEqual(decode_png_size(_png(2, (image,), width=width)), (width, 1))

        invalid_filter = _chunk(b"IDAT", zlib.compress(b"\x05" + bytes(width * 3)))
        with self.assertRaisesRegex(ValueError, "filter"):
            decode_png_size(_png(2, (invalid_filter,), width=width))

    def test_decompression_is_bounded_to_one_scanline_and_accepts_split_idat(self) -> None:
        width = 4096
        height = 32
        row_size = width * 3 + 1
        compressed = zlib.compress((b"\x00" + bytes(width * 3)) * height)
        split = len(compressed) // 2
        real_decoder = zlib.decompressobj()
        requested_limits: list[int] = []
        output_sizes: list[int] = []

        class TrackingDecoder:
            @property
            def eof(self) -> bool:
                return real_decoder.eof

            @property
            def unconsumed_tail(self) -> bytes:
                return real_decoder.unconsumed_tail

            @property
            def unused_data(self) -> bytes:
                return real_decoder.unused_data

            def decompress(self, value: bytes, max_length: int) -> bytes:
                requested_limits.append(max_length)
                output = real_decoder.decompress(value, max_length)
                output_sizes.append(len(output))
                return output

        tracker = TrackingDecoder()
        with patch("att_toolbox.png.zlib.decompressobj", return_value=tracker):
            self.assertEqual(
                decode_png_size(
                    _png(
                        2,
                        (
                            _chunk(b"IDAT", compressed[:split]),
                            _chunk(b"IDAT", compressed[split:]),
                        ),
                        width=width,
                        height=height,
                    )
                ),
                (width, height),
            )

        self.assertTrue(requested_limits)
        self.assertLessEqual(max(requested_limits), row_size)
        self.assertLessEqual(max(output_sizes), row_size)

    def test_streaming_decompression_rejects_truncated_and_extra_scanlines(self) -> None:
        valid = zlib.compress(b"\x00\x00")
        cases = (
            valid[:-1],
            zlib.compress(b"\x00\x00\x00\x00"),
        )
        for compressed in cases:
            with self.subTest(size=len(compressed)), self.assertRaises(ValueError):
                decode_png_size(_png(0, (_chunk(b"IDAT", compressed),)))

        self.assertEqual(
            decode_png_size(_png(0, (_chunk(b"IDAT", valid + b"trailing"),))),
            (1, 1),
        )

    def test_declared_large_nonindexed_row_is_not_allocated_before_data_exists(self) -> None:
        image = _chunk(b"IDAT", zlib.compress(b""))

        with self.assertRaises(ValueError):
            decode_png_size(_png(2, (image,), width=1_000_000_000, height=1))

    def test_idat_metadata_is_rescanned_without_retaining_one_object_per_chunk(self) -> None:
        compressed = zlib.compress(b"\x00\x00")
        chunks = tuple(_chunk(b"IDAT", part) for part in (b"", compressed, b""))

        self.assertEqual(decode_png_size(_png(0, chunks)), (1, 1))

    def test_first_pass_does_not_copy_large_chunk_payloads(self) -> None:
        width = 4096
        image = _png(
            2,
            (_chunk(b"IDAT", zlib.compress(b"\x00" + bytes(width * 3))),),
            width=width,
        )
        real_crc32 = binascii.crc32
        checked_large_payload = False

        def checked_crc32(value: bytes | bytearray | memoryview, checksum: int = 0) -> int:
            nonlocal checked_large_payload
            if len(value) > 32:
                self.assertIsInstance(value, memoryview)
                checked_large_payload = True
            return real_crc32(value, checksum)

        with patch("att_toolbox.png.binascii.crc32", side_effect=checked_crc32):
            self.assertEqual(decode_png_size(image), (width, 1))
        self.assertTrue(checked_large_payload)

    def test_idat_is_fed_to_zlib_in_bounded_slices(self) -> None:
        real_decompressobj = zlib.decompressobj
        feed_sizes: list[int] = []

        class TrackingDecoder:
            def __init__(self) -> None:
                self.inner = real_decompressobj()

            @property
            def eof(self) -> bool:
                return self.inner.eof

            @property
            def unconsumed_tail(self) -> bytes:
                return self.inner.unconsumed_tail

            def decompress(self, data: bytes | memoryview, max_length: int) -> bytes:
                feed_sizes.append(len(data))
                return self.inner.decompress(data, max_length)

        compressed = zlib.compress(b"\x00\x00") + bytes(200_000)
        image = _png(0, (_chunk(b"IDAT", compressed),))
        with patch("att_toolbox.png.zlib.decompressobj", TrackingDecoder):
            self.assertEqual(decode_png_size(image), (1, 1))

        self.assertTrue(feed_sizes)
        self.assertLessEqual(max(feed_sizes), 64 * 1024)

    def test_palette_is_forbidden_for_grayscale_types(self) -> None:
        palette = _chunk(b"PLTE", b"\x00\x00\x00")
        grayscale = _chunk(b"IDAT", zlib.compress(b"\x00\x00"))
        grayscale_alpha = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00"))

        for color_type, image in ((0, grayscale), (4, grayscale_alpha)):
            with self.subTest(color_type=color_type), self.assertRaises(ValueError):
                decode_png_size(_png(color_type, (palette, image)))

    def test_palette_remains_optional_for_truecolor_types(self) -> None:
        palette = _chunk(b"PLTE", b"\x00\x00\x00")
        truecolor = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00\x00"))
        truecolor_alpha = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00\x00\x00"))

        for color_type, image in ((2, truecolor), (6, truecolor_alpha)):
            with self.subTest(color_type=color_type):
                self.assertEqual(decode_png_size(_png(color_type, (image,))), (1, 1))
                self.assertEqual(decode_png_size(_png(color_type, (palette, image))), (1, 1))

    def test_dimensions_must_follow_format_and_platform_limits_before_decompression(self) -> None:
        image = _chunk(b"IDAT", zlib.compress(b""))

        with self.assertRaisesRegex(ValueError, "格式上限"):
            decode_png_size(_png(6, (image,), width=0xFFFFFFFF, height=1, bit_depth=16))

        with (
            patch("att_toolbox.png.sys.maxsize", 1),
            self.assertRaisesRegex(ValueError, "当前平台可表示范围"),
        ):
            decode_png_size(_png(6, (image,), width=1, height=1, bit_depth=16))


if __name__ == "__main__":
    unittest.main()
