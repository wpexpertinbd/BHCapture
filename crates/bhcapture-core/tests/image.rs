use bhcapture_core::geom::*;
use bhcapture_core::image::*;

fn sc(v: f64) -> Scale { Scale::exact(v) }
const BLUE: [u8; 4] = [255, 0, 0, 255];   // BGRA
const RED:  [u8; 4] = [0, 0, 255, 255];

fn filled(w: u32, h: u32, k: f64, px: [u8; 4]) -> Frame {
    let mut f = Frame::transparent(w, h, sc(k)).unwrap();
    for y in 0..h { for x in 0..w { f.set_pixel(x, y, px); } }
    f
}

#[test]
fn section0_rig_selfcheck() {
    let f = filled(4, 4, 1.0, RED);
    assert_eq!(f.width(), 4);
    assert_eq!(f.pixel(0, 0), Some(RED));
    assert_eq!(f.pixel(4, 0), None, "out of bounds must be None, not a wrapped read");
}

#[test]
fn construction_validates_instead_of_trusting_ffi() {
    assert_eq!(Frame::from_bgra(vec![0; 16], 0, 4, 16, sc(1.0)).unwrap_err(), FrameError::ZeroSized);
    assert_eq!(
        Frame::from_bgra(vec![0; 64], 4, 4, 8, sc(1.0)).unwrap_err(),
        FrameError::StrideTooSmall { stride: 8, min: 16 }
    );
    assert_eq!(
        Frame::from_bgra(vec![0; 32], 4, 4, 16, sc(1.0)).unwrap_err(),
        FrameError::BufferTooSmall { got: 32, need: 64 }
    );
    // Padded stride is legal and common -- backends align rows.
    assert!(Frame::from_bgra(vec![0; 4 * 24], 4, 4, 24, sc(1.0)).is_ok());
}

#[test]
fn to_rgba8_reorders_channels_and_packs_tightly() {
    // Deliberately padded stride, so packing is actually exercised.
    let mut f = Frame::from_bgra(vec![0; 2 * 24], 2, 2, 24, sc(1.0)).unwrap();
    f.set_pixel(0, 0, [1, 2, 3, 4]); // B,G,R,A
    let out = f.to_rgba8();
    assert_eq!(out.len(), 2 * 2 * 4, "output must be tightly packed, not strided");
    assert_eq!(&out[0..4], &[3, 2, 1, 4], "expected R,G,B,A");
}

#[test]
fn same_size_blit_is_an_exact_copy() {
    let src = filled(10, 10, 1.0, RED);
    let mut dst = filled(10, 10, 1.0, BLUE);
    dst.blit(PhysRect::new(2, 2, 4, 4), &src, PhysRect::new(0, 0, 4, 4));
    assert_eq!(dst.pixel(3, 3), Some(RED), "inside the blit");
    assert_eq!(dst.pixel(1, 1), Some(BLUE), "outside the blit must be untouched");
    assert_eq!(dst.pixel(6, 6), Some(BLUE), "past the blit must be untouched");
}

#[test]
fn out_of_range_blit_is_a_noop_not_a_panic() {
    let src = filled(4, 4, 1.0, RED);
    let mut dst = filled(4, 4, 1.0, BLUE);
    dst.blit(PhysRect::new(100, 100, 4, 4), &src, PhysRect::new(0, 0, 4, 4));
    dst.blit(PhysRect::new(0, 0, 4, 4), &src, PhysRect::new(99, 99, 4, 4));
    dst.blit(PhysRect::new(0, 0, 0, 0), &src, PhysRect::new(0, 0, 4, 4));
    for y in 0..4 { for x in 0..4 { assert_eq!(dst.pixel(x, y), Some(BLUE)); } }
}

#[test]
fn upscaling_blit_fills_the_whole_destination() {
    let src = filled(2, 2, 1.0, RED);
    let mut dst = filled(8, 8, 2.0, BLUE);
    dst.blit(PhysRect::new(0, 0, 8, 8), &src, PhysRect::new(0, 0, 2, 2));
    // A uniform source must stay uniform however it is resampled -- no edge artefacts,
    // no untouched destination pixels.
    for y in 0..8 { for x in 0..8 {
        assert_eq!(dst.pixel(x, y), Some(RED), "unwritten or discoloured pixel at ({x},{y})");
    } }
}

#[test]
fn resampling_interpolates_rather_than_duplicating() {
    // Left column black, right column white; upscaled 2px -> 8px.
    let mut src = Frame::transparent(2, 1, sc(1.0)).unwrap();
    src.set_pixel(0, 0, [0, 0, 0, 255]);
    src.set_pixel(1, 0, [255, 255, 255, 255]);
    let mut dst = Frame::transparent(8, 1, sc(1.0)).unwrap();
    dst.blit(PhysRect::new(0, 0, 8, 1), &src, PhysRect::new(0, 0, 2, 1));

    let v: Vec<u8> = (0..8).map(|x| dst.pixel(x, 0).unwrap()[0]).collect();
    assert_eq!(v[0], 0, "clamped at the left edge");
    assert_eq!(v[7], 255, "clamped at the right edge");
    assert!(v.windows(2).all(|w| w[0] <= w[1]), "must ramp monotonically, got {v:?}");
    let mids = &v[1..7];
    assert!(mids.iter().any(|&p| p != 0 && p != 255),
        "nearest-neighbour would give only 0s and 255s, got {v:?}");
}
