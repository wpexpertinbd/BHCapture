/* BHCapture macOS capture shim.
 *
 * A plain C surface over ScreenCaptureKit (the BHDisplay bridging-header pattern, not
 * @_silgen_name and not raw objc2 FFI): SCK is entirely block/completion-handler based, and
 * blocks are far safer to drive from Objective-C than from Rust.
 *
 * Every entry point returns a STATUS CODE rather than a bare -1, so the Rust side can tell a
 * permission failure from a missing target without parsing an error string. That distinction
 * matters: a permission failure is the only one where the correct advice is "grant it, then
 * relaunch the app".
 */
#ifndef BHC_SCK_H
#define BHC_SCK_H

#include <stdint.h>

#define BHC_OK                 0
#define BHC_ERR_GENERIC       -1
#define BHC_ERR_PERMISSION    -2
#define BHC_ERR_UNSUPPORTED   -3
#define BHC_ERR_NOT_FOUND     -4

#define BHC_TITLE_CAP 512
#define BHC_APP_CAP   256

typedef struct {
    uint32_t display_id;
    int32_t  x, y;                 /* logical points, global desktop coords */
    int32_t  pt_width, pt_height;  /* logical points  */
    int32_t  px_width, px_height;  /* physical pixels */
} BhcDisplayInfo;

typedef struct {
    uint64_t window_id;
    int32_t  x, y;
    int32_t  pt_width, pt_height;
    int32_t  on_screen;            /* 0/1 */
    char     title[BHC_TITLE_CAP];
    char     app[BHC_APP_CAP];
} BhcWindowInfo;

typedef struct {
    uint8_t *data;    /* BGRA8888 premultiplied; free with bhc_image_free */
    int32_t  width;
    int32_t  height;
    int32_t  stride;
    double   scale;   /* physical pixels per logical point for this image */
} BhcImage;

/* 1 = Screen Recording granted. Never prompts. */
int32_t bhc_preflight_permission(void);
/* Raises the system prompt when undecided. The grant does NOT take effect in this process. */
int32_t bhc_request_permission(void);

int32_t bhc_list_displays(BhcDisplayInfo *out, int32_t cap, int32_t *n_out, char *err, int32_t errcap);
int32_t bhc_list_windows(BhcWindowInfo *out, int32_t cap, int32_t *n_out, char *err, int32_t errcap);
/* `exclude` lists CGWindowIDs to leave out of the capture -- our own overlay windows. Excluding
 * them is how the selector avoids photographing itself; hiding the overlay first would flash and
 * would race the compositor, since hide() returns before the window is actually gone. */
int32_t bhc_capture_display(uint32_t display_id, const uint32_t *exclude, int32_t exclude_n,
                            BhcImage *out, char *err, int32_t errcap);

/* CGWindowID of an NSWindow pointer (what Tauri hands back from ns_window()). */
uint32_t bhc_cgwindow_id(void *nswindow);
int32_t bhc_capture_window(uint64_t window_id, BhcImage *out, char *err, int32_t errcap);
void    bhc_image_free(BhcImage *img);

#endif /* BHC_SCK_H */
