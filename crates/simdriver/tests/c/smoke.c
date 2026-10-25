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
    if (sd_key(NULL, "enter")) {
        fprintf(stderr, "sd_key(NULL) must return false\n");
        return 1;
    }
    sd_text(NULL, "hi");
    sd_button(NULL, 0, 1, 0.0f);

    printf("smoke ok\n");
    return 0;
}
