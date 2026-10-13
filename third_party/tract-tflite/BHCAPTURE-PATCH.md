# tract-tflite 0.23.8, patched for BHCapture

Upstream: https://github.com/sonos/tract (crate `tract-tflite` 0.23.8), MIT OR Apache-2.0 —
see `LICENSE-MIT` and `LICENSE-APACHE`.

Changed (only `src/ops/mod.rs`, two registrations and two small functions at the end):

- `DEQUANTIZE` → a cast to f32. Google's MediaPipe selfie segmenter stores its weights as float16
  behind DEQUANTIZE ops, which upstream does not translate.
- `CUSTOM` → passes its first input through. The model's only custom op is its last layer
  (`Convolution2DTransposeBias`, a 2x2 stride-2 transposed convolution); BHCapture cuts the graph
  just before it and computes that layer itself (`bhcapture_core::segment`).

Everything else is the published crate, unchanged.
