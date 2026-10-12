/* BHCapture Phase-0b spike: minimal C surface over ScreenCaptureKit.
 *
 * Deliberately a C API (the BHDisplay bridging-header pattern), not @_silgen_name
 * and not raw objc2 FFI: ScreenCaptureKit is block/completion-handler based, and
 * blocks are far safer to drive from Objective-C than from Rust.
 *
 * The shim hands Rust the RAW PIXELS. Rust writes the PNG. That is deliberate --
 * if the shim wrote the PNG we would only have proved Objective-C works, not that
 * the Rust side ever holds real pixel data, which is the actual exit criterion.
 */
#ifndef BHC_SCK_SHIM_H
#define BHC_SCK_SHIM_H

#include <stdint.h>

typedef struct {
    uint32_t display_id;
    int32_t  x, y;                 /* logical points, global desktop coords  */
    int32_t  pt_width, pt_height;  /* logical points                         */
    int32_t  px_width, px_height;  /* PHYSICAL pixels (Retina-aware)         */
} BhcDisplayInfo;

typedef struct {
    uint8_t *data;    /* BGRA8888, premultiplied-first; freed by bhc_image_free */
    int32_t  width;   /* physical pixels */
    int32_t  height;  /* physical pixels */
    int32_t  stride;  /* bytes per row; may exceed width*4 */
} BhcImage;

/* 1 = Screen Recording already granted, 0 = not. Never prompts. */
int32_t bhc_preflight_permission(void);

/* Prompts (once per app identity) if not yet decided. 1 = granted. */
int32_t bhc_request_permission(void);

/* Fills up to `cap` entries. Returns count, or -1 on error (reason in `err`). */
int32_t bhc_list_displays(BhcDisplayInfo *out, int32_t cap, char *err, int32_t errcap);

/* Captures one full-resolution frame of `display_id`. 0 = ok, -1 = error. */
int32_t bhc_capture_display(uint32_t display_id, BhcImage *out, char *err, int32_t errcap);

void bhc_image_free(BhcImage *img);

/* Peak resident set size of this process, in bytes. */
uint64_t bhc_peak_rss_bytes(void);

#endif /* BHC_SCK_SHIM_H */
