/* Demonstrate existing semantic cell tags and the navigation getter's
 * same-row C/D behavior; this is not an editable-buffer safety guarantee.
 * Build against a pinned Ghostty prefix:
 * cc -I "$OSC133_PREFIX/include" ghostty-proof.c
 *    "$OSC133_PREFIX/lib/libghostty-vt.a" -lm -lpthread -ldl -o /tmp/ghostty-proof
 * Run: /tmp/ghostty-proof
 */
#include <ghostty/vt.h>
#include <stdio.h>
#include <string.h>
#include <assert.h>
static void check(GhosttyTerminal t,const char *stage){
 bool at=false; assert(ghostty_terminal_get(t,GHOSTTY_TERMINAL_DATA_CURSOR_AT_PROMPT,&at)==GHOSTTY_SUCCESS);
 printf("%s at_prompt=%d cells=",stage,at);
 for(int x=0;x<6;x++) {
  GhosttyGridRef ref; GhosttyCell cell; GhosttyCellSemanticContent semantic;
  GhosttyPoint p={.tag=GHOSTTY_POINT_TAG_ACTIVE,.value={.coordinate={.x=x,.y=0}}};
  assert(ghostty_terminal_grid_ref(t,p,&ref)==GHOSTTY_SUCCESS);
  assert(ghostty_grid_ref_cell(&ref,&cell)==GHOSTTY_SUCCESS);
  assert(ghostty_cell_get(cell,GHOSTTY_CELL_DATA_SEMANTIC_CONTENT,&semantic)==GHOSTTY_SUCCESS);
  printf("%d",semantic);
 } puts("");
}
static void write_vt(GhosttyTerminal t,const char *s){ghostty_terminal_vt_write(t,(const unsigned char*)s,strlen(s));}
int main(){GhosttyTerminal t=NULL; assert(ghostty_terminal_new(NULL,&t,80,24)==GHOSTTY_SUCCESS);
 check(t,"initial"); write_vt(t,"\033]133;A\007$ \033]133;B\007"); check(t,"empty-B");
 write_vt(t,"draft"); check(t,"draft"); write_vt(t,"\033]133;C\007"); check(t,"C-same-row");
 write_vt(t,"\033]133;D;0\007"); check(t,"D-same-row");
 write_vt(t,"\033[?1049h"); check(t,"alternate"); ghostty_terminal_free(t); }
