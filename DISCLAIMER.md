# Disclaimer

BHCapture is an independent, open-source project by [BiswasHost](https://www.biswashost.com).

It is **not affiliated with, endorsed by, or derived from** Snagit, Camtasia, TechSmith, or any other
capture product. Those names are trademarks of their respective owners and are mentioned only to
describe what kind of tool BHCapture is. No proprietary code or assets from those products are included.

BHCapture builds on these open-source components, each under its own license:

- [Tauri](https://tauri.app) — application shell (MIT / Apache-2.0)
- [React](https://react.dev) — user interface (MIT)
- [nnnoiseless](https://github.com/jneem/nnnoiseless) — Rust port of Xiph's RNNoise noise filter (BSD-3-Clause)
- [image](https://github.com/image-rs/image) and [gif](https://github.com/image-rs/image-gif) — image encoders (MIT / Apache-2.0)
- [global-hotkey](https://github.com/tauri-apps/global-hotkey) — system-wide shortcuts (MIT / Apache-2.0)

Video and audio are encoded by your operating system's own encoders (Apple VideoToolbox/AVFoundation,
Microsoft Media Foundation); no codec is bundled on those platforms.

Use at your own risk. See [`LICENSE`](LICENSE) for warranty terms (there are none).
