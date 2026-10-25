#ifndef SIMDRIVER_H
#define SIMDRIVER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/**
 * The opaque handle: a started Driver plus the C callbacks it delivers to.
 */
typedef struct sd_driver sd_driver;

/**
 * One delivered frame. `data` points at `len` tightly packed BGRA bytes and is valid
 * only inside the frame callback.
 */
typedef struct sd_frame {
  uint32_t width;
  uint32_t height;
  uint32_t source_width;
  uint32_t source_height;
  uint64_t seq;
  const uint8_t *data;
  size_t len;
} sd_frame;

typedef void (*sd_frame_cb)(const struct sd_frame *frame, void *user);

typedef void (*sd_state_cb)(int32_t state, void *user);

struct sd_driver *sd_start(const char *udid,
                           uint32_t fps,
                           uint32_t scale,
                           const char *helper_dir,
                           sd_frame_cb frame_cb,
                           sd_state_cb state_cb,
                           void *user);

void sd_pointer(struct sd_driver *d, int32_t phase_code, double x, double y, uint32_t mask);

void sd_scroll(struct sd_driver *d, int32_t phase_code, double x, double y, double dx, double dy);

bool sd_key(struct sd_driver *d, const char *name);

void sd_text(struct sd_driver *d, const char *s);

void sd_button(struct sd_driver *d, int32_t button_code, uint32_t presses, float hold_secs);

/**
 * Stops the driver and frees the handle. NULL-safe; the handle must not be used after.
 */
void sd_stop(struct sd_driver *d);

/**
 * The last error on this thread, from the most recent `sd_` call that failed.
 * Valid until the next `sd_` call on this thread. Empty on a fresh thread. Never NULL.
 */
const char *sd_last_error(void);

#endif  /* SIMDRIVER_H */
