//! Inline images, as iTerm2's `imgcat` and many tools send them: an OSC
//! 1337 `File=` sequence with arguments (`inline=1`, `width`, `height`,
//! `preserveAspectRatio`) and the file in base64. Tern shows the image at
//! the cursor over a block of cells that moves with the text.

use base64::Engine as _;

/// How big an image should be along one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// As big as the image itself.
    Auto,
    Cells(u32),
    Pixels(u32),
    /// A share of the terminal.
    Percent(u32),
}

/// What an OSC 1337 `File=` sequence asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inline {
    /// Shown in the terminal; otherwise it's a download, which Tern
    /// doesn't take.
    pub inline: bool,
    pub width: Size,
    pub height: Size,
    pub keep_ratio: bool,
    /// The file: PNG, JPEG, GIF and the like.
    pub data: Vec<u8>,
}

/// Reads what followed `File=`: `name=value;…:base64`.
pub fn parse(content: &[u8]) -> Result<Inline, String> {
    let colon = content
        .iter()
        .position(|&byte| byte == b':')
        .ok_or("An image without data")?;
    let args = std::str::from_utf8(&content[..colon]).map_err(|_| "Its arguments aren't text")?;
    let mut inline = Inline {
        inline: false,
        width: Size::Auto,
        height: Size::Auto,
        keep_ratio: true,
        data: Vec::new(),
    };
    for arg in args.split(';') {
        let Some((name, value)) = arg.split_once('=') else {
            continue;
        };
        match name {
            "inline" => inline.inline = value == "1",
            "width" => inline.width = size(value),
            "height" => inline.height = size(value),
            "preserveAspectRatio" => inline.keep_ratio = value != "0",
            _ => {}
        }
    }
    let encoded: Vec<u8> = content[colon + 1..]
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    inline.data = base64::engine::general_purpose::STANDARD
        .decode(&encoded)
        .map_err(|_| "Its data isn't base64")?;
    Ok(inline)
}

fn size(value: &str) -> Size {
    let number = |text: &str| text.parse().ok();
    if value == "auto" {
        Size::Auto
    } else if let Some(pixels) = value.strip_suffix("px").and_then(number) {
        Size::Pixels(pixels)
    } else if let Some(percent) = value.strip_suffix('%').and_then(number) {
        Size::Percent(percent)
    } else {
        number(value).map_or(Size::Auto, Size::Cells)
    }
}

/// The cells, as columns and rows, an image of `pixels` covers when asked
/// for as `inline` says, with cells of `cell` pixels in a terminal of
/// `terminal` cells: never wider than the terminal, at least one cell.
pub fn cells(
    inline: &Inline,
    pixels: (u32, u32),
    cell: (f32, f32),
    terminal: (u16, u16),
) -> (u16, u16) {
    let (image_w, image_h) = (pixels.0.max(1) as f32, pixels.1.max(1) as f32);
    let (cell_w, cell_h) = (cell.0.max(1.), cell.1.max(1.));
    let along = |size: Size, cell: f32, whole: u16| -> Option<f32> {
        match size {
            Size::Auto => None,
            Size::Cells(cells) => Some(cells as f32 * cell),
            Size::Pixels(pixels) => Some(pixels as f32),
            Size::Percent(percent) => Some(whole as f32 * cell * percent as f32 / 100.),
        }
    };
    let asked_w = along(inline.width, cell_w, terminal.0);
    let asked_h = along(inline.height, cell_h, terminal.1);
    let (mut w, mut h) = match (asked_w, asked_h) {
        (None, None) => (image_w, image_h),
        (Some(w), None) => (w, w * image_h / image_w),
        (None, Some(h)) => (h * image_w / image_h, h),
        (Some(w), Some(h)) if inline.keep_ratio => {
            // The largest the image fits within both.
            let scale = (w / image_w).min(h / image_h);
            (image_w * scale, image_h * scale)
        }
        (Some(w), Some(h)) => (w, h),
    };
    // Too wide for the terminal: shrunk to fit, keeping its shape.
    let room = terminal.0.max(1) as f32 * cell_w;
    if w > room {
        h *= room / w;
        w = room;
    }
    let columns = (w / cell_w).ceil().clamp(1., terminal.0.max(1) as f32);
    let rows = (h / cell_h).ceil().clamp(1., 255.);
    (columns as u16, rows as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_imgcat_sends() {
        let inline = parse(b"name=Y2F0LnBuZw==;size=3;inline=1;width=20;height=50%:QUJD").unwrap();
        assert!(inline.inline && inline.keep_ratio);
        assert_eq!(
            (inline.width, inline.height),
            (Size::Cells(20), Size::Percent(50))
        );
        assert_eq!(inline.data, b"ABC");
        let download = parse(b"name=eA==;width=100px;preserveAspectRatio=0:QU\nJD").unwrap();
        assert!(!download.inline && !download.keep_ratio);
        assert_eq!(download.width, Size::Pixels(100));
        // Base64 broken over lines still reads.
        assert_eq!(download.data, b"ABC");
        assert!(parse(b"inline=1").is_err());
        assert!(parse(b"inline=1:!!!").is_err());
    }

    #[test]
    fn covers_as_many_cells_as_asked() {
        let at = |width, height| Inline {
            inline: true,
            width,
            height,
            keep_ratio: true,
            data: Vec::new(),
        };
        // 10 by 20 pixel cells, an 80 by 24 terminal.
        let cells = |inline: &Inline, pixels| cells(inline, pixels, (10., 20.), (80, 24));
        // Its own size: 200 by 100 pixels is 20 columns, 5 rows.
        assert_eq!(cells(&at(Size::Auto, Size::Auto), (200, 100)), (20, 5));
        // A width, the height following.
        assert_eq!(
            cells(&at(Size::Cells(40), Size::Auto), (200, 100)),
            (40, 10)
        );
        // Both, keeping its shape: the smaller fit.
        assert_eq!(
            cells(&at(Size::Cells(40), Size::Cells(2)), (200, 100)),
            (8, 2)
        );
        let stretched = Inline {
            keep_ratio: false,
            ..at(Size::Cells(40), Size::Cells(2))
        };
        assert_eq!(cells(&stretched, (200, 100)), (40, 2));
        // Wider than the terminal: shrunk to it, 800 by 40 pixels.
        assert_eq!(cells(&at(Size::Auto, Size::Auto), (2000, 100)), (80, 2));
        assert_eq!(
            cells(&at(Size::Percent(50), Size::Auto), (100, 100)),
            (40, 20)
        );
    }
}
