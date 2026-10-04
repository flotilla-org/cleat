#define _POSIX_C_SOURCE 200809L
#include "cleat_provider.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
static const cleat_clipboard_event *acquire(cleat_session *s) {
    for (unsigned i = 0; i < 1000; ++i) {
        const cleat_clipboard_event *event = cleat_session_acquire_clipboard_event(s);
        if (event) return event;
        struct timespec delay = { .tv_nsec = 10000000 };
        nanosleep(&delay, NULL);
    }
    assert(!"timed out acquiring an owned clipboard event"); return NULL;
}
int main(int argc, char **argv) {
    assert(argc == 2);
    /* Ownership contract: acquisitions survive session/provider destruction,
     * repeated draining removes events once, and clear is distinct from text. */
    cleat_provider_desc provider_desc = { .abi_version = CLEAT_PROVIDER_ABI_VERSION,
        .backend = CLEAT_PROVIDER_BACKEND_IN_PROCESS, .runtime_root = (const uint8_t *)argv[1], .runtime_root_len = strlen(argv[1]) };
    cleat_provider *provider = cleat_provider_open(&provider_desc); assert(provider);
    const char *command = "stty -echo; sleep 0.1; read line; printf '\\033]52;c;aGVsbG8=\\007\\033]52;s;\\033\\\\'; sleep 30";
    cleat_session_desc desc = { .cols = 80, .rows = 24, .vt_engine = CLEAT_PROVIDER_VT_GHOSTTY,
        .command = (const uint8_t *)command, .command_len = strlen(command) };
    cleat_session *session = cleat_session_create(provider, &desc); assert(session);
    assert(cleat_session_clipboard_supported(session));
    assert(cleat_session_write_bytes(session, (const uint8_t *)"go\n", 3));
    const cleat_clipboard_event *text = acquire(session);
    const cleat_clipboard_event *clear = acquire(session);
    assert(text->kind == 1 && text->destination == 0 && text->text_len == 5);
    assert(!memcmp(text->text, "hello", 5));
    assert(clear->kind == 2 && clear->destination == 1 && !clear->text && clear->text_len == 0);
    assert(clear->sequence > text->sequence);
    assert(!memcmp(text->session_epoch, clear->session_epoch, 16));
    assert(!cleat_session_acquire_clipboard_event(session));
    cleat_session_destroy(session); cleat_provider_close(provider);
    char native_sink[6] = {0}; memcpy(native_sink, text->text, text->text_len);
    assert(!strcmp(native_sink, "hello"));
    cleat_clipboard_event_release(clear); cleat_clipboard_event_release(text); cleat_clipboard_event_release(NULL);
    puts("PASS: owned C ABI clipboard acquisition/release and native sink");
}
