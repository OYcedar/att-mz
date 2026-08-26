"""无需外部图像库的严格、有限 PNG 截图解码检查。"""

from __future__ import annotations

import binascii
import struct
import zlib

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


def decode_png_size(data: bytes) -> tuple[int, int]:
    """校验 PNG chunk、CRC、zlib 和非交错 scanline，并返回非零像素尺寸。"""

    if not data.startswith(_SIGNATURE):
        raise ValueError("缺少 PNG signature")
    offset = len(_SIGNATURE)
    width: int | None = None
    height: int | None = None
    bits_per_pixel: int | None = None
    compressed = bytearray()
    saw_idat = False
    idat_finished = False
    saw_iend = False
    while offset < len(data):
        if offset + 12 > len(data):
            raise ValueError("PNG chunk 不完整")
        length = struct.unpack_from(">I", data, offset)[0]
        chunk_type = data[offset + 4 : offset + 8]
        chunk_end = offset + 12 + length
        if chunk_end > len(data):
            raise ValueError("PNG chunk 长度越界")
        payload = data[offset + 8 : offset + 8 + length]
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
            decoded_width, decoded_height, bit_depth, color_type, compression, filter_method, interlace = struct.unpack(
                ">IIBBBBB", payload
            )
            if decoded_width <= 0 or decoded_height <= 0:
                raise ValueError("PNG 像素尺寸为空")
            width = decoded_width
            height = decoded_height
            bits_per_pixel = _BITS_PER_PIXEL.get((color_type, bit_depth))
            if bits_per_pixel is None or compression != 0 or filter_method != 0 or interlace != 0:
                raise ValueError("PNG 像素格式不受截图解码器支持")
        elif chunk_type == b"IDAT":
            if idat_finished:
                raise ValueError("PNG IDAT 不连续")
            saw_idat = True
            compressed.extend(payload)
        else:
            if saw_idat:
                idat_finished = True
            if chunk_type == b"IEND":
                if length != 0:
                    raise ValueError("PNG IEND 非空")
                saw_iend = True
                offset = chunk_end
                break
            if chunk_type and 65 <= chunk_type[0] <= 90 and chunk_type not in {b"PLTE"}:
                raise ValueError("PNG 含未知 critical chunk")
        offset = chunk_end
    if not saw_iend or offset != len(data) or not saw_idat or width is None or height is None:
        raise ValueError("PNG 缺少完整图像数据或结束标记")
    assert bits_per_pixel is not None
    row_bytes = (width * bits_per_pixel + 7) // 8
    expected_size = height * (row_bytes + 1)
    try:
        decoder = zlib.decompressobj()
        pixels = decoder.decompress(bytes(compressed), expected_size + 1)
    except zlib.error as error:
        raise ValueError("PNG IDAT 无法解压") from error
    if (
        not decoder.eof
        or decoder.unconsumed_tail
        or decoder.unused_data
        or len(pixels) != expected_size
    ):
        raise ValueError("PNG 解码后的 scanline 长度无效")
    row_size = row_bytes + 1
    if any(pixels[offset] > 4 for offset in range(0, len(pixels), row_size)):
        raise ValueError("PNG scanline filter 无效")
    return width, height
