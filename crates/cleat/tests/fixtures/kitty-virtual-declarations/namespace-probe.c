#include <ghostty/vt.h>
#include <assert.h>
#include <stdio.h>
#include <string.h>

static void feed(GhosttyTerminal t, const char *s) {
    ghostty_terminal_vt_write(t, (const uint8_t *)s, strlen(s));
}
static void run(const char *label, const char *commands) {
    GhosttyTerminal t = NULL;
    assert(ghostty_terminal_new(NULL, &t, 10, 4) == GHOSTTY_SUCCESS);
    assert(ghostty_terminal_resize(t, 10, 4, 10, 20) == GHOSTTY_SUCCESS);
    feed(t, "\x1b_Ga=t,i=7,f=32,s=1,v=1;ESIz/w==\x1b\\");
    feed(t, commands);
    GhosttyKittyGraphics graphics = NULL;
    assert(ghostty_terminal_get(t, GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS, &graphics) == GHOSTTY_SUCCESS);
    GhosttyKittyGraphicsPlacementIterator it = NULL;
    assert(ghostty_kitty_graphics_placement_iterator_new(NULL, &it) == GHOSTTY_SUCCESS);
    assert(ghostty_kitty_graphics_get(graphics, GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR, &it) == GHOSTTY_SUCCESS);
    printf("%s declarations (before any placeholder cells):\n", label);
    while (ghostty_kitty_graphics_placement_next(it)) {
        uint32_t id, p, c, r; bool v;
        assert(ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID, &id) == GHOSTTY_SUCCESS);
        assert(ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_PLACEMENT_ID, &p) == GHOSTTY_SUCCESS);
        assert(ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_COLUMNS, &c) == GHOSTTY_SUCCESS);
        assert(ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_ROWS, &r) == GHOSTTY_SUCCESS);
        assert(ghostty_kitty_graphics_placement_get(it, GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IS_VIRTUAL, &v) == GHOSTTY_SUCCESS);
        printf("  image=%u placement=%u columns=%u rows=%u virtual=%d\n", id, p, c, r, v);
    }
    ghostty_kitty_graphics_placement_iterator_free(it);
    feed(t, "\x1b[38;2;0;0;7m\xf4\x8e\xbb\xae\xcc\x85\xcc\x85\x1b[0m");
    GhosttyKittyGraphicsVirtualPlacementIterator vi = NULL;
    assert(ghostty_kitty_graphics_virtual_placement_iterator_new(NULL, &vi) == GHOSTTY_SUCCESS);
    assert(ghostty_kitty_graphics_virtual_placement_iterator_reset(vi, t) == GHOSTTY_SUCCESS);
    GhosttyKittyGraphicsVirtualPlacementInfo info = { .size = sizeof(info) };
    while (ghostty_kitty_graphics_virtual_placement_next(vi, &info) == GHOSTTY_SUCCESS) {
        printf("  fragment image=%u placement=%u source=%ux%u pixels=%ux%u\n", info.image_id, info.placement_id, info.source_width, info.source_height, info.pixel_width, info.pixel_height);
    }
    ghostty_kitty_graphics_virtual_placement_iterator_free(vi);
    ghostty_terminal_free(t);
}
int main(void) {
    run("A: internal p=1 is 4x2, external p=1 is 2x1",
        "\x1b_Ga=p,i=7,U=1,c=1,r=1;\x1b\\"
        "\x1b_Ga=p,i=7,U=1,c=4,r=2;\x1b\\"
        "\x1b_Ga=p,i=7,p=1,U=1,c=2,r=1;\x1b\\");
    run("B: external p=1 is 4x2, internal p=1 is 2x1",
        "\x1b_Ga=p,i=7,U=1,c=1,r=1;\x1b\\"
        "\x1b_Ga=p,i=7,p=1,U=1,c=4,r=2;\x1b\\"
        "\x1b_Ga=p,i=7,U=1,c=2,r=1;\x1b\\");
}
