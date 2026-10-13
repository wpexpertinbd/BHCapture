/* BHCapture macOS screen recorder.
 *
 * ScreenCaptureKit delivers frames and audio; AVAssetWriter encodes them with the system's own
 * H.264 (VideoToolbox, hardware) and AAC encoders and writes the MP4 AS IT GOES -- a recording is
 * never held in memory. No ffmpeg, nothing bundled: decision 3, "native encoders, small bundle".
 *
 * Threading: every sample (video, system audio, microphone) arrives on ONE serial queue, so the
 * writer is only ever touched from one place. The control calls below may come from any thread.
 */
#ifndef BHC_REC_H
#define BHC_REC_H

#include <stdint.h>
#include <stddef.h>

typedef struct BhcRecorder BhcRecorder;

typedef struct {
    uint32_t    display_id;
    /* Region in DISPLAY-LOCAL logical points (origin = the display's top-left). */
    double      x, y, w, h;
    int32_t     fps;            /* 1..60 */
    int32_t     show_cursor;    /* 0/1 */
    int32_t     system_audio;   /* 0/1 */
    int32_t     microphone;     /* 0/1; needs macOS 15 -- refused below that rather than faked */
    int32_t     noise_filter;   /* 0/1; RNNoise on the microphone (48 kHz float only) */
    int32_t     auto_level;     /* 0/1; automatic voice level on the microphone */
    int32_t     voice_clarity;  /* 0/1; voice EQ on the microphone */
    uint32_t    include_window; /* CGWindowID of OUR window to keep in the picture (click rings); 0 = none */
    const char *path;           /* UTF-8; the .mp4 to create. Must not exist. */
} BhcRecordOptions;

typedef struct {
    int32_t state;              /* BHC_REC_* */
    double  seconds;            /* recorded time, pauses excluded */
    int64_t frames;             /* video frames written */
    int32_t width, height;      /* encoded size in pixels */
    float   level_system;       /* 0..1 peak since the last status call */
    float   level_mic;
} BhcRecordStatus;

#define BHC_REC_RECORDING 1
#define BHC_REC_PAUSED    2
#define BHC_REC_FAILED    3     /* stopped by an error (disk full, display gone...); call stop */

/* Start recording. Returns NULL and fills `err` on failure. Blocks until capture has started. */
BhcRecorder *bhc_rec_start(const BhcRecordOptions *opt, int32_t *code, char *err, int32_t errcap);
void         bhc_rec_set_paused(BhcRecorder *r, int32_t paused);
void         bhc_rec_status(BhcRecorder *r, BhcRecordStatus *out, char *err, int32_t errcap);
/* Stop and finish the file. Returns BHC_OK when a playable file was written. Always frees `r`. */
int32_t      bhc_rec_stop(BhcRecorder *r, char *err, int32_t errcap);

/* Mix all audio tracks of src into one stereo track in dst (video copied as is). */
int32_t      bhc_rec_mix_audio(const char *src_path, const char *dst_path, char *err, int32_t errcap);

/* 1 when ScreenCaptureKit can record the microphone (macOS 15+). */
int32_t      bhc_rec_mic_supported(void);

/* The noise filter, implemented in Rust (bhcapture_core::denoise). */
void  *bhc_denoise_new(void);
void   bhc_denoise_process(void *d, float *samples, size_t n);
void   bhc_denoise_free(void *d);
/* Automatic voice level (bhcapture_core::level). */
void  *bhc_level_new(uint32_t sample_rate);
void   bhc_level_process(void *l, float *samples, size_t n);
void   bhc_level_free(void *l);
/* Voice EQ (bhcapture_core::voice_eq). */
void  *bhc_eq_new(uint32_t sample_rate);
void   bhc_eq_process(void *e, float *samples, size_t n);
void   bhc_eq_free(void *e);

/* Decode a recording to RGBA frames at `fps` (longest side <= max_side); cb returns non-zero to stop. */
int32_t bhc_video_frames(const char *path, double fps, int32_t max_side,
                         int32_t (*cb)(const uint8_t *rgba, int32_t w, int32_t h, double t, double dur, void *ctx),
                         void *ctx, char *err, int32_t errcap);

/* Re-wrap as MOV (kind 0) or re-encode as HEVC MP4 (kind 1). progress returns non-zero to cancel. */
int32_t bhc_export_video(const char *src_path, const char *dst_path, int32_t kind,
                         int32_t (*progress)(double, void *), void *ctx, char *err, int32_t errcap);

/* Report every left/right mouse-down anywhere on screen as global TOP-LEFT logical points, for the
 * click highlight. A global NSEvent monitor: needs no Accessibility permission for mouse events.
 * Main thread only. One watcher at a time; watch again replaces it. */
void bhc_watch_clicks(void (*cb)(double x, double y, int32_t right, void *ctx), void *ctx);
void bhc_unwatch_clicks(void);

#endif /* BHC_REC_H */
