//! 图片：按 Claude 看图的上限缩放（Lanczos3，对文字友好），再压小体积。图片每次请求
//! 都要随上下文重新上传，体积直接影响速度，但像素不能丢：
//! - 截图类（PNG 等无损来源）：重新编码成 WebP 无损，和原图（缩放过的和 PNG）比，
//!   取最小的，像素逐位不变；
//! - 照片（JPEG）：本来就是压过的，不用缩放就保留原文件，再压只会更大或更糊；要缩放
//!   的存质量 80 的 JPEG——缩小后看不出和 90 的差别，体积小三成，飞书压过的截图原件
//!   往往只有 75～80 的质量，按 90 重压反而比原图还大。
//!
//! 解码有尺寸和内存上限，挡住「小文件、超大画布」的图片。
//!
//! 上限出自 Claude 的视觉文档：一次请求超过 20 张图时每张的长边不能超过 2000 像素
//! （之前轮次的图也算在内），一张图最多折算 4784 个视觉 token（28 像素一格），超出的
//! 服务端会再缩一次。

use std::io::Cursor;

use fast_image_resize::Resizer;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::{
    DynamicImage, ExtendedColorType, ImageEncoder as _, ImageFormat, ImageReader, Limits, Rgb,
    RgbImage,
};

/// 长边上限。按这个处理，就不用管一次请求里会累积多少张图。
pub const MAX_EDGE: u32 = 2000;
/// 一张图最多折算的视觉 token，以及每个 token 对应的边长（像素）。
const MAX_VISUAL_TOKENS: u32 = 4784;
const PATCH: u32 = 28;
/// 单张图的字节上限：API 按 base64 之后 5 MB 算，留出编码膨胀的余量。
const MAX_BYTES: usize = 3_750_000;
const MAX_CANVAS: u32 = 16_384;
const MAX_ALLOC: u64 = 256 * 1024 * 1024;
/// 照片、以及无损放不下时依次尝试的 JPEG 质量。
const JPEG_QUALITIES: [u8; 3] = [80, 72, 65];

/// 常见图片格式的魔数。
pub fn is_image(bytes: &[u8]) -> bool {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    let bmp = bytes.len() > 18
        && bytes.starts_with(b"BM")
        && matches!(
            u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]),
            12 | 40 | 52 | 56 | 108 | 124
        );
    bytes.starts_with(PNG)
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
        || bmp
}

/// 缩放、压缩。返回要保存的内容和扩展名。
///
/// 不管要不要缩放都先完整解码一遍：坏图在这里挡掉，不会被带进会话，让之后每一轮
/// 请求都因为这张图报错。
pub fn normalize(bytes: Vec<u8>) -> Result<(Vec<u8>, &'static str), String> {
    let mut reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|err| format!("读取图片失败：{err}"))?;
    let format = reader
        .format()
        .ok_or_else(|| "不认识的图片格式".to_owned())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_CANVAS);
    limits.max_image_height = Some(MAX_CANVAS);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|err| format!("图片解码失败：{err}"))?;
    let (width, height) = (image.width(), image.height());
    let target = fit(width, height);
    let resized = target != (width, height);
    let image = if resized {
        resize(&image, target)?
    } else {
        image
    };
    if format == ImageFormat::Jpeg {
        if !resized && bytes.len() <= MAX_BYTES {
            return Ok((bytes, "jpg"));
        }
        return jpeg(&image);
    }
    // 无损的候选：WebP 无损总要试；没缩放时原图本身就是一个（深色终端截图 PNG 往往
    // 比 WebP 还小），缩放过的再试一次 PNG
    let mut best = (webp_lossless(&image)?, "webp");
    let original = match format {
        ImageFormat::Png => Some("png"),
        ImageFormat::Gif => Some("gif"),
        ImageFormat::WebP => Some("webp"),
        _ => None,
    };
    if resized {
        let png = png(&image)?;
        if png.len() < best.0.len() {
            best = (png, "png");
        }
    } else if let Some(ext) = original
        && bytes.len() < best.0.len()
    {
        best = (bytes, ext);
    }
    if best.0.len() <= MAX_BYTES {
        return Ok(best);
    }
    // 噪点很多的大图无损放不下，只能有损
    jpeg(&image)
}

/// SIMD 实现的 Lanczos3（默认算法；带透明通道的按预乘处理，边缘不发黑）。
/// 一张 2560×1440 的截图约 30 毫秒，image 自带的要 0.4 秒。
fn resize(image: &DynamicImage, (width, height): (u32, u32)) -> Result<DynamicImage, String> {
    let mut resized = DynamicImage::new(width, height, image.color());
    Resizer::new()
        .resize(image, &mut resized, None)
        .map_err(|err| format!("图片缩放失败：{err}"))?;
    Ok(resized)
}

/// 缩放后的尺寸：长边不超过 `MAX_EDGE`、视觉 token 不超过 `MAX_VISUAL_TOKENS`，
/// 保持宽高比。本来就在范围内的原样返回。
fn fit(width: u32, height: u32) -> (u32, u32) {
    if width.max(height) <= MAX_EDGE && tokens(width, height) <= MAX_VISUAL_TOKENS {
        return (width, height);
    }
    let area = f64::from(width) * f64::from(height);
    let mut scale = (f64::from(MAX_EDGE) / f64::from(width.max(height)))
        .min(f64::from(PATCH) * (f64::from(MAX_VISUAL_TOKENS) / area).sqrt());
    loop {
        let size = (scaled(width, scale), scaled(height, scale));
        // 按格向上取整，估算的比例可能差一点，每次再缩 1%
        if size.0.max(size.1) <= MAX_EDGE && tokens(size.0, size.1) <= MAX_VISUAL_TOKENS {
            return size;
        }
        scale *= 0.99;
    }
}

fn tokens(width: u32, height: u32) -> u32 {
    width.div_ceil(PATCH) * height.div_ceil(PATCH)
}

/// 缩放后的边长，落在 [1, 原边长] 内（浮点转整数是饱和的）。
fn scaled(edge: u32, scale: f64) -> u32 {
    ((f64::from(edge) * scale).floor() as u32).clamp(1, edge)
}

/// WebP 无损。编码器只收 8 位的灰度、RGB、RGBA，其余先转换。
fn webp_lossless(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let encoder = WebPEncoder::new_lossless(&mut out);
    let result = match image {
        DynamicImage::ImageRgb8(rgb) => encoder.encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        ),
        DynamicImage::ImageRgba8(rgba) => encoder.encode(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        ),
        other if other.color().has_alpha() => {
            let rgba = other.to_rgba8();
            encoder.encode(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                ExtendedColorType::Rgba8,
            )
        }
        other => {
            let rgb = other.to_rgb8();
            encoder.encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                ExtendedColorType::Rgb8,
            )
        }
    };
    result.map_err(|err| format!("图片编码失败：{err}"))?;
    Ok(out)
}

/// PNG：默认压缩级别加逐行自适应滤波。最高级别只小 1%，却要慢两三倍。
fn png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let image = if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    };
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Default, FilterType::Adaptive)
        .write_image(
            image.as_bytes(),
            image.width(),
            image.height(),
            image.color().into(),
        )
        .map_err(|err| format!("图片编码失败：{err}"))?;
    Ok(out)
}

/// JPEG，质量从高往低试到放得下为止。
fn jpeg(image: &DynamicImage) -> Result<(Vec<u8>, &'static str), String> {
    let rgb = flatten(image);
    let mut out = Vec::new();
    for quality in JPEG_QUALITIES {
        out.clear();
        JpegEncoder::new_with_quality(&mut out, quality)
            .encode_image(&rgb)
            .map_err(|err| format!("图片编码失败：{err}"))?;
        if out.len() <= MAX_BYTES {
            break;
        }
    }
    Ok((out, "jpg"))
}

/// JPEG 没有透明通道：透明的地方铺白底，直接丢掉 alpha 会变成黑底，黑字就看不见了。
fn flatten(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend = |c: u8| {
            let value = (u32::from(c) * u32::from(a) + 255 * (255 - u32::from(a))) / 255;
            u8::try_from(value).unwrap_or(u8::MAX)
        };
        Rgb([blend(r), blend(g), blend(b)])
    })
}

#[cfg(test)]
mod tests {
    use image::{GenericImageView as _, Rgba, RgbaImage};

    use super::*;

    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }));
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), format)
            .expect("编码");
        out
    }

    fn dimensions(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes).expect("能解码").dimensions()
    }

    fn pixels(bytes: &[u8]) -> Vec<u8> {
        image::load_from_memory(bytes)
            .expect("能解码")
            .to_rgb8()
            .into_raw()
    }

    #[test]
    fn large_screenshots_are_scaled_down_keeping_the_aspect_ratio() {
        let bytes = encoded(2400, 600, ImageFormat::Png);
        assert!(is_image(&bytes));
        let (out, ext) = normalize(bytes).expect("处理");
        assert!(
            matches!(ext, "webp" | "png"),
            "截图缩放后仍是无损格式：{ext}"
        );
        assert_eq!(dimensions(&out), (2000, 500));
    }

    #[test]
    fn screenshots_are_recompressed_losslessly_and_never_grow() {
        let bytes = encoded(1280, 720, ImageFormat::Png);
        let (out, ext) = normalize(bytes.clone()).expect("处理");
        assert!(matches!(ext, "webp" | "png"), "{ext}");
        assert!(out.len() <= bytes.len(), "{} > {}", out.len(), bytes.len());
        assert_eq!(pixels(&out), pixels(&bytes), "像素要逐位一致");
    }

    #[test]
    fn photos_within_the_limits_keep_their_original_file() {
        let bytes = encoded(1920, 1080, ImageFormat::Jpeg);
        let (out, ext) = normalize(bytes.clone()).expect("处理");
        assert_eq!(ext, "jpg");
        assert_eq!(out, bytes, "JPEG 再压只会更大或更糊");
    }

    #[test]
    fn photos_that_need_scaling_stay_jpeg() {
        let (out, ext) = normalize(encoded(2400, 1200, ImageFormat::Jpeg)).expect("处理");
        assert_eq!(ext, "jpg");
        assert_eq!(dimensions(&out), (2000, 1000));
    }

    #[test]
    fn fit_respects_both_limits_for_any_shape() {
        for (w, h) in [
            (1, 1),
            (2000, 2000),
            (2001, 10),
            (1170, 2532),
            (3440, 1440),
            (16_000, 200),
            (4032, 3024),
        ] {
            let (tw, th) = fit(w, h);
            assert!(tw.max(th) <= MAX_EDGE, "{w}x{h} -> {tw}x{th}");
            assert!(tokens(tw, th) <= MAX_VISUAL_TOKENS, "{w}x{h} -> {tw}x{th}");
            assert!(tw >= 1 && th >= 1 && tw <= w && th <= h);
        }
        assert_eq!(fit(1170, 2532), (924, 2000));
        // 接近正方形的大图由视觉 token 封顶，但不该缩得太狠
        let (side, _) = fit(2000, 2000);
        assert!(
            side > 1900 && tokens(side, side) <= MAX_VISUAL_TOKENS,
            "{side}"
        );
    }

    #[test]
    fn transparent_areas_turn_white_when_falling_back_to_jpeg() {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0])));
        assert_eq!(flatten(&image).get_pixel(0, 0).0, [255, 255, 255]);
    }

    #[test]
    fn bitmaps_are_converted_losslessly_since_the_model_cannot_read_them() {
        let bytes = encoded(64, 32, ImageFormat::Bmp);
        let (out, ext) = normalize(bytes.clone()).expect("处理");
        assert_eq!(ext, "webp");
        assert_eq!(pixels(&out), pixels(&bytes));
    }

    #[test]
    fn text_that_starts_like_a_bitmap_is_not_an_image() {
        assert!(!is_image(b"BMW 3 series maintenance log ........"));
        assert!(normalize(b"\x89PNG\r\n\x1a\nbroken".to_vec()).is_err());
    }
}
