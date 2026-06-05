#ifndef CLEAT_PROVIDER_H
#define CLEAT_PROVIDER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define CLEAT_PROVIDER_ABI_VERSION 1u

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
} cleat_provider_desc;

typedef struct cleat_session_desc {
    uint16_t cols;
    uint16_t rows;
    float cell_width_px;
    float cell_height_px;
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

cleat_dirty_state cleat_session_dirty(const cleat_session *session);
bool cleat_session_snapshot(cleat_session *session, cleat_snapshot *out);
void cleat_session_release_snapshot(cleat_session *session, cleat_snapshot *snapshot);

#ifdef __cplusplus
}
#endif

#endif
