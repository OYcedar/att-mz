"""无需外部图像库的严格、有限 PNG 截图解码检查。"""

from __future__ import annotations

import binascii
import struct
import sys
import zlib
from collections.abc import Iterator

_SIGNATURE = b"\x89PNG\r\n\x1a\n"
_BITS_PER_PIXEL = {
    (0, 1): 1,
    (0, 2): 2,
    (0, 4): 4,
    (0, 8): 8,
    (0, 16): 16,
    (2, 8): 24,
    (2, 16): 48,
    (3, 1): 1,
    (3, 2): 2,
    (3, 4): 4,
    (3, 8): 8,
    (4, 8): 16,
    (4, 16): 32,
    (6, 8): 32,
    (6, 16): 64,
}
_PLTE_ORDERED_CHUNKS = {b"bKGD", b"tRNS"}
_COMPRESSED_FEED_SIZE = 64 * 1024


def _paeth_predictor(left: int, above: int, upper_left: int) -> int:
    estimate = left + above - upper_left
    left_distance = abs(estimate - left)
    above_distance = abs(estimate - above)
    upper_left_distance = abs(estimate - upper_left)
    if left_distance <= above_distance and left_distance <= upper_left_distance:
        return left
    if above_distance <= upper_left_distance:
        return above
    return upper_left


def _reconstruct_scanline(
    filtered: bytes,
    previous: bytes | bytearray | None,
    filter_type: int,
    filter_bytes_per_pixel: int,
) -> bytearray:
    reconstructed = bytearray(len(filtered))
    for index, value in enumerate(filtered):
        left = reconstructed[index - filter_bytes_per_pixel] if index >= filter_bytes_per_pixel else 0
        above = previous[index] if previous is not None else 0
        upper_left = (
            previous[index - filter_bytes_per_pixel]
            if previous is not None and index >= filter_bytes_per_pixel
            else 0
        )
        if filter_type == 0:
            predictor = 0
        elif filter_type == 1:
            predictor = left
        elif filter_type == 2:
            predictor = above
        elif filter_type == 3:
            predictor = (left + above) // 2
        elif filter_type == 4:
            predictor = _paeth_predictor(left, above, upper_left)
        else:
            raise ValueError("PNG scanline filter 无效")
        reconstructed[index] = (value + predictor) & 0xFF
    return reconstructed


def _validate_palette_indices(
    scanline: bytes | bytearray,
    width: int,
    bit_depth: int,
    palette_entries: int,
) -> None:
    pixels_per_byte = 8 // bit_depth
    mask = (1 << bit_depth) - 1
    for pixel_index in range(width):
        packed = scanline[pixel_index // pixels_per_byte]
        shift = 8 - bit_depth * (pixel_index % pixels_per_byte + 1)
        if (packed >> shift) & mask >= palette_entries:
            raise ValueError("PNG indexed 像素引用了不存在的 PLTE 条目")


def _idat_payload_ranges(data: bytes) -> Iterator[tuple[int, int]]:
    """在结构校验完成后以常量额外内存重新遍历连续 IDAT payload。"""

    offset = len(_SIGNATURE)
    while offset < len(data):
        length = struct.unpack_from(">I", data, offset)[0]
        chunk_type = data[offset + 4 : offset + 8]
        payload_start = offset + 8
        payload_end = payload_start + length
        if chunk_type == b"IDAT":
            yield payload_start, payload_end
        offset = payload_end + 4
        if chunk_type == b"IEND":
            return


def decode_png_size(data: bytes) -> tuple[int, int]:
    """校验 PNG chunk、CRC、zlib 和非交错 scanline，并返回非零像素尺寸。"""

    if not data.startswith(_SIGNATURE):
        raise ValueError("缺少 PNG signature")
    data_view = memoryview(data)
    offset = len(_SIGNATURE)
    width: int | None = None
    height: int | None = None
    bits_per_pixel: int | None = None
    color_type: int | None = None
    bit_depth: int | None = None
    palette_entries: int | None = None
    saw_plte = False
    plte_must_not_follow = False
    saw_trns = False
    saw_bkgd = False
    saw_hist = False
    saw_idat = False
    idat_finished = False
    saw_iend = False
    while offset < len(data):
        if offset + 12 > len(data):
            raise ValueError("PNG chunk 不完整")
        length = struct.unpack_from(">I", data, offset)[0]
        chunk_type = data[offset + 4 : offset + 8]
        if any(not (65 <= character <= 90 or 97 <= character <= 122) for character in chunk_type):
            raise ValueError("PNG chunk type 必须由四个 ASCII 字母组成")
        chunk_end = offset + 12 + length
        if chunk_end > len(data):
            raise ValueError("PNG chunk 长度越界")
        payload = data_view[offset + 8 : offset + 8 + length]
        expected_crc = struct.unpack_from(">I", data, offset + 8 + length)[0]
        actual_crc = binascii.crc32(chunk_type)
        actual_crc = binascii.crc32(payload, actual_crc) & 0xFFFFFFFF
        if expected_crc != actual_crc:
            raise ValueError("PNG chunk CRC 无效")
        if width is None and chunk_type != b"IHDR":
            raise ValueError("PNG 首个 chunk 不是 IHDR")
        if chunk_type == b"IHDR":
            if width is not None or length != 13:
                raise ValueError("PNG IHDR 无效或重复")
            (
                decoded_width,
                decoded_height,
                decoded_bit_depth,
                decoded_color_type,
                compression,
                filter_method,
                interlace,
            ) = struct.unpack(">IIBBBBB", payload)
            if decoded_width <= 0 or decoded_height <= 0:
                raise ValueError("PNG 像素尺寸为空")
            if decoded_width > 0x7FFFFFFF or decoded_height > 0x7FFFFFFF:
                raise ValueError("PNG 像素尺寸超过格式上限")
            width = decoded_width
            height = decoded_height
            color_type = decoded_color_type
            bit_depth = decoded_bit_depth
            bits_per_pixel = _BITS_PER_PIXEL.get((decoded_color_type, decoded_bit_depth))
            if bits_per_pixel is None or compression != 0 or filter_method != 0 or interlace != 0:
                raise ValueError("PNG 像素格式不受截图解码器支持")
        elif chunk_type == b"PLTE":
            if saw_plte:
                raise ValueError("PNG PLTE 重复")
            if saw_idat:
                raise ValueError("PNG PLTE 位于 IDAT 之后")
            if plte_must_not_follow:
                raise ValueError("PNG PLTE 位于依赖块之后")
            if color_type in {0, 4}:
                raise ValueError("PNG 当前色型禁止 PLTE")
            if length == 0 or length % 3 != 0 or length > 768:
                raise ValueError("PNG PLTE 长度无效")
            if color_type == 3 and bit_depth is not None and length // 3 > 2**bit_depth:
                raise ValueError("PNG PLTE 条目超过 indexed 色深范围")
            palette_entries = length // 3
            saw_plte = True
        elif chunk_type == b"IDAT":
            if idat_finished:
                raise ValueError("PNG IDAT 不连续")
            saw_idat = True
        else:
            if saw_idat:
                idat_finished = True
            if chunk_type == b"IEND":
                if length != 0:
                    raise ValueError("PNG IEND 非空")
                saw_iend = True
                offset = chunk_end
                break
            if chunk_type in _PLTE_ORDERED_CHUNKS:
                if saw_idat:
                    raise ValueError("PNG PLTE 依赖块位于 IDAT 之后")
                if color_type == 3 and not saw_plte:
                    raise ValueError("PNG indexed 的 PLTE 依赖块位于 PLTE 之前")
                if not saw_plte:
                    plte_must_not_follow = True
                if chunk_type == b"tRNS":
                    if saw_trns:
                        raise ValueError("PNG tRNS 重复")
                    saw_trns = True
                    if color_type == 0:
                        valid_length = length == 2
                    elif color_type == 2:
                        valid_length = length == 6
                    elif color_type == 3:
                        valid_length = palette_entries is not None and length <= palette_entries
                    else:
                        raise ValueError("PNG 当前色型禁止 tRNS")
                    if not valid_length:
                        raise ValueError("PNG tRNS 长度无效")
                else:
                    if saw_bkgd:
                        raise ValueError("PNG bKGD 重复")
                    saw_bkgd = True
                    expected_length = 1 if color_type == 3 else 2 if color_type in {0, 4} else 6
                    if length != expected_length:
                        raise ValueError("PNG bKGD 长度无效")
                    if color_type == 3 and palette_entries is not None and payload[0] >= palette_entries:
                        raise ValueError("PNG bKGD 引用了不存在的 PLTE 条目")
            elif chunk_type == b"hIST":
                if saw_idat:
                    raise ValueError("PNG hIST 位于 IDAT 之后")
                if saw_hist:
                    raise ValueError("PNG hIST 重复")
                saw_hist = True
                if palette_entries is None:
                    raise ValueError("PNG hIST 位于 PLTE 之前")
                if length != palette_entries * 2:
                    raise ValueError("PNG hIST 长度与 PLTE 条目数不一致")
            if chunk_type and 65 <= chunk_type[0] <= 90:
                raise ValueError("PNG 含未知 critical chunk")
        offset = chunk_end
    if not saw_iend or offset != len(data) or not saw_idat or width is None or height is None:
        raise ValueError("PNG 缺少完整图像数据或结束标记")
    if color_type == 3 and not saw_plte:
        raise ValueError("PNG indexed 色型缺少 PLTE")
    assert bits_per_pixel is not None
    row_bytes = (width * bits_per_pixel + 7) // 8
    row_size = row_bytes + 1
    if row_size > sys.maxsize:
        raise ValueError("PNG 单行 scanline 超过当前平台可表示范围")

    row = bytearray()
    row_count = 0
    filter_bytes_per_pixel = max(1, (bits_per_pixel + 7) // 8)
    previous: bytes | bytearray | None = None

    def consume(output: bytes) -> None:
        nonlocal previous, row_count
        if not output:
            return
        if row_count >= height:
            raise ValueError("PNG 解码后的 scanline 长度无效")
        row.extend(output)
        if len(row) != row_size:
            return
        filter_type = row[0]
        if filter_type > 4:
            raise ValueError("PNG scanline filter 无效")
        if color_type == 3:
            reconstructed = _reconstruct_scanline(
                bytes(row[1:]),
                previous,
                filter_type,
                filter_bytes_per_pixel,
            )
            assert bit_depth is not None and palette_entries is not None
            _validate_palette_indices(reconstructed, width, bit_depth, palette_entries)
            previous = reconstructed
        row.clear()
        row_count += 1

    def feed(compressed: bytes | memoryview) -> None:
        if decoder.eof:
            return
        pending = compressed
        while True:
            output_limit = 1 if row_count >= height else row_size - len(row)
            output = decoder.decompress(pending, output_limit)
            pending = decoder.unconsumed_tail
            consume(output)
            if decoder.eof:
                return
            if pending:
                continue
            if len(output) == output_limit:
                pending = b""
                continue
            break

    try:
        decoder = zlib.decompressobj()
        for start, end in _idat_payload_ranges(data):
            while start < end and not decoder.eof:
                next_start = min(start + _COMPRESSED_FEED_SIZE, end)
                feed(data_view[start:next_start])
                start = next_start
        feed(b"")
    except (MemoryError, OverflowError, zlib.error) as error:
        raise ValueError("PNG IDAT 无法解压") from error
    if not decoder.eof or row or row_count != height:
        raise ValueError("PNG 解码后的 scanline 长度无效")
    return width, height
