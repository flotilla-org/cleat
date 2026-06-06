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
#define CLEAT_KEY_INSERT 6u
#define CLEAT_KEY_HOME 7u
#define CLEAT_KEY_END 8u
#define CLEAT_KEY_PAGE_UP 9u
#define CLEAT_KEY_PAGE_DOWN 10u
#define CLEAT_KEY_ARROW_UP 12u
#define CLEAT_KEY_ARROW_DOWN 13u
#define CLEAT_KEY_ARROW_LEFT 14u
#define CLEAT_KEY_ARROW_RIGHT 15u
#define CLEAT_KEY_FUNCTION_BASE 100u
#define CLEAT_KEY_F1 101u
#define CLEAT_KEY_F2 102u
#define CLEAT_KEY_F3 103u
#define CLEAT_KEY_F4 104u
#define CLEAT_KEY_F5 105u
#define CLEAT_KEY_F6 106u
#define CLEAT_KEY_F7 107u
#define CLEAT_KEY_F8 108u
#define CLEAT_KEY_F9 109u
#define CLEAT_KEY_F10 110u
#define CLEAT_KEY_F11 111u
#define CLEAT_KEY_F12 112u
#define CLEAT_KEY_ACTION_PRESS 1u
#define CLEAT_KEY_ACTION_REPEAT 2u
#define CLEAT_KEY_ACTION_RELEASE 3u
#define CLEAT_MOD_SHIFT 1u
#define CLEAT_MOD_CTRL 2u
#define CLEAT_MOD_ALT 4u
#define CLEAT_MOD_SUPER 8u
#define CLEAT_MOUSE_PRESS 1u
#define CLEAT_MOUSE_RELEASE 2u
#define CLEAT_MOUSE_MOVE 3u
#define CLEAT_MOUSE_WHEEL 4u
#define CLEAT_MOUSE_BUTTON_NONE 0u
#define CLEAT_MOUSE_BUTTON_LEFT 1u
#define CLEAT_MOUSE_BUTTON_MIDDLE 2u
#define CLEAT_MOUSE_BUTTON_RIGHT 3u
#define CLEAT_MOUSE_BUTTON_BACK 4u
#define CLEAT_MOUSE_BUTTON_FORWARD 5u
#define CLEAT_MOUSE_BUTTON_FLAG_LEFT 1u
#define CLEAT_MOUSE_BUTTON_FLAG_MIDDLE 2u
#define CLEAT_MOUSE_BUTTON_FLAG_RIGHT 4u
#define CLEAT_MOUSE_BUTTON_FLAG_BACK 8u
#define CLEAT_MOUSE_BUTTON_FLAG_FORWARD 16u
#define CLEAT_CELL_WIDTH_NARROW 0u
#define CLEAT_CELL_WIDTH_WIDE 1u
#define CLEAT_CELL_WIDTH_SPACER_TAIL 2u
#define CLEAT_CELL_WIDTH_SPACER_HEAD 3u
#define CLEAT_CURSOR_STYLE_BAR 0u
#define CLEAT_CURSOR_STYLE_BLOCK 1u
#define CLEAT_CURSOR_STYLE_UNDERLINE 2u
#define CLEAT_CURSOR_STYLE_BLOCK_HOLLOW 3u
#define CLEAT_VIEWPORT_LIVE_NORMAL 1u
#define CLEAT_VIEWPORT_LIVE_ALTERNATE 2u
#define CLEAT_VIEWPORT_NORMAL_SCROLLBACK 3u
#define CLEAT_VIEWPORT_COMMAND_TOP 1u
#define CLEAT_VIEWPORT_COMMAND_BOTTOM 2u
#define CLEAT_VIEWPORT_COMMAND_DELTA_ROWS 3u
#define CLEAT_VIEWPORT_OUTCOME_MOVED 1u
#define CLEAT_VIEWPORT_OUTCOME_NO_OP 2u
#define CLEAT_VIEWPORT_OUTCOME_UNSUPPORTED 3u

typedef struct CleatProvider cleat_provider;
typedef struct CleatSession cleat_session;
typedef void cleat_wake_fn(void *user_data);

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

typedef struct cleat_terminal_geometry {
    float cell_width_px;
    float cell_height_px;
    float content_x_px;
    float content_y_px;
    float content_width_px;
    float content_height_px;
} cleat_terminal_geometry;

typedef struct cleat_terminal_scrollbar_state {
    uint32_t viewport_kind;
    uint64_t total_rows;
    uint16_t viewport_rows;
    uint64_t viewport_top_row;
    bool at_bottom;
} cleat_terminal_scrollbar_state;

typedef struct cleat_snapshot {
    uint16_t cols;
    uint16_t rows;
    cleat_terminal_geometry geometry;
    uint32_t viewport_kind;
    uint64_t scrollback_offset_rows;
    cleat_terminal_scrollbar_state scrollbar;
    uint64_t render_generation;
    const cleat_cell *cells;
    size_t cell_count;
    const uint16_t *dirty_rows;
    size_t dirty_row_count;
    cleat_cursor cursor;
    cleat_dirty_state dirty;
} cleat_snapshot;

/*
 * For mouse events, cell_col/cell_row are the authoritative terminal-cell
 * coordinates. x_px/y_px are terminal pixel coordinates relative to the
 * terminal content area.
 */
typedef struct cleat_input_event {
    uint32_t kind;
    uint16_t modifiers;
    uint16_t consumed_modifiers;
    bool focused;
    uint32_t key_action;
    uint32_t key_kind;
    uint32_t key_code;
    const uint8_t *text;
    size_t text_len;
    const uint8_t *generated_text;
    size_t generated_text_len;
    uint32_t platform_keycode;
    uint32_t mouse_kind;
    uint32_t mouse_button;
    uint16_t mouse_buttons;
    uint16_t cell_col;
    uint16_t cell_row;
    float x_px;
    float y_px;
    float wheel_delta_x;
    float wheel_delta_y;
} cleat_input_event;

typedef struct cleat_input_result {
    uint64_t first_sequence;
    size_t count;
} cleat_input_result;

typedef struct cleat_scrollback_extent {
    uint64_t normal_scrollback_rows;
    uint16_t live_rows;
    bool alternate_screen;
} cleat_scrollback_extent;

typedef struct cleat_viewport_request {
    uint32_t kind;
    uint64_t scrollback_offset_rows;
} cleat_viewport_request;

typedef struct cleat_viewport_command {
    uint32_t kind;
    int64_t delta_rows;
} cleat_viewport_command;

typedef struct cleat_viewport_command_result {
    uint32_t outcome;
} cleat_viewport_command_result;

uint32_t cleat_provider_abi_version(void);

cleat_provider *cleat_provider_open(const cleat_provider_desc *desc);
/*
 * Registers an edge-triggered wake callback. The provider calls it when a
 * session transitions from observed/clean to dirty; callers should poll
 * sessions to find the one that needs attention.
 */
void cleat_provider_set_wake_callback(cleat_provider *provider, cleat_wake_fn *wake, void *user_data);
void cleat_provider_close(cleat_provider *provider);

cleat_session *cleat_session_create(cleat_provider *provider, const cleat_session_desc *desc);
void cleat_session_destroy(cleat_session *session);

/* Updates row/column terminal size. Pixel geometry is updated separately. */
bool cleat_session_resize(cleat_session *session, uint16_t cols, uint16_t rows);
/*
 * Updates terminal-visible pixel geometry without changing row/column size.
 * content_* describes the terminal content rect; input pixel coordinates are
 * relative to that content area.
 */
bool cleat_session_update_geometry(cleat_session *session, const cleat_terminal_geometry *geometry);
bool cleat_session_send_input(cleat_session *session, const cleat_input_event *event);
bool cleat_session_send_input_ex(cleat_session *session, const cleat_input_event *event, cleat_input_result *out);
bool cleat_session_send_input_batch(cleat_session *session, const cleat_input_event *events, size_t event_count, cleat_input_result *out);
bool cleat_session_write_bytes(cleat_session *session, const uint8_t *bytes, size_t size);

cleat_dirty_state cleat_session_poll(cleat_session *session);
cleat_dirty_state cleat_session_dirty(const cleat_session *session);
/*
 * Marks a render_generation returned by cleat_session_snapshot as observed.
 * Snapshots do not clear dirty state by themselves.
 */
bool cleat_session_mark_observed(cleat_session *session, uint64_t generation);
/*
 * Reports VT-owned live scrollback state. This is not derived from session
 * recordings. Providers may report zero normal scrollback rows until the VT
 * backend exposes scrollback rows.
 */
bool cleat_session_scrollback_extent(cleat_session *session, cleat_scrollback_extent *out);
bool cleat_session_scrollbar_state(cleat_session *session, cleat_terminal_scrollbar_state *out);
bool cleat_session_scroll_viewport(cleat_session *session,
                                   const cleat_viewport_command *command,
                                   cleat_viewport_command_result *out);

/*
 * Only one snapshot may be live per session. Call
 * cleat_session_release_snapshot before requesting another snapshot for the
 * same session.
 * dirty_rows is populated only when dirty is CLEAT_DIRTY_PARTIAL and the
 * provider knows exact dirty rows; otherwise dirty_row_count is zero.
 */
bool cleat_session_snapshot(cleat_session *session, cleat_snapshot *out);
/*
 * Returns a snapshot for a requested terminal viewport. Unsupported viewport
 * kinds or offsets return false.
 */
bool cleat_session_viewport_snapshot(cleat_session *session, const cleat_viewport_request *request, cleat_snapshot *out);
void cleat_session_release_snapshot(cleat_session *session, cleat_snapshot *snapshot);

#ifdef __cplusplus
}
#endif

#endif
