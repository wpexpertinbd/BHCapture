//! Person segmentation for the camera background on Windows and Linux (decision 35/36): Google's
//! MediaPipe selfie segmenter (landscape, 256x144), run in pure Rust with tract. macOS uses Vision.
//!
//! The model file ships unchanged (assets/NOTICE.md). Its last layer is a MediaPipe custom op that
//! tract does not implement -- a 2x2, stride-2 transposed convolution from 16 features to 1 -- so
//! the graph is cut just before it and that layer is computed here (`head`), then the sigmoid.

use tract_tflite::prelude::*;

const MODEL: &[u8] = include_bytes!("../assets/selfie_segmenter_landscape.tflite");
pub const IN_W: usize = 256;
pub const IN_H: usize = 144;
/// The tensor feeding the custom layer.
const CUT: &str = "add_13__xeno_compat__1";
const FEAT: usize = 16;

type Plan = std::sync::Arc<TypedRunnableModel>;

pub struct Segmenter {
    plan: Plan,
    /// [kh][kw][c] for the single output channel.
    kernel: [[[f32; FEAT]; 2]; 2],
    bias: f32,
}

fn f16_to_f32(h: u16) -> f32 {
    let s = (h >> 15) & 1;
    let e = ((h >> 10) & 0x1f) as i32;
    let m = (h & 0x3ff) as f32;
    let v = match e {
        0 => m * 2f32.powi(-24),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    };
    if s == 1 { -v } else { v }
}

impl Segmenter {
    pub fn new() -> Result<Self, String> {
        let e = |x: TractError| x.to_string();
        // The last layer's weights, straight from the file (float16).
        let fb = tract_tflite::tflite::root_as_model(MODEL).map_err(|x| x.to_string())?;
        let sg = fb.subgraphs().ok_or("no subgraph")?.get(0);
        let tensors = sg.tensors().ok_or("no tensors")?;
        let buffers = fb.buffers().ok_or("no buffers")?;
        let weights = |name: &str| -> Result<Vec<f32>, String> {
            let t = tensors.iter().find(|t| t.name() == Some(name)).ok_or(format!("{name} missing"))?;
            let data = buffers.get(t.buffer() as usize).data().ok_or(format!("{name} has no data"))?.bytes();
            Ok(data.chunks_exact(2).map(|c| f16_to_f32(u16::from_le_bytes([c[0], c[1]]))).collect())
        };
        let k = weights("conv2d_transpose/Kernel")?;
        let b = weights("conv2d_transpose/Bias")?;
        if k.len() != 2 * 2 * FEAT || b.len() != 1 { return Err("unexpected last-layer shape".into()); }
        let mut kernel = [[[0f32; FEAT]; 2]; 2];
        for y in 0..2 { for x in 0..2 { for c in 0..FEAT { kernel[y][x][c] = k[(y * 2 + x) * FEAT + c]; } } }

        let mut model = tract_tflite::tflite().model_for_read(&mut std::io::Cursor::new(MODEL)).map_err(e)?;
        let id = model.node_by_name(CUT).map_err(e)?.id;
        model.select_output_outlets(&[OutletId::new(id, 0)]).map_err(e)?;
        let plan = model.into_optimized().map_err(e)?.into_runnable().map_err(e)?;
        Ok(Self { plan, kernel, bias: b[0] })
    }

    /// Person mask (255 = person) for a BGRA frame, at IN_W x IN_H, covering the whole frame.
    pub fn mask(&self, bgra: &[u8], w: u32, h: u32, stride: usize) -> Option<Vec<u8>> {
        if w == 0 || h == 0 || stride < w as usize * 4 || bgra.len() < stride * (h as usize - 1) + w as usize * 4 { return None; }
        // Nearest-neighbour downscale to 256x144, RGB in 0..1, NHWC.
        let mut input = tract_ndarray::Array4::<f32>::zeros((1, IN_H, IN_W, 3));
        for y in 0..IN_H {
            let sy = (y * h as usize) / IN_H;
            for x in 0..IN_W {
                let sx = (x * w as usize) / IN_W;
                let i = sy * stride + sx * 4;
                input[[0, y, x, 0]] = bgra[i + 2] as f32 / 255.0;
                input[[0, y, x, 1]] = bgra[i + 1] as f32 / 255.0;
                input[[0, y, x, 2]] = bgra[i] as f32 / 255.0;
            }
        }
        let out = self.plan.run(tvec!(Tensor::from(input).into())).ok()?;
        let f = out[0].to_plain_array_view::<f32>().ok()?;
        if f.shape() != [1, IN_H / 2, IN_W / 2, FEAT] { return None; }
        Some(head(f.as_slice()?, &self.kernel, self.bias))
    }
}

/// The custom last layer: each feature pixel becomes a 2x2 block of output, then the sigmoid.
fn head(feat: &[f32], kernel: &[[[f32; FEAT]; 2]; 2], bias: f32) -> Vec<u8> {
    let (fw, fh) = (IN_W / 2, IN_H / 2);
    let mut out = vec![0u8; IN_W * IN_H];
    for fy in 0..fh {
        for fx in 0..fw {
            let v = &feat[(fy * fw + fx) * FEAT..(fy * fw + fx + 1) * FEAT];
            for ky in 0..2 {
                for kx in 0..2 {
                    let s: f32 = bias + v.iter().zip(kernel[ky][kx].iter()).map(|(a, b)| a * b).sum::<f32>();
                    out[(fy * 2 + ky) * IN_W + fx * 2 + kx] = (255.0 / (1.0 + (-s).exp())).round() as u8;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_model_is_googles_file_unchanged() {
        // SHA-256 recorded in assets/NOTICE.md; a cheap stand-in check of length and header here,
        // the full hash is checked by the release script.
        assert_eq!(MODEL.len(), 250_177);
        assert_eq!(&MODEL[4..8], b"TFL3");
    }

    #[test]
    fn the_model_loads_and_gives_a_full_size_mask() {
        let s = Segmenter::new().expect("model loads");
        let frame = vec![128u8; 320 * 180 * 4];
        let m = s.mask(&frame, 320, 180, 320 * 4).expect("mask");
        assert_eq!(m.len(), IN_W * IN_H);
        assert!(s.mask(&frame[..100], 320, 180, 320 * 4).is_none(), "short buffer refused");
    }

    #[test]
    fn head_is_a_2x2_upscale_with_sigmoid() {
        let mut k = [[[0f32; FEAT]; 2]; 2];
        k[0][0][0] = 10.0; // top-left of each block follows feature 0
        let mut feat = vec![0f32; (IN_W / 2) * (IN_H / 2) * FEAT];
        feat[0] = 1.0; // first feature pixel, channel 0
        let m = head(&feat, &k, 0.0);
        assert!(m[0] > 250, "strongly positive -> person");
        assert_eq!(m[1], 128, "zero -> 0.5");
        assert_eq!(m[IN_W], 128);
    }
}
