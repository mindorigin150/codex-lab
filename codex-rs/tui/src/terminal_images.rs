//! Kitty graphics protocol helpers for terminal-resident PNG images.

use base64::Engine;
use base64::engine::general_purpose;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

const ESC: &str = "\x1b";
const ST: &str = "\x1b\\";
const KITTY_CHUNK_SIZE: usize = 4096;
// VS Code 1.117's xterm image addon interprets q=1 as "suppress OK" and still acknowledges q=2.
const KITTY_QUIET_SUCCESS: u8 = 1;
static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(0x434f_5000);

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub(crate) struct TerminalImageCapabilities {
    pub(crate) kitty_graphics: bool,
    pub(crate) cell_size_pixels: Option<(u16, u16)>,
}

pub(crate) fn next_image_id() -> u32 {
    NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Pixel rectangle selected from the transmitted PNG before placement.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct SourceCrop {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Encodes an in-memory PNG as a direct Kitty transmission and placement.
///
/// The caller owns image-id allocation: VS Code's Kitty implementation requires a distinct id for
/// every placement. `columns` and `rows` are terminal-cell dimensions. `crop`, when present, is a
/// source-pixel rectangle (`x`, `y`, `w`, `h`) in the PNG.
pub(crate) fn transmit_png(
    png: &[u8],
    columns: u16,
    rows: u16,
    image_id: u32,
    crop: Option<SourceCrop>,
) -> String {
    let payload = general_purpose::STANDARD.encode(png);
    let mut output = String::new();

    for (index, start) in (0..payload.len()).step_by(KITTY_CHUNK_SIZE).enumerate() {
        let end = (start + KITTY_CHUNK_SIZE).min(payload.len());
        let chunk = &payload[start..end];
        let has_more = end < payload.len();
        if index == 0 {
            let crop = crop.map_or_else(String::new, |crop| {
                format!(
                    ",x={},y={},w={},h={}",
                    crop.x, crop.y, crop.width, crop.height
                )
            });
            output.push_str(&format!(
                "{ESC}_Ga=T,t=d,f=100,i={image_id},c={columns},r={rows},C=1,q={KITTY_QUIET_SUCCESS}{crop},m={};{}{}",
                u8::from(has_more),
                chunk,
                ST,
            ));
        } else {
            output.push_str(&format!(
                "{ESC}_Gq={KITTY_QUIET_SUCCESS},m={};{}{}",
                u8::from(has_more),
                chunk,
                ST,
            ));
        }
    }

    output
}

/// Deletes every placement and the backing image data for one Kitty image id.
pub(crate) fn delete_image(image_id: u32) -> String {
    format!("{ESC}_Ga=d,d=I,i={image_id},q={KITTY_QUIET_SUCCESS};{ST}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn commands(encoded: &str) -> Vec<&str> {
        encoded
            .split(ST)
            .filter(|command| !command.is_empty())
            .collect()
    }

    fn payload(command: &str) -> &str {
        command.split_once(';').unwrap().1
    }

    #[test]
    fn transmits_small_png_with_required_placement_fields() {
        let encoded = transmit_png(b"png", 7, 2, 42, None);
        assert_eq!(
            encoded,
            "\x1b_Ga=T,t=d,f=100,i=42,c=7,r=2,C=1,q=1,m=0;cG5n\x1b\\"
        );
    }

    #[test]
    fn chunks_base64_payload_and_only_describes_first_chunk() {
        let png = vec![0xabu8; 7_000];
        let encoded = transmit_png(&png, 80, 12, 99, None);
        let commands = commands(&encoded);

        assert_eq!(commands.len(), 3);
        assert!(commands[0].starts_with("\x1b_Ga=T,t=d,f=100,i=99,c=80,r=12,C=1,q=1,m=1;"));
        assert!(commands[1].starts_with("\x1b_Gq=1,m=1;"));
        assert!(commands[2].starts_with("\x1b_Gq=1,m=0;"));
        assert!(commands.iter().all(|command| command.contains("q=1")));

        let joined = commands
            .iter()
            .map(|command| payload(command))
            .collect::<String>();
        assert_eq!(general_purpose::STANDARD.decode(joined).unwrap(), png);
    }

    #[test]
    fn includes_source_crop_on_first_chunk() {
        let encoded = transmit_png(
            b"png",
            9,
            3,
            7,
            Some(SourceCrop {
                x: 11,
                y: 12,
                width: 130,
                height: 40,
            }),
        );

        assert!(
            encoded
                .starts_with("\x1b_Ga=T,t=d,f=100,i=7,c=9,r=3,C=1,q=1,x=11,y=12,w=130,h=40,m=0;")
        );
    }

    #[test]
    fn deletes_whole_image_by_id() {
        assert_eq!(delete_image(123), "\x1b_Ga=d,d=I,i=123,q=1;\x1b\\");
    }
}
