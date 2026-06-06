#ifndef CLEAT_PROVIDER_H
#define CLEAT_PROVIDER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define CLEAT_PROVIDER_ABI_VERSION 1u
#define CLEAT_PROVIDER_BACKEND_MOCK 0u
#define CLEAT_PROVIDER_BACKEND_IN_PROCESS 1u
#define CLEAT_PROVIDER_BACKEND_DAEMON 2u
#define CLEAT_PROVIDER_VT_DEFAULT 0u
#define CLEAT_PROVIDER_VT_PASSTHROUGH 1u
#define CLEAT_PROVIDER_VT_GHOSTTY 2u
#define CLEAT_INPUT_KEY 1u
#define CLEAT_INPUT_TEXT 2u
#define CLEAT_INPUT_MOUSE 3u
#define CLEAT_INPUT_FOCUS 4u
#define CLEAT_INPUT_PASTE 5u
#define CLEAT_INPUT_RESIZE 6u
#define CLEAT_KEY_UNICODE_SCALAR 1u
#define CLEAT_KEY_NAMED 2u
#define CLEAT_KEY_ENTER 1u
#define CLEAT_KEY_ESCAPE 2u
#define CLEAT_KEY_BACKSPACE 3u
#define CLEAT_KEY_TAB 4u
#define CLEAT_KEY_DELETE 5u
#define CLEAT_KEY_ARROW_UP 12u
#define CLEAT_KEY_ARROW_DOWN 13u
#define CLEAT_KEY_ARROW_LEFT 14u
#define CLEAT_KEY_ARROW_RIGHT 15u
#define CLEAT_CELL_WIDTH_NARROW 0u
#define CLEAT_CELL_WIDTH_WIDE 1u
#define CLEAT_CELL_WIDTH_SPACER_TAIL 2u
#define CLEAT_CELL_WIDTH_SPACER_HEAD 3u
#define CLEAT_CURSOR_STYLE_BAR 0u
#define CLEAT_CURSOR_STYLE_BLOCK 1u
#define CLEAT_CURSOR_STYLE_UNDERLINE 2u
#define CLEAT_CURSOR_STYLE_BLOCK_HOLLOW 3u

typedef struct CleatProvider cleat_provider;
typedef struct CleatSession cleat_session;

typedef enum cleat_dirty_state {
    CLEAT_DIRTY_CLEAN = 0,
    CLEAT_DIRTY_PARTIAL = 1,
    CLEAT_DIRTY_FULL = 2,
} cleat_dirty_state;

typedef struct cleat_provider_desc {
    uint32_t abi_version;
    uint32_t requested_features;
    uint32_t backend;
    const uint8_t *runtime_root;
    size_t runtime_root_len;
} cleat_provider_desc;

typedef struct cleat_session_desc {
    uint16_t cols;
    uint16_t rows;
    float cell_width_px;
    float cell_height_px;
    uint32_t vt_engine;
    const uint8_t *command;
    size_t command_len;
    const uint8_t *cwd;
    size_t cwd_len;
    bool record;
} cleat_session_desc;

typedef struct cleat_rgb {
    uint8_t r;
    uint8_t g;
    uint8_t b;
} cleat_rgb;

typedef struct cleat_cell {
    const uint32_t *graphemes;
    size_t grapheme_count;
    cleat_rgb fg;
    cleat_rgb bg;
    uint32_t flags;
    uint32_t width;
} cleat_cell;

typedef struct cleat_cursor {
    uint16_t col;
    uint16_t row;
    bool visible;
    uint32_t style;
    bool wide_tail;
} cleat_cursor;

typedef struct cleat_snapshot {
    uint16_t cols;
    uint16_t rows;
    const cleat_cell *cells;
    size_t cell_count;
    cleat_cursor cursor;
    cleat_dirty_state dirty;
} cleat_snapshot;

typedef struct cleat_input_event {
    uint32_t kind;
    uint16_t modifiers;
    uint32_t key_kind;
    uint32_t key_code;
    const uint8_t *text;
    size_t text_len;
    uint16_t cell_col;
    uint16_t cell_row;
    float x_px;
    float y_px;
    float wheel_delta_x;
    float wheel_delta_y;
} cleat_input_event;

uint32_t cleat_provider_abi_version(void);

cleat_provider *cleat_provider_open(const cleat_provider_desc *desc);
void cleat_provider_close(cleat_provider *provider);

cleat_session *cleat_session_create(cleat_provider *provider, const cleat_session_desc *desc);
void cleat_session_destroy(cleat_session *session);

bool cleat_session_resize(cleat_session *session, uint16_t cols, uint16_t rows, float cell_w_px, float cell_h_px);
bool cleat_session_send_input(cleat_session *session, const cleat_input_event *event);
bool cleat_session_write_bytes(cleat_session *session, const uint8_t *bytes, size_t size);

cleat_dirty_state cleat_session_poll(cleat_session *session);
cleat_dirty_state cleat_session_dirty(const cleat_session *session);

/*
 * Only one snapshot may be live per session. Call
 * cleat_session_release_snapshot before requesting another snapshot for the
 * same session.
 */
bool cleat_session_snapshot(cleat_session *session, cleat_snapshot *out);
void cleat_session_release_snapshot(cleat_session *session, cleat_snapshot *snapshot);

#ifdef __cplusplus
}
#endif

#endif
