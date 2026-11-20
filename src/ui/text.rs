use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    ID2D1RenderTarget, ID2D1SolidColorBrush, D2D1_DRAW_TEXT_OPTIONS_CLIP,
};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteFactory, IDWriteTextFormat, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT, DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};

pub(crate) fn encode_utf16_into<'a>(s: &str, buf: &'a mut Vec<u16>) -> &'a [u16] {
    buf.clear();
    buf.extend(s.encode_utf16());
    buf.as_slice()
}

/// Creates a single-line DirectWrite `IDWriteTextFormat` using the system-default
/// empty locale `""` (enabling proper CJK font fallback), optionally attaching
/// character-level ellipsis trimming so long strings never overflow into adjacent widgets.
pub(crate) unsafe fn create_text_format(
    factory: &IDWriteFactory,
    font_name: PCWSTR,
    weight: DWRITE_FONT_WEIGHT,
    size: f32,
    align: DWRITE_TEXT_ALIGNMENT,
    ellipsis_trimming: bool,
) -> windows::core::Result<IDWriteTextFormat> {
    let locale = HSTRING::new();
    let fmt = factory.CreateTextFormat(
        font_name,
        None,
        weight,
        DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_STRETCH_NORMAL,
        size,
        &locale,
    )?;
    fmt.SetTextAlignment(align)?;
    fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;

    if ellipsis_trimming {
        let sign = factory.CreateEllipsisTrimmingSign(&fmt)?;
        let trimming = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        };
        fmt.SetTrimming(&trimming, &sign)?;
    }

    Ok(fmt)
}

/// Draws text clipped strictly to `rect` (`D2D1_DRAW_TEXT_OPTIONS_CLIP`), reusing `utf16_buf`
/// and the caller's single solid brush.
pub(crate) unsafe fn draw_text_clipped(
    rt: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    utf16_buf: &mut Vec<u16>,
    text: &str,
    fmt: &IDWriteTextFormat,
    color: &D2D1_COLOR_F,
    rect: D2D_RECT_F,
) {
    brush.SetColor(color);
    let slice = encode_utf16_into(text, utf16_buf);
    rt.DrawText(
        slice,
        fmt,
        &rect,
        brush,
        D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_WEIGHT_SEMI_BOLD,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    };

    #[test]
    fn text_format_uses_default_locale_and_supports_ellipsis_trimming() {
        unsafe {
            let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).unwrap();
            let fmt = create_text_format(
                &factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                12.0,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )
            .unwrap();
            let mut buf = [0u16; 16];
            fmt.GetLocaleName(&mut buf).unwrap();
            assert_eq!(buf[0], 0);
        }
    }
}
