/* BHCapture macOS camera: AVFoundation capture, and Vision's person segmentation for background
 * replacement (built into macOS 12+, nothing downloaded).
 *
 * Frames are delivered as tightly packed BGRA on a private serial queue. When segmentation is on,
 * each frame comes with a one-byte-per-pixel person mask (255 = person) at the mask's own size;
 * the compositing itself is portable Rust (bhcapture_core::camera), shared with Windows and Linux.
 */
#ifndef BHC_CAM_H
#define BHC_CAM_H

#include <stdint.h>

typedef struct BhcCamera BhcCamera;

/* Called once per frame on the capture queue. `mask` is NULL unless segmentation is on. */
typedef void (*BhcCamFrameFn)(void *ctx, const uint8_t *bgra, int32_t w, int32_t h, int32_t stride,
                              const uint8_t *mask, int32_t mw, int32_t mh, int32_t mstride);

/* Fills `out` with "uniqueID\tname\n" lines (UTF-8). Returns the number of cameras. */
int32_t bhc_cam_list(char *out, int32_t cap);

/* 0 = not asked yet, 1 = allowed, 2 = denied or restricted. */
int32_t bhc_cam_permission(void);
/* Ask (shows the system prompt the first time); blocks until answered. Returns 1 when allowed. */
int32_t bhc_cam_request_permission(void);

/* Start a camera ("" = the default one). `max_height` picks the largest format up to it (e.g.
 * 1080). Returns NULL and fills `err` on failure. */
BhcCamera *bhc_cam_start(const char *unique_id, int32_t max_height, int32_t fps,
                         BhcCamFrameFn fn, void *ctx, char *err, int32_t errcap);
/* Turn person segmentation on/off while running (0 off, 1 fast, 2 balanced). */
void bhc_cam_set_segmentation(BhcCamera *c, int32_t level);
/* Stop. After it returns no further frame callback runs. Frees `c`. */
void bhc_cam_stop(BhcCamera *c);

#endif
