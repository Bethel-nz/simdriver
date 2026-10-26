// Task 10 C smoke test: compiled against the built cdylib and run by tests/c_abi.rs.
// Exercises only local error paths — no simulator needed.
#include <simdriver.h>
#include <stdio.h>

static void on_frame(const sd_frame *f, void *user) { (void)f; (void)user; }
static void on_state(int32_t state, void *user) { (void)state; (void)user; }

int main(void) {
    // Unknown device: start fails and says why.
    sd_driver *d = sd_start("00000000-0000-0000-0000-000000000000", 60, 2, NULL,
                            on_frame, on_state, NULL);
    if (d != NULL) {
        fprintf(stderr, "sd_start on an unknown device must return NULL\n");
        return 1;
    }
    const char *err = sd_last_error();
    if (err == NULL || err[0] == '\0') {
        fprintf(stderr, "expected a non-empty sd_last_error, got: %s\n", err ? err : "(null)");
        return 1;
    }
    printf("error as expected: %s\n", err);

    // NULL-safety on every entry point.
    sd_stop(NULL);
    // ...and the next call that succeeds clears the error.
    err = sd_last_error();
    if (err == NULL || err[0] != '\0') {
        fprintf(stderr, "sd_last_error must be empty after a successful call, got: %s\n", err ? err : "(null)");
        return 1;
    }
    sd_pointer(NULL, 0, 0.5, 0.5, 0);
    sd_scroll(NULL, 1, 0.5, 0.5, 0.0, -0.1);
    sd_touch2(NULL, 0, 0.25, 0.5, 0.75, 0.5);
    double sw = -1, sh = -1;
    if (sd_screen_points(NULL, &sw, &sh) || sw != -1 || sh != -1) {
        fprintf(stderr, "sd_screen_points(NULL) must return false and write nothing\n");
        return 1;
    }
    if (sd_key(NULL, "enter")) {
        fprintf(stderr, "sd_key(NULL) must return false\n");
        return 1;
    }
    sd_text(NULL, "hi");
    sd_tap(NULL, 0.5, 0.5);
    if (sd_accessibility_json(NULL) != NULL) {
        fprintf(stderr, "sd_accessibility_json(NULL) must return NULL\n");
        return 1;
    }
    sd_string_free(NULL);
    double gx = 0, gy = 0;
    if (sd_grid_rows(402, 874, 6) != 13 || !sd_grid_cell(402, 874, 6, "a1", &gx, &gy)
        || gx != 1.0 / 12.0 || gy != 1.0 / 26.0 || sd_grid_cell(402, 874, 6, "O1", &gx, &gy)
        || sd_grid_cell(402, 874, 6, "E1", &gx, &gy)) {
        fprintf(stderr, "grid: rows %u, A1 at (%f, %f)\n", sd_grid_rows(402, 874, 6), gx, gy);
        return 1;
    }
    sd_button(NULL, 0, 1, 0.0f);

    printf("smoke ok\n");
    return 0;
}
