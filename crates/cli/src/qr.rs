//! Terminal QR code for the dashboard URL.
//!
//! Pointing a phone camera at the terminal is the shortest possible path from
//! "I installed this on my laptop" to "the dashboard is open on my phone" —
//! no typing an IP address on a touch keyboard.

use anyhow::{Context, Result};
use qrcode::types::Color;
use qrcode::QrCode;

/// Render `data` as a QR code using half-block characters.
///
/// Two module rows are packed into one character cell, so the code comes out
/// roughly square in a terminal whose cells are about twice as tall as they
/// are wide. Drawing one row per line would produce a code twice as tall as
/// it is wide, which many camera apps fail to lock onto.
pub fn render(data: &str) -> Result<String> {
    let code = QrCode::new(data.as_bytes()).context("the URL is too long to encode")?;
    let width = code.width();
    let modules = code.to_colors();

    let dark = |x: usize, y: usize| -> bool {
        // Outside the symbol is quiet zone, which is light.
        if x >= width || y >= width {
            return false;
        }
        modules[y * width + x] == Color::Dark
    };

    // A quiet zone of four modules is required by the spec; scanners are
    // markedly less reliable without it.
    const QUIET: usize = 4;
    let span = width + QUIET * 2;

    let mut out = String::new();
    for row in (0..span).step_by(2) {
        for column in 0..span {
            let x = column.wrapping_sub(QUIET);
            let top = dark(x, row.wrapping_sub(QUIET));
            let bottom = dark(x, (row + 1).wrapping_sub(QUIET));

            // The terminal background stands in for light modules, so a dark
            // module is drawn as an inked half-block.
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_renders_to_block_characters() {
        let rendered = render("http://192.168.1.10:9000").expect("should encode");

        assert!(rendered.contains('█'), "should contain inked modules");
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() > 8, "should be more than a few rows");

        // Every row is the same width, so the code is not skewed.
        let width = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == width));

        // Half-block packing keeps it close to square rather than twice as
        // tall as it is wide.
        let ratio = width as f64 / lines.len() as f64;
        assert!((1.5..2.5).contains(&ratio), "aspect ratio was {ratio}");
    }

    #[test]
    fn the_quiet_zone_is_blank() {
        let rendered = render("http://10.0.0.5:9000").unwrap();
        let lines: Vec<&str> = rendered.lines().collect();

        // Four light modules pack into two blank character rows.
        assert!(
            lines[0].trim().is_empty(),
            "the top quiet zone should be blank"
        );
        assert!(lines[1].trim().is_empty());
        assert!(lines.last().unwrap().trim().is_empty());

        // And four blank columns on each side.
        for line in &lines {
            let chars: Vec<char> = line.chars().collect();
            assert!(chars[..4].iter().all(|c| *c == ' '), "left quiet zone");
            assert!(
                chars[chars.len() - 4..].iter().all(|c| *c == ' '),
                "right quiet zone"
            );
        }
    }

    #[test]
    fn an_oversized_payload_reports_rather_than_panics() {
        // Beyond the largest QR version's capacity.
        assert!(render(&"x".repeat(8000)).is_err());
    }
}
