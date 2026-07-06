#ifndef CLEAT_PROVIDER_H
#define CLEAT_PROVIDER_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define CLEAT_PROVIDER_ABI_VERSION 7u
#define CLEAT_PROVIDER_BACKEND_MOCK 0u
#define CLEAT_PROVIDER_BACKEND_IN_PROCESS 1u
#define CLEAT_PROVIDER_BACKEND_DAEMON 2u
#define CLEAT_PROVIDER_VT_DEFAULT 0u
#define CLEAT_PROVIDER_VT_PASSTHROUGH 1u
#define CLEAT_PROVIDER_VT_GHOSTTY 2u
/* Transport state of a session (cleat_session_connection_state). */
#define CLEAT_SESSION_CONNECTING 0u
#define CLEAT_SESSION_STREAMING 1u
#define CLEAT_SESSION_DISCONNECTED 2u
#define CLEAT_SESSION_CLOSED 3u
/* Attachment role (cleat_session_role / cleat_session_desc.role). */
#define CLEAT_ROLE_UNKNOWN 0u
#define CLEAT_ROLE_WATCHER 1u
#define CLEAT_ROLE_CONTROLLER 2u
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
/* Reported mouse tracking level (cleat_terminal_mode_state.mouse_tracking_mode). */
#define CLEAT_MOUSE_TRACKING_NONE 0u
#define CLEAT_MOUSE_TRACKING_X10 1u
#define CLEAT_MOUSE_TRACKING_NORMAL 2u
#define CLEAT_MOUSE_TRACKING_BUTTON 3u
#define CLEAT_MOUSE_TRACKING_ANY 4u
/* Reported mouse report format (cleat_terminal_mode_state.mouse_report_format). */
#define CLEAT_MOUSE_FORMAT_LEGACY 0u
#define CLEAT_MOUSE_FORMAT_SGR 1u
#define CLEAT_MOUSE_FORMAT_SGR_PIXELS 2u
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
#define CLEAT_PROVIDER_FEATURE_CELL_SNAPSHOTS (1u << 0)
#define CLEAT_PROVIDER_FEATURE_DAMAGE_ROWS (1u << 1)
#define CLEAT_PROVIDER_FEATURE_STRUCTURED_MOUSE_INPUT (1u << 2)
#define CLEAT_PROVIDER_FEATURE_IMAGE_STATE (1u << 3)
#define CLEAT_PROVIDER_FEATURE_REMOTE_TARGETS (1u << 4)
#define CLEAT_PROVIDER_FEATURE_RENDER_UPDATES (1u << 5)
#define CLEAT_RENDER_UPDATE_VERSION 1u
#define CLEAT_RENDER_OP_FULL_VISIBLE_REPLACE 1u
#define CLEAT_RENDER_OP_ROW_REPLACE 2u
#define CLEAT_RENDER_OP_SCROLL_COPY 3u
#define CLEAT_STYLE_COLOR_NONE 0u
#define CLEAT_STYLE_COLOR_PALETTE 1u
#define CLEAT_STYLE_COLOR_RGB 2u
#define CLEAT_IMAGE_FORMAT_RGB 0u
#define CLEAT_IMAGE_FORMAT_RGBA 1u
#define CLEAT_IMAGE_FORMAT_PNG 2u
#define CLEAT_IMAGE_FORMAT_GRAY_ALPHA 3u
#define CLEAT_IMAGE_FORMAT_GRAY 4u
#define CLEAT_IMAGE_COMPRESSION_NONE 0u
#define CLEAT_IMAGE_COMPRESSION_ZLIB_DEFLATE 1u
#define CLEAT_IMAGE_PLACEMENT_VIRTUAL (1u << 0)

typedef struct CleatProvider cleat_provider;
typedef struct CleatSession cleat_session;
typedef void cleat_wake_fn(void *user_data);
typedef bool cleat_image_resource_data_fn(void *user_data, const uint8_t *data, size_t data_len);

typedef enum cleat_dirty_state {
    CLEAT_DIRTY_CLEAN = 0,
    CLEAT_DIRTY_PARTIAL = 1,
    CLEAT_DIRTY_FULL = 2,
} cleat_dirty_state;

/* Borrowed, non-NUL-terminated UTF-8 slice. */
typedef struct cleat_str {
    const uint8_t *ptr;
    size_t len;
} cleat_str;

typedef struct cleat_provider_desc {
    uint32_t abi_version;
    uint32_t requested_features;
    uint32_t backend;
    const uint8_t *runtime_root;
    size_t runtime_root_len;
    /*
     * Daemon-backend only: name of the daemon under the runtime root (NULL for
     * the default daemon). One provider talks to one daemon over one
     * multiplexed packet connection; open one provider per daemon.
     */
    const uint8_t *daemon_name;
    size_t daemon_name_len;
    /*
     * Daemon-backend only: optional directory subscription tag selectors.
     * AND-only exact match against opaque session tags; empty subscribes to
     * every session on the daemon.
     */
    const cleat_str *directory_selectors;
    size_t directory_selector_count;
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
    /*
     * Optional client-supplied session id (UTF-8, not NUL-terminated). When set,
     * the session reuses this durable identity across recreations; when id is
     * NULL the provider allocates one. Pair the pointer with id_len.
     */
    const uint8_t *id;
    size_t id_len;
    bool record;
    const struct cleat_session_colors *colors;
    /*
     * Daemon-backend only: opaque tags attached at creation (client convention
     * is key=value, e.g. "project=uishell"). Ignored by other backends.
     */
    const cleat_str *tags;
    size_t tag_count;
    /*
     * Daemon-backend only: requested attachment role. CLEAT_ROLE_UNKNOWN and
     * CLEAT_ROLE_CONTROLLER request control (the daemon may grant watcher if
     * another controller holds the session); CLEAT_ROLE_WATCHER attaches
     * read-only. The granted role is reported by cleat_session_role.
     */
    uint32_t role;
} cleat_session_desc;

/*
 * One entry of the daemon directory subscription. All cleat_str pointers
 * borrow from the live directory and stay valid until
 * cleat_provider_directory_release.
 */
typedef struct cleat_directory_entry {
    cleat_str session_id;
    cleat_str state;
    const cleat_str *tags;
    size_t tag_count;
    uint32_t controller_count;
    uint32_t watcher_count;
    bool recreatable;
    uint16_t cols;
    uint16_t rows;
} cleat_directory_entry;

typedef struct cleat_directory {
    uint64_t generation;
    const cleat_directory_entry *entries;
    size_t entry_count;
} cleat_directory;

typedef struct cleat_rgb {
    uint8_t r;
    uint8_t g;
    uint8_t b;
} cleat_rgb;

/*
 * Optional terminal default colours for session creation. Set size to
 * sizeof(cleat_session_colors). A false has_* flag leaves that default unset.
 * Snapshots still contain resolved RGB cells after the provider applies these
 * defaults.
 */
typedef struct cleat_session_colors {
    size_t size;
    bool has_foreground;
    cleat_rgb foreground;
    bool has_background;
    cleat_rgb background;
    bool has_cursor;
    cleat_rgb cursor;
} cleat_session_colors;

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
    bool blink;
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

typedef struct cleat_terminal_mode_state {
    bool mouse_tracking;
    uint32_t mouse_tracking_mode;
    uint32_t mouse_report_format;
    bool mouse_sgr;
    bool mouse_sgr_pixels;
    bool active_alternate_screen;
    bool application_cursor_keys;
    bool alternate_scroll;
} cleat_terminal_mode_state;

typedef struct cleat_snapshot {
    uint16_t cols;
    uint16_t rows;
    cleat_terminal_geometry geometry;
    uint32_t viewport_kind;
    uint64_t scrollback_offset_rows;
    cleat_terminal_scrollbar_state scrollbar;
    cleat_terminal_mode_state terminal_modes;
    uint64_t render_generation;
    const cleat_cell *cells;
    size_t cell_count;
    const uint16_t *dirty_rows;
    size_t dirty_row_count;
    cleat_cursor cursor;
    cleat_dirty_state dirty;
} cleat_snapshot;

typedef struct cleat_style_color {
    size_t size;
    uint32_t tag;
    uint8_t palette_index;
    cleat_rgb rgb;
} cleat_style_color;

typedef struct cleat_render_style {
    size_t size;
    uint32_t flags;
    uint32_t width;
    cleat_rgb fg;
    cleat_rgb bg;
    cleat_style_color fg_color;
    cleat_style_color bg_color;
    uint32_t underline_style;
    cleat_style_color underline_color;
    bool protected_cell;
    bool has_hyperlink;
    uint32_t semantic;
    uint64_t hyperlink_id;
    uint32_t content_tag;
    bool has_text;
    bool has_styling;
    uint16_t style_id;
} cleat_render_style;

typedef struct cleat_render_cell {
    size_t size;
    const uint32_t *graphemes;
    size_t grapheme_count;
    cleat_render_style style;
} cleat_render_cell;

typedef struct cleat_render_row {
    size_t size;
    uint16_t row;
    uint16_t col_count;
    const cleat_render_cell *cells;
    size_t cell_count;
    bool wrap;
    bool wrap_continuation;
    bool has_graphemes;
    bool has_styling;
    bool has_hyperlink;
    uint32_t semantic_prompt;
    bool has_kitty_virtual_placeholder;
    bool dirty;
} cleat_render_row;

typedef struct cleat_render_update_op {
    size_t size;
    uint32_t kind;
    uint16_t first_row;
    uint16_t row_count;
    uint16_t col_count;
    const cleat_render_row *rows;
    size_t row_desc_count;
    const cleat_render_cell *cells;
    size_t cell_count;
    uint16_t src_row;
    uint16_t dst_row;
} cleat_render_update_op;

typedef struct cleat_image_resource {
    size_t size;
    uint32_t image_id;
    uint64_t generation;
    uint32_t width_px;
    uint32_t height_px;
    uint32_t format;
    uint32_t compression;
    size_t data_len;
} cleat_image_resource;

typedef struct cleat_image_placement {
    size_t size;
    uint32_t image_id;
    uint64_t generation;
    uint32_t placement_id;
    int32_t z;
    int32_t viewport_col;
    int32_t viewport_row;
    uint32_t grid_cols;
    uint32_t grid_rows;
    uint32_t pixel_width;
    uint32_t pixel_height;
    uint32_t source_x;
    uint32_t source_y;
    uint32_t source_width;
    uint32_t source_height;
    uint32_t x_offset_px;
    uint32_t y_offset_px;
    uint32_t flags;
} cleat_image_placement;

typedef struct cleat_render_update {
    size_t size;
    uint32_t version;
    uint16_t cols;
    uint16_t rows;
    cleat_terminal_geometry geometry;
    uint32_t viewport_kind;
    uint64_t scrollback_offset_rows;
    cleat_terminal_scrollbar_state scrollbar;
    cleat_terminal_mode_state terminal_modes;
    uint64_t render_generation;
    cleat_cursor cursor;
    cleat_dirty_state dirty;
    const cleat_render_update_op *ops;
    size_t op_count;
    const cleat_image_resource *image_resources;
    size_t image_resource_count;
    const cleat_image_placement *image_placements;
    size_t image_placement_count;
} cleat_render_update;

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
 * Registers an edge-triggered provider wake callback. The callback is a
 * scheduling nudge, not a rendering callback: it may be invoked synchronously
 * from the Cleat call that dirtied a session, and future backends may invoke it
 * from provider-owned IO threads. Keep it non-blocking and bounce to the
 * session-owner/UI thread before calling session APIs.
 */
void cleat_provider_set_wake_callback(cleat_provider *provider, cleat_wake_fn *wake, void *user_data);
void cleat_provider_close(cleat_provider *provider);

/*
 * Daemon directory subscription (Workspace Inventory feed). The generation
 * counter starts at 0 before the first snapshot and bumps on every snapshot or
 * delta (including retags); poll it and re-read the directory on change. Only
 * one directory may be live per provider; release before requesting again.
 * Entries are sorted by session id. Always 0/false for non-daemon providers.
 */
uint64_t cleat_provider_directory_generation(const cleat_provider *provider);
bool cleat_provider_directory_snapshot(cleat_provider *provider, cleat_directory *out);
void cleat_provider_directory_release(cleat_provider *provider, cleat_directory *directory);

cleat_session *cleat_session_create(cleat_provider *provider, const cleat_session_desc *desc);
/*
 * Attach to an existing daemon session by id instead of creating one. Honors
 * only id, cols, rows, and cell pixel sizes of the desc. If the session does
 * not exist the daemon closes the channel and the session reports
 * CLEAT_SESSION_CLOSED.
 */
cleat_session *cleat_session_attach(cleat_provider *provider, const cleat_session_desc *desc);
void cleat_session_destroy(cleat_session *session);

/*
 * Daemon session id for attach-by-id and directory correlation (borrows from
 * the session; valid until destroy). False for non-daemon sessions.
 */
bool cleat_session_id(const cleat_session *session, cleat_str *out);
/*
 * Transport state (CLEAT_SESSION_*). In-process sessions are always
 * STREAMING. Daemon sessions report CONNECTING until the first render packet,
 * DISCONNECTED while the connection is down (it reconnects with backoff and
 * recovers on its own), and CLOSED once the daemon reported the session gone.
 */
uint32_t cleat_session_connection_state(const cleat_session *session);
/*
 * Granted attachment role (CLEAT_ROLE_*). In-process sessions are their own
 * controllers. Daemon sessions report UNKNOWN until the daemon's grant
 * arrives; the role can change later (another client may take control — the
 * wake callback fires on the change). Watcher input, resize, and viewport
 * commands are dropped daemon-side.
 */
uint32_t cleat_session_role(const cleat_session *session);
/*
 * Request the controller role, preempting another packet client's control if
 * needed (a legacy `cleat attach` stream controller is never preempted). The
 * grant lands asynchronously; poll cleat_session_role after wake.
 */
bool cleat_session_take_control(cleat_session *session);

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

/*
 * Services provider/session progress and returns known dirty state. For the
 * in-process backend this currently pumps PTY output; cleat_session_dirty does
 * not. Call this from the session-owner thread.
 */
cleat_dirty_state cleat_session_poll(cleat_session *session);
/*
 * Returns already-known dirty state without pumping provider IO.
 */
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
 * Returns a versioned, operation-shaped render update. The initial/full-dirty
 * path uses CLEAT_RENDER_OP_FULL_VISIBLE_REPLACE. Partial dirty rows use
 * CLEAT_RENDER_OP_ROW_REPLACE when the VT backend exposes exact row damage.
 * Scrolling currently falls back to full visible replacement until Ghostty
 * exposes scroll/copy damage.
 */
bool cleat_session_render_update(cleat_session *session, cleat_render_update *out);
/*
 * Borrows image bytes for an image resource reported by
 * cleat_session_render_update. The callback is invoked synchronously and the
 * data pointer is valid only for the duration of that callback. This currently
 * succeeds only for in-process sessions. The callback must not call back into
 * this session.
 */
bool cleat_session_with_image_resource_data(cleat_session *session,
                                            uint32_t image_id,
                                            uint64_t generation,
                                            cleat_image_resource_data_fn *callback,
                                            void *user_data);
/*
 * Returns a snapshot for a requested terminal viewport. Unsupported viewport
 * kinds or offsets return false.
 */
bool cleat_session_viewport_snapshot(cleat_session *session, const cleat_viewport_request *request, cleat_snapshot *out);
void cleat_session_release_snapshot(cleat_session *session, cleat_snapshot *snapshot);
void cleat_session_release_render_update(cleat_session *session, cleat_render_update *update);

#ifdef __cplusplus
}
#endif

#endif
