use glide_platform::BackendError;

const HTML_LIMIT: usize = 32 * 1024 * 1024;
const IMAGE_LIMIT: usize = 96 * 1024 * 1024;
const PIXEL_LIMIT: u64 = 16_000_000;
const HTML_HEADER: &[u8] = b"Version:1.0\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n";
const FRAGMENT_START: &[u8] = b"<!--StartFragment-->";
const FRAGMENT_END: &[u8] = b"<!--EndFragment-->";

fn invalid(message: &'static str) -> BackendError {
    BackendError::InvalidInput(message.into())
}

fn native_failure() -> BackendError {
    BackendError::Failed("Windows Imaging Component operation failed".into())
}

pub(super) fn cf_html_encode(fragment: &[u8]) -> Result<Vec<u8>, BackendError> {
    if fragment.len() > HTML_LIMIT {
        return Err(invalid("HTML clipboard payload exceeds limit"));
    }
    std::str::from_utf8(fragment).map_err(|_| invalid("HTML clipboard payload is not UTF-8"))?;

    let html_start = HTML_HEADER.len();
    let (fragment_start, fragment_end, html_end) = cf_html_layout(fragment.len())?;

    let mut output = Vec::with_capacity(html_end);
    output.extend_from_slice(HTML_HEADER);
    patch_html_offset(&mut output, b"StartHTML:", html_start)?;
    patch_html_offset(&mut output, b"EndHTML:", html_end)?;
    patch_html_offset(&mut output, b"StartFragment:", fragment_start)?;
    patch_html_offset(&mut output, b"EndFragment:", fragment_end)?;
    output.extend_from_slice(b"<html><body>");
    output.extend_from_slice(FRAGMENT_START);
    output.extend_from_slice(fragment);
    output.extend_from_slice(FRAGMENT_END);
    output.extend_from_slice(b"</body></html>");
    Ok(output)
}

fn cf_html_layout(fragment_len: usize) -> Result<(usize, usize, usize), BackendError> {
    let fragment_start = HTML_HEADER
        .len()
        .checked_add(b"<html><body>".len())
        .and_then(|offset| offset.checked_add(FRAGMENT_START.len()))
        .ok_or_else(|| invalid("HTML clipboard payload is too large"))?;
    let fragment_end = fragment_start
        .checked_add(fragment_len)
        .ok_or_else(|| invalid("HTML clipboard payload is too large"))?;
    let html_end = fragment_end
        .checked_add(FRAGMENT_END.len())
        .and_then(|offset| offset.checked_add(b"</body></html>".len()))
        .ok_or_else(|| invalid("HTML clipboard payload is too large"))?;
    if html_end > HTML_LIMIT {
        return Err(invalid("HTML clipboard payload exceeds limit"));
    }
    Ok((fragment_start, fragment_end, html_end))
}

fn patch_html_offset(output: &mut [u8], field: &[u8], offset: usize) -> Result<(), BackendError> {
    let start = output
        .windows(field.len())
        .position(|window| window == field)
        .ok_or_else(|| invalid("invalid CF_HTML header"))?
        + field.len();
    let end = start + 10;
    let value = format!("{offset:010}");
    output[start..end].copy_from_slice(value.as_bytes());
    Ok(())
}

pub(super) fn cf_html_decode(data: &[u8]) -> Result<Vec<u8>, BackendError> {
    if data.len() > HTML_LIMIT {
        return Err(invalid("HTML clipboard payload exceeds limit"));
    }

    let header_end = cf_html_header_end(data);
    let start = html_offset(data, b"StartFragment:");
    let end = html_offset(data, b"EndFragment:");
    if matches!(start, Some(Err(()))) || matches!(end, Some(Err(()))) {
        return Err(invalid("invalid CF_HTML fragment offsets"));
    }
    if let (Some(Ok(Some(start))), Some(Ok(Some(end)))) = (start, end) {
        if start <= end && end <= data.len() && start >= header_end {
            return html_bytes(&data[start..end]);
        }
        return Err(invalid("invalid CF_HTML fragment offsets"));
    }

    if let (Some(start), Some(end)) = (
        find_bytes(data, FRAGMENT_START),
        find_bytes(data, FRAGMENT_END),
    ) {
        let start = start + FRAGMENT_START.len();
        if start <= end {
            return html_bytes(&data[start..end]);
        }
    }

    if header_end != 0 {
        let html_start = html_offset(data, b"StartHTML:");
        let html_end = html_offset(data, b"EndHTML:");
        if matches!(html_start, Some(Err(()))) || matches!(html_end, Some(Err(()))) {
            return Err(invalid("invalid CF_HTML document offsets"));
        }
        if let (Some(Ok(Some(start))), Some(Ok(Some(end)))) = (html_start, html_end) {
            if start <= end && end <= data.len() && start >= header_end {
                return html_bytes(&data[start..end]);
            }
            return Err(invalid("invalid CF_HTML document offsets"));
        }
        return html_bytes(&data[header_end..]);
    }

    html_bytes(data)
}

fn html_bytes(data: &[u8]) -> Result<Vec<u8>, BackendError> {
    std::str::from_utf8(data).map_err(|_| invalid("HTML clipboard payload is not UTF-8"))?;
    Ok(data.to_vec())
}

fn html_offset(data: &[u8], field: &[u8]) -> Option<Result<Option<usize>, ()>> {
    let field_start = data
        .windows(field.len())
        .position(|window| window == field)?
        + field.len();
    let line_end = data[field_start..]
        .iter()
        .position(|byte| *byte == b'\r' || *byte == b'\n')
        .map(|index| field_start + index)
        .unwrap_or(data.len());
    let value = trim_ascii(&data[field_start..line_end]);
    if value == b"-1" {
        return Some(Ok(None));
    }
    Some(parse_decimal(value).map(Some).ok_or(()))
}

fn cf_html_header_end(data: &[u8]) -> usize {
    let mut cursor = 0;
    let mut saw_header = false;
    while cursor < data.len() {
        let line_end = data[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|index| cursor + index)
            .unwrap_or(data.len());
        let line = &data[cursor..line_end];
        if line.is_empty() {
            return (line_end + 2).min(data.len());
        }
        let known_field = [
            b"Version:".as_slice(),
            b"StartHTML:",
            b"EndHTML:",
            b"StartFragment:",
            b"EndFragment:",
            b"StartSelection:",
            b"EndSelection:",
            b"SourceURL:",
        ]
        .iter()
        .any(|field| line.starts_with(field));
        if !known_field {
            break;
        }
        saw_header = true;
        cursor = (line_end + 2).min(data.len());
    }
    if saw_header {
        cursor
    } else {
        0
    }
}

fn parse_decimal(value: &[u8]) -> Option<usize> {
    if value.is_empty() {
        return None;
    }
    value.iter().try_fold(0usize, |result, byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        result
            .checked_mul(10)?
            .checked_add(usize::from(*byte - b'0'))
    })
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(windows)]
mod wic {
    use super::{invalid, native_failure, IMAGE_LIMIT, PIXEL_LIMIT};
    use glide_platform::BackendError;
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows::core::{Interface, HRESULT};
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
        IWICBitmapDecoder, IWICBitmapFrameEncode, IWICImagingFactory, WICBitmapDitherTypeNone,
        WICBitmapEncoderNoCache, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IStream, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
    };

    const BMP_FILE_HEADER: usize = 14;
    const DIBV5_HEADER: u32 = 124;
    const BI_RGB: u32 = 0;
    const BI_RLE8: u32 = 1;
    const BI_RLE4: u32 = 2;
    const BI_BITFIELDS: u32 = 3;
    const BI_JPEG: u32 = 4;
    const BI_PNG: u32 = 5;
    const BI_ALPHABITFIELDS: u32 = 6;
    const LCS_PROFILE_EMBEDDED: u32 = 0x4d42_4544;
    const LCS_PROFILE_LINKED: u32 = 0x4c49_4e4b;
    const LCS_SRGB: u32 = 0x7352_4742;

    struct ComApartment {
        owns_initialization: bool,
    }

    impl ComApartment {
        fn enter() -> Result<Self, BackendError> {
            // SAFETY: CoInitializeEx is called once for the current thread and balanced in Drop
            // only when this call successfully acquired an initialization count.
            let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if result.is_ok() {
                Ok(Self {
                    owns_initialization: true,
                })
            } else if result == RPC_E_CHANGED_MODE {
                Ok(Self {
                    owns_initialization: false,
                })
            } else {
                Err(native_failure())
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.owns_initialization {
                // SAFETY: paired with this instance's successful CoInitializeEx call.
                unsafe { CoUninitialize() };
            }
        }
    }

    fn imaging_factory() -> Result<IWICImagingFactory, BackendError> {
        // SAFETY: WIC is an in-process COM server and the apartment is initialized by callers.
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| native_failure())
    }

    fn checked_dimensions(width: u32, height: u32) -> Result<(usize, usize), BackendError> {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > PIXEL_LIMIT {
            return Err(invalid("image dimensions exceed limit"));
        }
        let stride = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| invalid("image dimensions overflow"))?;
        let byte_len = stride
            .checked_mul(usize::try_from(height).map_err(|_| invalid("image dimensions overflow"))?)
            .filter(|length| *length <= IMAGE_LIMIT)
            .ok_or_else(|| invalid("image dimensions exceed limit"))?;
        Ok((stride, byte_len))
    }

    fn decoded_bgra(
        factory: &IWICImagingFactory,
        decoder: &IWICBitmapDecoder,
    ) -> Result<(u32, u32, Vec<u8>), BackendError> {
        // SAFETY: These WIC calls operate on COM interfaces returned by the factory and validate
        // the frame index before copying into an exactly sized Rust-owned buffer.
        let (width, height, bytes) = unsafe {
            if decoder.GetFrameCount().map_err(|_| native_failure())? == 0 {
                return Err(invalid("image contains no frames"));
            }
            let frame = decoder.GetFrame(0).map_err(|_| native_failure())?;
            let mut width = 0;
            let mut height = 0;
            frame
                .GetSize(&mut width, &mut height)
                .map_err(|_| invalid("invalid image dimensions"))?;
            let (stride, byte_len) = checked_dimensions(width, height)?;
            let converter = factory
                .CreateFormatConverter()
                .map_err(|_| native_failure())?;
            converter
                .Initialize(
                    &frame,
                    &GUID_WICPixelFormat32bppBGRA,
                    WICBitmapDitherTypeNone,
                    None::<&windows::Win32::Graphics::Imaging::IWICPalette>,
                    0.0,
                    WICBitmapPaletteTypeCustom,
                )
                .map_err(|_| invalid("unsupported image pixel data"))?;
            let mut pixels = vec![0; byte_len];
            converter
                .CopyPixels(
                    std::ptr::null(),
                    u32::try_from(stride).map_err(|_| invalid("image stride overflow"))?,
                    &mut pixels,
                )
                .map_err(|_| invalid("unsupported image pixel data"))?;
            (width, height, pixels)
        };
        Ok((width, height, bytes))
    }

    fn decode_stream(
        factory: &IWICImagingFactory,
        stream: &windows::Win32::Graphics::Imaging::IWICStream,
    ) -> Result<(u32, u32, Vec<u8>), BackendError> {
        // SAFETY: The initialized WIC stream remains alive during decoder/frame reads.
        let decoder = unsafe {
            factory
                .CreateDecoderFromStream(stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
                .map_err(|_| invalid("invalid image data"))?
        };
        decoded_bgra(factory, &decoder)
    }

    fn validate_dib(dib: &[u8]) -> Result<(u32, u32, usize), BackendError> {
        if dib.len() < 40 || dib.len() > IMAGE_LIMIT {
            return Err(invalid("invalid DIB payload size"));
        }
        let header_size = read_u32(dib, 0)?;
        if !matches!(header_size, 40 | 52 | 56 | 108 | DIBV5_HEADER)
            || header_size as usize > dib.len()
        {
            return Err(invalid("unsupported DIB header"));
        }
        let width = read_i32(dib, 4)?;
        let signed_height = read_i32(dib, 8)?;
        let planes = read_u16(dib, 12)?;
        let bit_count = read_u16(dib, 14)?;
        let compression = read_u32(dib, 16)?;
        let image_size = read_u32(dib, 20)? as usize;
        let color_count = read_u32(dib, 32)? as usize;
        if width <= 0 || signed_height == 0 || signed_height == i32::MIN || planes != 1 {
            return Err(invalid("invalid DIB dimensions or planes"));
        }
        if !matches!(bit_count, 1 | 4 | 8 | 16 | 24 | 32) {
            return Err(invalid("unsupported DIB pixel format"));
        }
        if !matches!(
            compression,
            BI_RGB | BI_RLE8 | BI_RLE4 | BI_BITFIELDS | BI_JPEG | BI_PNG | BI_ALPHABITFIELDS
        ) {
            return Err(invalid("unsupported DIB compression"));
        }
        if (compression == BI_RLE8 && bit_count != 8)
            || (compression == BI_RLE4 && bit_count != 4)
            || (matches!(compression, BI_RLE8 | BI_RLE4) && signed_height < 0)
            || (matches!(compression, BI_BITFIELDS | BI_ALPHABITFIELDS)
                && !matches!(bit_count, 16 | 32))
        {
            return Err(invalid("invalid DIB compression and pixel format"));
        }

        let width = width as u32;
        let height = signed_height.unsigned_abs();
        checked_dimensions(width, height)?;
        let mut pixel_offset = header_size as usize;
        if header_size == 40 && matches!(compression, BI_BITFIELDS | BI_ALPHABITFIELDS) {
            pixel_offset = pixel_offset
                .checked_add(if compression == BI_ALPHABITFIELDS {
                    16
                } else {
                    12
                })
                .ok_or_else(|| invalid("DIB pixel offset overflow"))?;
        }
        let palette_entries = if color_count != 0 {
            color_count
        } else if bit_count <= 8 {
            1usize << bit_count
        } else {
            0
        };
        if bit_count <= 8 && palette_entries > (1usize << bit_count) {
            return Err(invalid("DIB palette has too many entries"));
        }
        pixel_offset = pixel_offset
            .checked_add(
                palette_entries
                    .checked_mul(4)
                    .ok_or_else(|| invalid("DIB palette size overflow"))?,
            )
            .ok_or_else(|| invalid("DIB pixel offset overflow"))?;

        if header_size == DIBV5_HEADER {
            let color_space = read_u32(dib, 56)?;
            let profile_offset = read_u32(dib, 112)? as usize;
            let profile_size = read_u32(dib, 116)? as usize;
            if color_space == LCS_PROFILE_LINKED {
                return Err(invalid("linked DIB color profiles are not supported"));
            }
            if color_space == LCS_PROFILE_EMBEDDED && profile_size != 0 {
                let profile_end = profile_offset
                    .checked_add(profile_size)
                    .filter(|end| profile_offset >= header_size as usize && *end <= dib.len())
                    .ok_or_else(|| invalid("invalid DIB color profile"))?;
                pixel_offset = pixel_offset.max(profile_end);
            }
        }

        if pixel_offset > dib.len() {
            return Err(invalid("DIB pixel data is truncated"));
        }
        match compression {
            BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS => {
                let row_bits = u64::from(width) * u64::from(bit_count);
                let row_bytes = row_bits.div_ceil(32) * 4;
                let needed = row_bytes
                    .checked_mul(u64::from(height))
                    .and_then(|length| usize::try_from(length).ok())
                    .ok_or_else(|| invalid("DIB pixel data size overflow"))?;
                if needed > dib.len() - pixel_offset
                    || (image_size != 0 && image_size > dib.len() - pixel_offset)
                {
                    return Err(invalid("DIB pixel data is truncated"));
                }
            }
            BI_RLE8 | BI_RLE4 | BI_JPEG | BI_PNG => {
                let needed = if image_size == 0 {
                    dib.len() - pixel_offset
                } else {
                    image_size
                };
                if needed == 0 || needed > dib.len() - pixel_offset {
                    return Err(invalid("DIB compressed data is truncated"));
                }
            }
            _ => return Err(invalid("unsupported DIB compression")),
        }
        let bmp_offset = BMP_FILE_HEADER
            .checked_add(pixel_offset)
            .filter(|offset| *offset <= u32::MAX as usize)
            .ok_or_else(|| invalid("DIB pixel offset overflow"))?;
        Ok((width, height, bmp_offset))
    }

    pub(crate) fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, BackendError> {
        let _apartment = ComApartment::enter()?;
        let (width, height, pixel_offset) = validate_dib(dib)?;
        let bmp_len = BMP_FILE_HEADER
            .checked_add(dib.len())
            .filter(|length| *length <= IMAGE_LIMIT + BMP_FILE_HEADER)
            .ok_or_else(|| invalid("DIB payload exceeds limit"))?;
        let mut bmp = Vec::with_capacity(bmp_len);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(bmp_len as u32).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
        bmp.extend_from_slice(dib);

        let factory = imaging_factory()?;
        // SAFETY: WIC retains the supplied bytes only for this call chain; `bmp` outlives the
        // stream, decoder, and frame that read from the stream.
        let stream = unsafe {
            let stream = factory.CreateStream().map_err(|_| native_failure())?;
            stream
                .InitializeFromMemory(&bmp)
                .map_err(|_| invalid("invalid DIB pixel data"))?;
            stream
        };
        // SAFETY: The input stream references a live, bounded BMP buffer.
        let decoder = unsafe {
            factory
                .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
                .map_err(|_| invalid("invalid DIB image"))?
        };
        let (decoded_width, decoded_height, pixels) = decoded_bgra(&factory, &decoder)?;
        if decoded_width != width || decoded_height != height {
            return Err(invalid("DIB dimensions do not match decoded image"));
        }
        encode_png(&factory, width, height, &pixels)
    }

    pub(crate) fn png_to_dib(png: &[u8]) -> Result<Vec<u8>, BackendError> {
        if png.is_empty() || png.len() > IMAGE_LIMIT {
            return Err(invalid("PNG clipboard payload exceeds limit"));
        }
        let expected_dimensions = png_dimensions(png)?;
        let _apartment = ComApartment::enter()?;
        let factory = imaging_factory()?;
        // SAFETY: The stream keeps the PNG bytes borrowed only while this call chain remains live.
        let stream = unsafe {
            let stream = factory.CreateStream().map_err(|_| native_failure())?;
            stream
                .InitializeFromMemory(png)
                .map_err(|_| invalid("invalid PNG data"))?;
            stream
        };
        let (width, height, pixels) = decode_stream(&factory, &stream)?;
        if (width, height) != expected_dimensions {
            return Err(invalid("PNG dimensions do not match decoded image"));
        }
        let mut dib = dib_v5_header(width, height, pixels.len())?;
        dib.extend_from_slice(&pixels);
        if dib.len() > IMAGE_LIMIT {
            return Err(invalid("decoded image exceeds limit"));
        }
        Ok(dib)
    }

    fn encode_png(
        factory: &IWICImagingFactory,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<Vec<u8>, BackendError> {
        let (stride, expected) = checked_dimensions(width, height)?;
        if pixels.len() != expected {
            return Err(invalid("invalid decoded image buffer"));
        }
        let stream = create_memory_stream()?;
        // SAFETY: Stream and bitmap bytes remain alive through encoder/frame commit.
        unsafe {
            let encoder = factory
                .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())
                .map_err(|_| native_failure())?;
            encoder
                .Initialize(&stream, WICBitmapEncoderNoCache)
                .map_err(|_| native_failure())?;
            let mut frame = None::<IWICBitmapFrameEncode>;
            let mut options = None;
            encoder
                .CreateNewFrame(&mut frame, &mut options)
                .map_err(|_| native_failure())?;
            let frame = frame.ok_or_else(native_failure)?;
            frame
                .Initialize(options.as_ref())
                .map_err(|_| native_failure())?;
            frame.SetSize(width, height).map_err(|_| native_failure())?;
            let mut pixel_format = GUID_WICPixelFormat32bppBGRA;
            frame
                .SetPixelFormat(&mut pixel_format)
                .map_err(|_| native_failure())?;
            if pixel_format != GUID_WICPixelFormat32bppBGRA {
                return Err(BackendError::Unsupported);
            }
            frame
                .WritePixels(
                    height,
                    u32::try_from(stride).map_err(|_| invalid("image stride overflow"))?,
                    pixels,
                )
                .map_err(|_| native_failure())?;
            frame.Commit().map_err(|_| native_failure())?;
            encoder.Commit().map_err(|_| native_failure())?;
        }
        read_stream(&stream)
    }

    fn dib_v5_header(width: u32, height: u32, pixel_bytes: usize) -> Result<Vec<u8>, BackendError> {
        checked_dimensions(width, height)?;
        let image_size =
            u32::try_from(pixel_bytes).map_err(|_| invalid("decoded image exceeds limit"))?;
        let mut header = Vec::with_capacity(DIBV5_HEADER as usize);
        header.extend_from_slice(&DIBV5_HEADER.to_le_bytes());
        header.extend_from_slice(&width.to_le_bytes());
        header.extend_from_slice(&(-(height as i32)).to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&32u16.to_le_bytes());
        header.extend_from_slice(&BI_BITFIELDS.to_le_bytes());
        header.extend_from_slice(&image_size.to_le_bytes());
        header.extend_from_slice(&0i32.to_le_bytes());
        header.extend_from_slice(&0i32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&0x00ff_0000u32.to_le_bytes());
        header.extend_from_slice(&0x0000_ff00u32.to_le_bytes());
        header.extend_from_slice(&0x0000_00ffu32.to_le_bytes());
        header.extend_from_slice(&0xff00_0000u32.to_le_bytes());
        header.extend_from_slice(&LCS_SRGB.to_le_bytes());
        header.extend_from_slice(&[0; 36]);
        header.extend_from_slice(&[0; 12]);
        header.extend_from_slice(&4u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        if header.len() != DIBV5_HEADER as usize {
            return Err(native_failure());
        }
        Ok(header)
    }

    fn png_dimensions(png: &[u8]) -> Result<(u32, u32), BackendError> {
        if png.len() < 24
            || png.get(..8) != Some(&b"\x89PNG\r\n\x1a\n"[..])
            || png.get(8..12) != Some(&13u32.to_be_bytes()[..])
            || png.get(12..16) != Some(&b"IHDR"[..])
        {
            return Err(invalid("invalid PNG header"));
        }
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        checked_dimensions(width, height)?;
        Ok((width, height))
    }

    fn create_memory_stream() -> Result<IStream, BackendError> {
        let mut raw_stream = null_mut::<c_void>();
        // SAFETY: Ole32 returns an owned IStream pointer on success; TRUE makes the stream
        // responsible for releasing its backing HGLOBAL when its final COM reference is dropped.
        let result = unsafe { CreateStreamOnHGlobal(null_mut(), 1, &mut raw_stream) };
        if result.is_err() || raw_stream.is_null() {
            return Err(native_failure());
        }
        // SAFETY: `raw_stream` is a non-null IStream pointer transferred by Ole32 above.
        Ok(unsafe { IStream::from_raw(raw_stream) })
    }

    fn read_stream(stream: &IStream) -> Result<Vec<u8>, BackendError> {
        let mut stat = STATSTG::default();
        // SAFETY: Stat fills this initialized output structure.
        unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }.map_err(|_| native_failure())?;
        let length = usize::try_from(stat.cbSize).map_err(|_| native_failure())?;
        if length == 0 || length > IMAGE_LIMIT {
            return Err(invalid("converted image exceeds limit"));
        }
        // SAFETY: Reset the owned seekable memory stream before reading its measured size.
        unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }.map_err(|_| native_failure())?;
        let mut bytes = vec![0; length];
        let mut read = 0u32;
        // SAFETY: The destination buffer is writable for `length` bytes, and its size was checked
        // to fit in u32 before passing it to IStream::Read.
        let result = unsafe {
            stream.Read(
                bytes.as_mut_ptr().cast(),
                u32::try_from(length).map_err(|_| native_failure())?,
                Some(&mut read),
            )
        };
        if result.is_err() || read as usize != length {
            return Err(native_failure());
        }
        Ok(bytes)
    }

    fn read_u16(data: &[u8], offset: usize) -> Result<u16, BackendError> {
        let bytes = data
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("truncated DIB header"))?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32(data: &[u8], offset: usize) -> Result<u32, BackendError> {
        let bytes = data
            .get(offset..offset + 4)
            .ok_or_else(|| invalid("truncated DIB header"))?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_i32(data: &[u8], offset: usize) -> Result<i32, BackendError> {
        Ok(read_u32(data, offset)? as i32)
    }

    // SAFETY: This declaration matches the ole32 COM API; `create_memory_stream` validates and
    // wraps the returned owned IStream pointer before exposing it to Rust callers.
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CreateStreamOnHGlobal(
            hglobal: *mut c_void,
            delete_on_release: i32,
            stream: *mut *mut c_void,
        ) -> HRESULT;
    }

    #[cfg(test)]
    pub(super) fn make_test_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let _apartment = ComApartment::enter().unwrap_or_else(|_| panic!("COM unavailable"));
        let factory = imaging_factory().unwrap_or_else(|_| panic!("WIC unavailable"));
        encode_png(&factory, width, height, pixels).unwrap_or_else(|_| panic!("PNG encode failed"))
    }

    #[cfg(test)]
    pub(super) fn dimensions(dib: &[u8]) -> Option<(u32, u32)> {
        validate_dib(dib)
            .ok()
            .map(|(width, height, _)| (width, height))
    }
}

#[cfg(windows)]
pub(crate) use wic::{dib_to_png, png_to_dib};

#[cfg(not(windows))]
pub(super) fn dib_to_png(_dib: &[u8]) -> Result<Vec<u8>, BackendError> {
    Err(BackendError::Unsupported)
}

#[cfg(not(windows))]
pub(super) fn png_to_dib(_png: &[u8]) -> Result<Vec<u8>, BackendError> {
    Err(BackendError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::{cf_html_decode, cf_html_encode, FRAGMENT_END, FRAGMENT_START};

    #[test]
    fn cf_html_offsets_count_utf8_bytes_and_round_trip() {
        let fragment = b"<b>snowman \xe2\x98\x83 and emoji \xf0\x9f\xa6\x80</b>";
        let encoded = cf_html_encode(fragment).expect("valid HTML should encode");

        let start = offset(&encoded, b"StartFragment:");
        let end = offset(&encoded, b"EndFragment:");
        assert_eq!(&encoded[start..end], fragment);

        let marker_data = [
            b"Version:1.0\r\nStartFragment:-1\r\nEndFragment:-1\r\n".as_slice(),
            FRAGMENT_START,
            b"<i>fragment</i>",
            FRAGMENT_END,
        ]
        .concat();
        for (data, expected) in [
            (encoded.as_slice(), fragment.as_slice()),
            (marker_data.as_slice(), b"<i>fragment</i>".as_slice()),
        ] {
            assert_eq!(cf_html_decode(data).unwrap(), expected);
        }
    }

    #[test]
    fn cf_html_rejects_out_of_bounds_offsets() {
        let mut data = cf_html_encode(b"fragment").unwrap();
        replace_offset(&mut data, b"EndFragment:", 9999);
        assert!(cf_html_decode(&data).is_err());
    }

    #[test]
    fn cf_html_total_size_limit_includes_header_and_wrappers() {
        let fixed_size = super::HTML_HEADER.len()
            + b"<html><body>".len()
            + FRAGMENT_START.len()
            + FRAGMENT_END.len()
            + b"</body></html>".len();
        let largest_fragment = super::HTML_LIMIT - fixed_size;
        let (_, _, html_end) = super::cf_html_layout(largest_fragment).unwrap();
        assert_eq!(html_end, super::HTML_LIMIT);
        assert!(super::cf_html_layout(largest_fragment + 1).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn wic_png_and_dib_v5_round_trip() {
        let pixels = [
            10, 20, 30, 0, 40, 50, 60, 64, 70, 80, 90, 128, 100, 110, 120, 255,
        ];
        let png = super::wic::make_test_png(2, 2, &pixels);
        let dib = super::png_to_dib(&png).unwrap();
        assert_eq!(&dib[..4], &124u32.to_le_bytes());
        assert_eq!(super::wic::dimensions(&dib), Some((2, 2)));
        let round_trip = super::dib_to_png(&dib).unwrap();
        let decoded = super::png_to_dib(&round_trip).unwrap();
        assert_eq!(&decoded[124..], &pixels);
    }

    #[cfg(windows)]
    #[test]
    fn wic_rejects_linked_profiles_and_oversized_palettes() {
        let pixels = [11, 22, 33, 255];
        let png = super::wic::make_test_png(1, 1, &pixels);
        let mut dib = super::png_to_dib(&png).unwrap();

        dib[56..60].copy_from_slice(&0x4c49_4e4bu32.to_le_bytes());
        assert!(super::wic::dimensions(&dib).is_none());

        dib[56..60].copy_from_slice(&0x7352_4742u32.to_le_bytes());
        dib[14..16].copy_from_slice(&8u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes());
        dib[32..36].copy_from_slice(&257u32.to_le_bytes());
        assert!(super::wic::dimensions(&dib).is_none());
    }

    fn offset(data: &[u8], field: &[u8]) -> usize {
        let start = data
            .windows(field.len())
            .position(|window| window == field)
            .unwrap()
            + field.len();
        let end = data[start..]
            .iter()
            .position(|byte| *byte == b'\r')
            .unwrap()
            + start;
        std::str::from_utf8(&data[start..end])
            .unwrap()
            .parse()
            .unwrap()
    }

    fn replace_offset(data: &mut [u8], field: &[u8], value: usize) {
        let start = data
            .windows(field.len())
            .position(|window| window == field)
            .unwrap()
            + field.len();
        data[start..start + 10].copy_from_slice(format!("{value:010}").as_bytes());
    }
}
