//! Uncompressed AVI files for exercising the Media Foundation path in tests.
//!
//! No machine this is developed on has a camera that Media Foundation opens,
//! and the failures users hit live in the source reader's format negotiation.
//! That negotiation is the same for a file as for a camera: the reader asks the
//! source for its native types and builds a pipeline to the requested output.
//! A file whose only native type is RGB24 or YUY2 reproduces the failure
//! exactly, on any machine.
//!
//! The picture is four coloured quadrants, so orientation and channel order can
//! be checked by reading pixels back:
//!
//! ```text
//! +--------+--------+
//! |  red   | green  |
//! +--------+--------+
//! |  blue  | white  |
//! +--------+--------+
//! ```

use std::path::{Path, PathBuf};

use crate::capture::Size;

/// Test-only layouts. RGB24 is written bottom-up, as every Windows bitmap is.
///
/// MJPEG is the one that matters most: it is what nearly every USB webcam
/// sends at 720p and above, and the only one here that needs a decoder.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Layout {
    Rgb24,
    Yuy2,
    Mjpeg,
}

/// BGR of each quadrant, top-left first, in reading order.
pub(crate) const QUADRANTS: [[u8; 3]; 4] = [
    [0, 0, 255],     // vermelho
    [0, 255, 0],     // verde
    [255, 0, 0],     // azul
    [255, 255, 255], // branco
];

fn quadrant(size: Size, x: u32, y: u32) -> usize {
    let right = usize::from(x >= size.width / 2);
    let bottom = usize::from(y >= size.height / 2);
    bottom * 2 + right
}

/// BT.601 limited range, the same matrix libyuv uses for `ARGBToNV12`.
fn yuv([b, g, r]: [u8; 3]) -> (u8, u8, u8) {
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let y = 16.0 + 0.257 * r + 0.504 * g + 0.098 * b;
    let u = 128.0 - 0.148 * r - 0.291 * g + 0.439 * b;
    let v = 128.0 + 0.439 * r - 0.368 * g - 0.071 * b;
    (y.round() as u8, u.round() as u8, v.round() as u8)
}

/// The luma each quadrant should come out with, top-left first.
pub(crate) fn expected_luma() -> [u8; 4] {
    QUADRANTS.map(|bgr| yuv(bgr).0)
}

fn frame(layout: Layout, size: Size) -> Vec<u8> {
    let (w, h) = (size.width, size.height);
    match layout {
        Layout::Rgb24 => {
            let stride = (w as usize * 3).div_ceil(4) * 4;
            let mut data = vec![0u8; stride * h as usize];
            for y in 0..h {
                // De baixo para cima: a primeira linha do arquivo é a última
                // da imagem.
                let row = (h - 1 - y) as usize * stride;
                for x in 0..w {
                    let at = row + x as usize * 3;
                    data[at..at + 3].copy_from_slice(&QUADRANTS[quadrant(size, x, y)]);
                }
            }
            data
        }
        Layout::Mjpeg => {
            // JPEG é de cima para baixo, sempre.
            let mut bgr = Vec::with_capacity((w * h * 3) as usize);
            for y in 0..h {
                for x in 0..w {
                    bgr.extend_from_slice(&QUADRANTS[quadrant(size, x, y)]);
                }
            }
            let mut jpeg = Vec::new();
            jpeg_encoder::Encoder::new(&mut jpeg, 90)
                .encode(&bgr, w as u16, h as u16, jpeg_encoder::ColorType::Bgr)
                .expect("codificando o quadro");
            jpeg
        }
        Layout::Yuy2 => {
            let mut data = Vec::with_capacity((w * h * 2) as usize);
            for y in 0..h {
                for pair in 0..w / 2 {
                    let (y0, u, v) = yuv(QUADRANTS[quadrant(size, pair * 2, y)]);
                    let (y1, _, _) = yuv(QUADRANTS[quadrant(size, pair * 2 + 1, y)]);
                    data.extend_from_slice(&[y0, u, y1, v]);
                }
            }
            data
        }
    }
}

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 9);
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut inner = kind.to_vec();
    inner.extend_from_slice(body);
    chunk(b"LIST", &inner)
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Writes a short AVI of `frames` identical frames and returns its path.
pub(crate) fn write_avi(directory: &Path, layout: Layout, size: Size, frames: u32) -> PathBuf {
    let picture = frame(layout, size);
    let image = picture.len() as u32;
    let (compression, bits, handler, id) = match layout {
        Layout::Rgb24 => (0u32, 24u16, 0u32, b"00db"),
        Layout::Yuy2 => {
            let fourcc = u32::from_le_bytes(*b"YUY2");
            (fourcc, 16u16, fourcc, b"00db")
        }
        Layout::Mjpeg => {
            let fourcc = u32::from_le_bytes(*b"MJPG");
            (fourcc, 24u16, fourcc, b"00dc")
        }
    };

    let avih = words(&[
        33_333,     // microssegundos por quadro: 30 fps
        image * 30, // bytes por segundo
        0,          // granularidade
        0x10,       // AVIF_HASINDEX
        frames,
        0,
        1, // um fluxo
        image,
        size.width,
        size.height,
        0,
        0,
        0,
        0,
    ]);

    let mut strh = Vec::new();
    strh.extend_from_slice(b"vids");
    strh.extend_from_slice(&handler.to_le_bytes());
    strh.extend_from_slice(&words(&[0])); // flags
    strh.extend_from_slice(&[0, 0, 0, 0]); // prioridade e idioma
    strh.extend_from_slice(&words(&[0, 1, 30, 0, frames, image, u32::MAX, image]));
    for value in [0u16, 0, size.width as u16, size.height as u16] {
        strh.extend_from_slice(&value.to_le_bytes());
    }

    let mut strf = Vec::new();
    strf.extend_from_slice(&40u32.to_le_bytes());
    strf.extend_from_slice(&(size.width as i32).to_le_bytes());
    strf.extend_from_slice(&(size.height as i32).to_le_bytes());
    strf.extend_from_slice(&1u16.to_le_bytes());
    strf.extend_from_slice(&bits.to_le_bytes());
    strf.extend_from_slice(&compression.to_le_bytes());
    strf.extend_from_slice(&image.to_le_bytes());
    strf.extend_from_slice(&[0u8; 16]);

    let hdrl = list(
        b"hdrl",
        &[
            chunk(b"avih", &avih),
            list(
                b"strl",
                &[chunk(b"strh", &strh), chunk(b"strf", &strf)].concat(),
            ),
        ]
        .concat(),
    );

    let mut movi = Vec::new();
    let mut index = Vec::new();
    for _ in 0..frames {
        // O deslocamento do índice conta a partir do "movi" da lista.
        let offset = 4 + movi.len() as u32;
        movi.extend_from_slice(&chunk(id, &picture));
        index.extend_from_slice(id);
        index.extend_from_slice(&words(&[0x10, offset, image]));
    }

    let mut riff = b"AVI ".to_vec();
    riff.extend_from_slice(&hdrl);
    riff.extend_from_slice(&list(b"movi", &movi));
    riff.extend_from_slice(&chunk(b"idx1", &index));
    let file = chunk(b"RIFF", &riff);

    std::fs::create_dir_all(directory).expect("diretorio do teste");
    let path = directory.join(format!("{layout:?}-{}x{}.avi", size.width, size.height));
    std::fs::write(&path, file).expect("gravando o avi");
    path
}
