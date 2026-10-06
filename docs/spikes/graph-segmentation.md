# Spike: graph-based screen segmentation (#319)

> Historical spike report. PR #316 adopts the fixed-threshold producer and
> replaces the experimental interface/test dumps with the contracts in the
> [crate README](../../crates/terminal-screen/README.md). The adaptive `GRAPH_K`
> rerun mode and old `line` selector name below apply to the spike only.

Branch `spike/graph-segmentation` (on top of PR #316). This is spike code and is not meant to be merged as it stands.

- `crates/terminal-screen/src/segment.rs` holds the producer interface sketch and the graph producer.
- `crates/terminal-screen/tests/graph_segmentation.rs` has the synthetic layouts and an ignored timing test.
- `crates/cleat/tests/graph_segmentation_casts.rs` is an ignored test that replays the #312 casts through Ghostty and dumps region maps.
- `tree.rs` gains `ScreenTree::from_root` and a `pub(crate)` `span_node`, so that another producer can build a `ScreenTree`.

## Verdict

The graph producer finds the regions that #316's corner-matched boxes miss. On every Claude Code and Codex frame tested, the composer comes out as a region of its own.

- **Claude Code (normal).** The composer is an *enclosure*: the cells between the two `─` rules. The footer below the second rule and the transcript above the first rule are separate enclosures. On the frames tested the composer region is exactly one row (`❯`, or `❯ draft probe` plus the painted cursor) with confidence 1.0, because walls bound it.
- **Codex (normal and inline).** The composer is a *block*: the `›` row that blank rows set apart. Its confidence is 0.8. The footer and the right-hand warning are separate blocks, and so is the `• Working (…)` row. One level up, a *group* joins the composer and the footer, which is Codex's bottom pane.
- **Synthetic grids.** Inset boxes nest. Lazygit panels separate, and so do table cells, including `┬├┼` junctions. A one-row inverse menu highlight becomes a zone of its own. tmux panes separate only through the status bar's colour; the tmux section below has details.
- **Speed.** A release build takes 0.6–1.9 ms at 120×40 and 200×50, and no input was pathological. A screen full of `+` takes 0.8 ms at 120×40 and 1.6 ms at 200×50; #316's `analyze` takes 119 ms and 486 ms on the same input. The worst case found is a dense ASCII grid of 1×1 cells (`+-+-` / `| | `): 3.4 ms and 7.1 ms, producing 7.5k regions.

What did not work:

- Claude's screen-reader (AX) mode draws no rules, no colour and no blank rows between the status row, the footer and the composer. Those three rows merge into one block. Graph segmentation cannot recover structure that the application does not paint. Semantic tags and row metadata help only where the application emits them.
- The *group* level, which joins regions across one blank row, chains under single linkage. In a transcript where every paragraph sits one blank row from the next, the whole transcript becomes one group.
- An enclosure is a set of cells connected without crossing a wall. When a divider does not reach the screen edge, the panes on either side remain one enclosure and separate only at a lower level. tmux's divider stops at the status line, so this affects tmux.
- Frames that touch merge into one frame component. An inset box that sits on its parent's bottom border joins the outer frame. The "surround ray" rule still nests the inner enclosure under the outer one, but the inner frame gets no node of its own.
- The Felzenszwalb–Huttenlocher `k/|C|` term added nothing useful; the comparison section below has details. Single linkage with fixed thresholds is simpler and easier to explain.
- All weights are hand-tuned on six recordings and seven synthetic grids. None of this has been checked against ground truth.

## Algorithm

**Cells.** Each cell is classified once:

- *wall:* a box-drawing glyph (U+2500–U+257F); an ASCII `-` or `=` in a run of at least 3; a `|` with a border neighbour above or below; or a `+` attached to any of these. A screen of `+` therefore has no walls. A title embedded in a horizontal border (`╭─ Files ──╮`) also counts as wall, so it belongs to the frame rather than to the region beneath it.
- *gutter:* a blank cell in a horizontal blank run of at least 3 cells (`gutter_min`). Blank rows are therefore always gutters.
- *thin:* a gutter cell in a vertical blank run of at most one row, with content directly above and below it.
- *content:* every other cell, including word gaps of 1–2 cells.

Three special cases apply:

- The visible cursor cell counts as content and takes the semantic tag of its left neighbour.
- A one-cell background island next to content is treated as a *mark*: it counts as content, and its horizontal background change is not a boundary. This is how Claude's painted inverse cursor stays with the composer text.
- For an inverse cell, fg and bg are swapped before any comparison.

**Edges.** Each pair of 4-neighbours gets a weight built from additive terms. The code keeps a bit per term so that every boundary can report why it is there.

| term | weight |
|---|---|
| effective bg differs (inverse included) | +0.70 |
| gutter ↔ non-gutter | +0.50 |
| thin ↔ content | +0.40 |
| content ↔ content, vertical | +0.30 |
| fg differs (both non-blank) | +0.10 |
| bold/faint/italic/underline/strike differs | +0.05 |
| OSC 133 semantic content differs | +0.40 |
| vertical, row `semantic_prompt` differs | +0.20 |
| wall ↔ non-wall | no edge (wall) |
| wall ↔ wall | 0 |

**Merge sweep.** The edges are sorted once. A union-find then merges every edge whose weight is at most τ, for each level in turn:

| level | τ |
|---|---|
| block | 0.35 |
| group | 0.45 |
| zone | 0.65 |
| enclosure | no limit (every remaining edge) |

Each level starts from the previous level's partition, so the levels nest by construction. The result is a single-linkage dendrogram cut at four heights. An optional FH term (`block_k`) also merges an edge at block level when `w ≤ min(Int(C) + k/|C|)` over the two components.

**Tree.** Wall components become `frame` regions, and the non-wall components at the top level become `enclosure` regions. Adjacency and bounding boxes decide the nesting:

- A frame goes under the smallest adjacent enclosure whose bbox contains the frame's bbox.
- An enclosure goes under the smallest adjacent frame whose bbox strictly contains its own. It can instead go under an enclosure that surrounds it: rays cast from its bbox centre lines, passing over walls, must hit that enclosure in at least 3 of the 4 directions.

Below each enclosure come zone, group and block; a block made only of whitespace is a `gap`. The leaves are maximal row runs of one block. Every cell lies in exactly one leaf, and a test asserts this on every synthetic grid.

When two levels have the same cell set, they collapse into one node that carries all their kinds (for example `enclosure+zone+block`). The selector engine matches an element name *or* a role, so the selector `block` still finds such a node.

**Confidence** is `(cheapest boundary edge − costliest internal edge) / 0.5`, clamped to 0–1. A region bounded by walls scores 1.0. Each region also carries an `evidence` string that names the boundary reason, for example `block: internal 0.10, boundary 0.50 (gutter)`. The score is the region's persistence margin in the dendrogram: easy to explain, but not calibrated.

## Results on the #312 recordings

The key frames are taken before each input event (empty composer, pasted draft, idle after the answer), midway through the output after submit (working), and at the end (typed draft).

In the region maps, letters label the regions of the named level in reading order, `#` marks a frame cell, and `.` marks a gap at block level. Full dumps for every frame are in `scratchpad/graph/*.graph.txt`; the Rerun section gives the path.

The anchor is the visible cursor. When the real cursor is hidden, the anchor is Claude's one-cell inverse painted cursor instead. The table lists the regions that contain the anchor; `=` means the same region as the enclosure column.

| recording / frame | enclosure | group | block | verdict |
|---|---|---|---|---|
| Claude normal, empty | row 10 `❯` | = | = | composer found |
| Claude normal, pasted | row 10 `❯ Reply exactly OK…` | = | = | composer found |
| Claude normal, working | row 14 `❯` | = | = | composer found; spinner row is its own block |
| Claude normal, final | row 16 `❯ draft probe` | = | = | composer found; footer is a separate enclosure |
| Claude AX, final | whole screen | rows 11–13 | rows 11–13 `Brewed… auto mode… $ draft probe` | failed: status, footer and composer merged |
| Claude wezterm (AX), final | whole screen | rows 0–9 | rows 0–9 | failed: banner and composer merged (no blank row) |
| Codex normal, empty | whole screen | rows 36–39 (composer + footer) | row 36 `› Ask Codex to do anything` | composer found (placeholder included) |
| Codex normal, working | whole screen | rows 36–39 | row 36 | composer found; `• Working` is a separate block |
| Codex normal, final | whole screen | rows 36–38 (composer + footer) | row 36 `› draft probe` | composer found; footer and warning are separate |
| Codex inline, all frames | whole screen | composer + footer | `›` row | composer found |
| bash OSC 133, final | whole screen | row 2 | `draft probe` (the semantic tag splits off the prompt `$ ` as its own block) | correct |

#316 finds 0 boxes on every one of these frames.

### Claude Code 2.1.287 normal, final (columns 0–40)

```
    text                                     | block                                    | zone                                     | enclosure
 8                                           | ........................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
 9  ❯ Reply exactly OK. Do not use tools.    | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC... | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
10                                           | ........................................ | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
11  ● OK                                     | DDDD.................................... | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
12                                           | ........................................ | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
13  ✻ Cooked for 4s · done 12:04 AM          | EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE......... | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
14                                           | ........................................ | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
15  ──────────────────────────────────────── | ######################################## | ######################################## | ########################################
16  ❯ draft probe                            | FFFFFFFFFFFFFF.......................... | DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
17  ──────────────────────────────────────── | ######################################## | ######################################## | ########################################
18    ⏵⏵ auto mode on (shift+tab to cycle)   | GGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG.. | EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC
```

Enclosure B is the composer, bounded by the two rules, and enclosure C is the footer. Inside enclosure A, the echoed prompt on row 9 (bg `#373737`) forms its own zone, B.

### Claude Code normal, working

```
    text                                     | block                                    | zone                                     | enclosure
 9  ❯ Reply exactly OK. Do not use tools.    | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC... | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
10                                           | ........................................ | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
11  ✢ Processing…                            | DDDDDDDDDDDDD........................... | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
12                                           | ........................................ | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
13  ──────────────────────────────────────── | ######################################## | ######################################## | ########################################
14  ❯                                        | EEE..................................... | DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
15  ──────────────────────────────────────── | ######################################## | ######################################## | ########################################
16    ⏵⏵ auto mode on (shift+tab to cycle) · | FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF | EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC
```

`✢ Processing…` is block D. The composer (just `❯`, between the rules, not shown) is enclosure B with confidence 1.0.

### Codex 0.160 normal, final (block map, full width)

```
    text                                                                                                                     | block
34                                                                                                                           | ........................................................................................................................
35                                                                                                                           | ........................................................................................................................
36  › draft probe                                                                                                            | HHHHHHHHHHHHHH..........................................................................................................
37                                                                                                                           | ........................................................................................................................
38    GPT-6.1-Sol default · /tmp/osc133-evidence · Reply exactly OK                                                          | IIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIII.........................................................
39                                                                                                 ⚠ 1 warning · f2 to view  | ...............................................................................................JJJJJJJJJJJJJJJJJJJJJJJJJ
```

The composer is block H and the footer is block I. The warning J sits 30 cells to the right; the space between is a gutter, so the warning is a block of its own.

### Codex normal, working (block and group)

```
    text                                                         | block                                                        | group
26                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
27                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
28                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
29                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
30                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
31                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
32                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
33  • Working (1s • esc to interrupt)                            | FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF........................... | EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEAAAAAAAAAAAAAAAAAAAAAAAAAAA
34                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
35                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
36  › Ask Codex to do anything                                   | GGGGGGGGGGGGGGGGGGGGGGGGGG.................................. | FFFFFFFFFFFFFFFFFFFFFFFFFFAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
37                                                               | ............................................................ | FFFFFFFFFFFFFFFFFFFFFFFFFFAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
38    GPT-6.1-Sol default · /tmp/osc133-evidence · ⠸             | HHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH............ | FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFAAAAAAAAAAAA
39    ? for shortcuts                                            | HHHHHHHHHHHHHHHHH........................................... | FFFFFFFFFFFFFFFFFAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
```

The working row is block F. Group F joins the composer and the footer across the single blank row between them. That recovers Codex's bottom pane even though Codex draws no border around it.

### Claude AX mode, final: where it fails

```
    text                                          | block                                         | zone
 8  auto mode on (shift+tab to cycle)             | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA............ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
 9  you: Reply exactly OK. Do not use tools.      | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB..... | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBAAAAA
10  claude: OK                                    | CCCCCCCCCC................................... | BBBBBBBBBBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
11  Brewed for 1s · done 12:04 AM                 | DDDDDDDDDDDDDDDDDDDDDDDDDDDDD................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
12  auto mode on (shift+tab to cycle)             | DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD............ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
13  $  draft probe                                | DDDDDDDDDDDDDDD.............................. | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
```

Rows 11–13 (status, footer and composer) form one block. Rows 9–10 split off only because they carry OSC 133 prompt semantics.

### Claude trust dialog (first frame)

```
    text                                                         | block                                                        | group                                                        | enclosure
 0                                                               | ............................................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
 1  ──────────────────────────────────────────────────────────── | ############################################################ | ############################################################ | ############################################################
 2   Accessing workspace:                                        | AAAAAAAAAAAAAAAAAAAAA....................................... | BBBBBBBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
 3                                                               | ............................................................ | BBBBBBBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
 4   /tmp/osc133-evidence                                        | BBBBBBBBBBBBBBBBBBBBB....................................... | BBBBBBBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
 5                                                               | ............................................................ | BBBBBBBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
 6   Quick safety check: Is this a project you created or one yo | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
 7   project, or work from your team). If not, take a moment to  | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
 8                                                               | ............................................................ | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
 9   Claude Code'll be able to read, edit, and execute files her | DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
10                                                               | ............................................................ | BBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
11   Security guide                                              | EEEEEEEEEEEEEEE............................................. | BBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
12                                                               | ............................................................ | BBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
13   ❯ No, exit                                                  | FFFFFFFFFFF................................................. | BBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
14     Yes, I trust this folder                                  | ...FFFFFFFFFFFFFFFFFFFFFFFF................................. | DDDBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
15                                                               | ............................................................ | DDDBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
16   Enter to confirm · Esc to cancel                            | GGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG........................... | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCCCCCCCCCCCCC | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
```

The rule on row 1 is a frame. The menu (`❯ No, exit` / `Yes, I trust this folder`) is one block, F. The selection marker does not change the style, so the two options do not separate.

### bash with OSC 133, final

```
    text                                     | block                                    | enclosure
 0  bash-5.2$ export PS1=$'\e]133;A\a$ \e]13 | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
 1  ;0\007"'                                 | AAAAAAAA................................ | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
 2  $ draft probe                            | BBCCCCCCCCCCCC.......................... | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
```

The prompt `$ ` (block B) and the input `draft probe` with the cursor (block C) are separate blocks. Only the semantic tags separate them.

## Synthetic grids

Each map shows four levels, left to right: block, group, zone and enclosure.

```
=== inset box  (33 regions)
  text                            | block                           | group                           | zone                            | enclosure
  ╭─ Edit ──────────────────────╮ | ############################### | ############################### | ############################### | ###############################
  │ Notes about the change      │ | #AAAAAAAAAAAAAAAAAAAAAAA......# | #AAAAAAAAAAAAAAAAAAAAAAABBBBBB# | #AAAAAAAAAAAAAAAAAAAAAAAAAAAAA# | #AAAAAAAAAAAAAAAAAAAAAAAAAAAAA#
  │ ╭─────────────────────────╮ │ | #A###########################B# | #A###########################C# | #A###########################A# | #A###########################A#
  │ │ > draft probe           │ │ | #A#CCCCCCCCCCCCCCC..........#B# | #A#DDDDDDDDDDDDDDDEEEEEEEEEE#C# | #A#BBBBBBBBBBBBBBBBBBBBBBBBB#A# | #A#BBBBBBBBBBBBBBBBBBBBBBBBB#A#
  │ ╰─────────────────────────╯ │ | #A###########################B# | #A###########################C# | #A###########################A# | #A###########################A#
  ╰─────────────────────────────╯ | ############################### | ############################### | ############################### | ###############################
  enclosure:has(cursor):not(:has(enclosure)) -> [" > draft probe           "]
  frame > enclosure -> [" Notes about the change      \n   \n   > draft probe             \n   "]
```

The inner enclosure nests under the outer one through the surround rays. The two frames touch at the outer bottom border, so they form a single frame component.

```
=== lazygit panels  (65 regions)
  text                                       | block                                      | group                                      | zone                                       | enclosure
  ╭─ Files ──────╮╭─ Diff ─────────────────╮ | ########################################## | ########################################## | ########################################## | ##########################################
  │ M src/a.rs   ││ @@ -1,3 +1,4 @@        │ | #AAAAAAAAAAA...##BBBBBBBBBBBBBBBB........# | #AAAAAAAAAAABBB##CCCCCCCCCCCCCCCCDDDDDDDD# | #AAAAAAAAAAAAAA##BBBBBBBBBBBBBBBBBBBBBBBB# | #AAAAAAAAAAAAAA##BBBBBBBBBBBBBBBBBBBBBBBB#
  │ A src/b.rs   ││ -old line              │ | #AAAAAAAAAAA...##BBBBBBBBBB..............# | #AAAAAAAAAAABBB##CCCCCCCCCCDDDDDDDDDDDDDD# | #AAAAAAAAAAAAAA##BBBBBBBBBBBBBBBBBBBBBBBB# | #AAAAAAAAAAAAAA##BBBBBBBBBBBBBBBBBBBBBBBB#
  ╰──────────────╯│ +new line              │ | #################BBBBBBBBBB..............# | #################CCCCCCCCCCDDDDDDDDDDDDDD# | #################BBBBBBBBBBBBBBBBBBBBBBBB# | #################BBBBBBBBBBBBBBBBBBBBBBBB#
  ╭─ Branches ───╮│ +another               │ | #################BBBBBBBBB...............# | #################CCCCCCCCCDDDDDDDDDDDDDDD# | #################BBBBBBBBBBBBBBBBBBBBBBBB# | #################BBBBBBBBBBBBBBBBBBBBBBBB#
  │ * main       ││                        │ | #CDDDDDD.......##........................# | #EFFFFFFGGGGGHH##DDDDDDDDDDDDDDDDDDDDDDDD# | #CDDDDDDDDDDDCC##BBBBBBBBBBBBBBBBBBBBBBBB# | #CCCCCCCCCCCCCC##BBBBBBBBBBBBBBBBBBBBBBBB#
  │   feature    ││                        │ | #...EEEEEEE....##........................# | #IIIJJJJJJJHHHH##DDDDDDDDDDDDDDDDDDDDDDDD# | #CCCCCCCCCCCCCC##BBBBBBBBBBBBBBBBBBBBBBBB# | #CCCCCCCCCCCCCC##BBBBBBBBBBBBBBBBBBBBBBBB#
  ╰──────────────╯╰────────────────────────╯ | ########################################## | ########################################## | ########################################## | ##########################################
  q: quit  enter: stage   ?: help            | FFFFFFFFFFFFFFFFFFFFF...GGGGGGG........... | KKKKKKKKKKKKKKKKKKKKKLLLMMMMMMMNNNNNNNNNNN | EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE | DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD
  enclosure:has-text('main') -> [" * main       \n   feature    "]
  zone:has(span[inverse]) -> ["* main     "]
```

Files, Branches, Diff and the unframed key-hint row are four separate enclosures. The panel titles belong to the frame. The inverse selection in Branches is a zone of its own.

```
=== table with junctions  (51 regions)
  text                   | block                  | group                  | zone                   | enclosure
  ┌──────┬───────┬─────┐ | ###################### | ###################### | ###################### | ######################
  │ name │ size  │ ok  │ | #AAAAAA#BBBBBBB#CCCCC# | #AAAAAA#BBBBBBB#CCCCC# | #AAAAAA#BBBBBBB#CCCCC# | #AAAAAA#BBBBBBB#CCCCC#
  ├──────┼───────┼─────┤ | ###################### | ###################### | ###################### | ######################
  │ a.rs │ 12 kB │ yes │ | #DDDDDD#EEEEEEE#FFFFF# | #DDDDDD#EEEEEEE#FFFFF# | #DDDDDD#EEEEEEE#FFFFF# | #DDDDDD#EEEEEEE#FFFFF#
  │ b.rs │ 3 kB  │ no  │ | #DDDDDD#EEEEEEE#FFFFF# | #DDDDDD#EEEEEEE#FFFFF# | #DDDDDD#EEEEEEE#FFFFF# | #DDDDDD#EEEEEEE#FFFFF#
  └──────┴───────┴─────┘ | ###################### | ###################### | ###################### | ######################
  +------+-------+       | ################...... | ################GGGGGG | ################GGGGGG | ################GGGGGG
  | x    | y     |       | #GG....#HH.....#...... | #HHIIII#JJKKKKK#GGGGGG | #HHHHHH#IIIIIII#GGGGGG | #HHHHHH#IIIIIII#GGGGGG
  +------+-------+       | ################...... | ################GGGGGG | ################GGGGGG | ################GGGGGG
```

Each cell of the Unicode table, including those meeting at `┬├┼┴` junctions, is an enclosure, and so is each cell of the ASCII `+---+` table. Rows 3 and 4 have no divider between them, so each column's cells across those rows correctly stay together.

```
=== tmux split + status  (65 regions)
  text                                                       | block                                                      | group                                                      | zone                                                       | enclosure
  $ cargo build                 │top - 10:00 up 3 days       | AAAAAAAAAAAAA.................#BBBBBBBBBBBBBBBBBBBBB...... | AAAAAAAAAAAAABBBBBBBBBBBBBBBBB#CCCCCCCCCCCCCCCCCCCCCDDDDDD | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#BBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#AAAAAAAAAAAAAAAAAAAAAAAAAAA
     Compiling foo v0.1.0       │Tasks: 200 total            | ...AAAAAAAAAAAAAAAAAAAA.......#BBBBBBBBBBBBBBBB........... | BBBAAAAAAAAAAAAAAAAAAAABBBBBBB#CCCCCCCCCCCCCCCCDDDDDDDDDDD | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#BBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#AAAAAAAAAAAAAAAAAAAAAAAAAAA
      Finished dev              │                            | ....AAAAAAAAAAAA..............#........................... | BBBBAAAAAAAAAAAABBBBBBBBBBBBBB#CCCCCCCCCCEEECCCDDDDDDDDDDD | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#BBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#AAAAAAAAAAAAAAAAAAAAAAAAAAA
  $ _                           │  PID USER   %CPU COMMAND   | CCC...........................#DDDDDDDDDD...EEEEEEEEEEEEEE | FFFBBBBBBBBBBBBBBBBBBBBBBBBBBB#CCCCCCCCCCEEECCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#BBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#AAAAAAAAAAAAAAAAAAAAAAAAAAA
                                │  123 rob    12.0 cargo     | ..............................#DDDDDDDDD....EEEEEEEEEE.... | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB#CCCCCCCCCEEEECCCCCCCCCCGGGG | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#BBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#AAAAAAAAAAAAAAAAAAAAAAAAAAA
                                │  456 rob     1.0 zsh       | ..............................#DDDDDDDDD.....EEEEEEE...... | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB#CCCCCCCCCEEEECCCCCCCCGGGGGG | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#BBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA#AAAAAAAAAAAAAAAAAAAAAAAAAAA
  [0] 0:zsh* 1:top-             "host" 10:00 05-Oct-26       | FFFFFFFFFFFFFFFFF.............GGGGGGGGGGGGGGGGGGGGGG...... | HHHHHHHHHHHHHHHHHIIIIIIIIIIIIIJJJJJJJJJJJJJJJJJJJJJJKKKKKK | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
  zone:has(cursor) -> ["$ cargo build                 \n   Compiling foo v0.1.0       \n    Finished dev              \n$ _                           \n                              \n                              "]
```

This one partly fails. The `│` divider stops at the status row, so both panes and the status bar form one enclosure. The green status bar becomes its own zone, which leaves the left and right panes as separate zones A and B. Without a coloured status bar the panes would merge. A rule that splits an enclosure along any wall run spanning at least 90% of its height would fix this; it is not implemented.

```
=== menu highlight  (26 regions)
  text         | block        | group        | zone         | enclosure
  ┌─Menu─────┐ | ############ | ############ | ############ | ############
  │ apple    │ | #AAAAAA....# | #AAAAAABBBB# | #AAAAAAAAAA# | #AAAAAAAAAA#
  │ banana   │ | #BBBBBBB...# | #CCCCCCCDDD# | #BBBBBBBBBB# | #AAAAAAAAAA#
  │ cherry   │ | #CCCCCCC...# | #EEEEEEEFFF# | #CCCCCCCCCC# | #AAAAAAAAAA#
  └──────────┘ | ############ | ############ | ############ | ############
  zone:has(span[inverse]) -> [" banana   "]
```

```
=== claude-like  (29 regions)
  text                                          | block                                         | group                                         | zone                                          | enclosure
  ⏺ I updated the file and ran the tests.       | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA...... | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
    All 12 passed.                              | AAAAAAAAAAAAAAAA............................. | AAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
                                                | ............................................. | AAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
  ✻ Cooked for 2s                               | BBBBBBBBBBBBBBB.............................. | AAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
  ───────────────────────────────────────────── | ############################################# | ############################################# | ############################################# | #############################################
  ❯ draft probe                                 | CCCCCCCCCCCCCC............................... | CCCCCCCCCCCCCCDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB
  ───────────────────────────────────────────── | ############################################# | ############################################# | ############################################# | #############################################
    ⏵⏵ auto mode on (shift+tab to cycle)        | DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD....... | EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEFFFFFFF | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC | CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC
  enclosure:has(span[inverse]) -> ["❯ draft probe                                "]
```

```
=== codex-like  (38 regions)
  text                                               | block                                              | group                                              | zone                                               | enclosure
  • Ran cargo test                                   | AAAAAAAAAAAAAAAA.................................. | AAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
    └ ok                                             | AA#AAA............................................ | AA#AAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AA#AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AA#AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
                                                     | .................................................. | AACAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
  • Working (3s • esc to interrupt)                  | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB................. | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
                                                     | .................................................. | AAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
  › draft probe                                      | CCCCCCCCCCCCCC.................................... | AAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
                                                     | .................................................. | AAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
    GPT-6.1 default · /tmp/x                         | DDDDDDDDDDDDDDDDDDDDDDDDDD........................ | AAAAAAAAAAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBBBBBBBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
                                 ⚠ 1 warning · f2    | ...............................EEEEEEEEEEEEEEEE... | BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBDDDDDDDDDDDDDDDDBBB | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA | AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
  block:has(cursor) -> ["› draft probe "]
  group:has(cursor) -> ["• Ran cargo test\n    ok\n      \n• Working (3s • esc to interrupt)\n              \n› draft probe \n              \n  GPT-6.1 default · /tmp/x"]
```

The codex-like grid shows group chaining. Every paragraph sits one blank row from the next, so the whole transcript and the bottom pane end up in one group.

## Timing (release, Apple Silicon, mean of 50 iterations)

Column meanings:

- `segment`: the graph producer.
- `to_screen_tree`: the adapter that adds spans and the cursor so #316 selectors can run.
- `#316 analyze`: the existing structural tree on the same grid.

| size | content | segment | to_screen_tree | #316 analyze | regions |
|---|---|---|---|---|---|
| 120×40 | prose + blank rows | 1.0 ms | 1.0 ms | 0.29 ms | 546 |
| 120×40 | all `+` | 0.85 ms | 0.22 ms | **118.5 ms** | 42 |
| 120×40 | all `─` | 0.69 ms | 0.26 ms | 0.38 ms | 42 |
| 120×40 | ASCII 1×1 grid `+-+` / `\| \|` | 3.4 ms | 5.4 ms | 7.5 ms | 3622 |
| 120×40 | every cell new fg/bg | 0.86 ms | 3.5 ms | 3.3 ms | 82 |
| 120×40 | blank | 0.58 ms | 0.20 ms | 0.26 ms | 42 |
| 200×50 | prose + blank rows | 1.8 ms | 2.0 ms | 0.52 ms | 1136 |
| 200×50 | all `+` | 1.6 ms | 0.32 ms | **486 ms** | 52 |
| 200×50 | all `─` | 1.6 ms | 0.50 ms | 0.72 ms | 52 |
| 200×50 | ASCII 1×1 grid | 7.1 ms | 12.7 ms | 38.5 ms | 7527 |
| 200×50 | every cell new fg/bg | 1.9 ms | 8.3 ms | 9.0 ms | 102 |
| 200×50 | blank | 1.6 ms | 0.40 ms | 0.65 ms | 52 |

The cost is O(cells · log cells) for the sort, plus about five linear passes. Nothing is quadratic: frame–enclosure adjacency uses a list per region, and the surround rays cost O(rows + cols) per enclosure.

The code is unoptimised. It builds an evidence string and a `BTreeSet` for every region and makes four labelling passes. Bucket-sorting quantised weights and reusing buffers across frames should make it several times faster. Even as it stands, under 2 ms per frame is fast enough to run on every frame.

The machine was busy with other work during these runs, so absolute numbers are noisy. The #316 numbers come from the same run; for the 200×50 `+` screen, the earlier probe measured 445 ms where this run measured 486 ms.

## Felzenszwalb–Huttenlocher vs single linkage

With `GRAPH_K=8`, which enables the FH adaptive term at block level, the cast dumps differ from single linkage in only two places:

- Codex's composer block absorbs the blank row below it, and its confidence drops to 0.00.
- Claude AX's merged block grows by one row.

The edge weights here take only a few quantised values. FH's `k/|C|` term only lets small components absorb their neighbours. It was designed for texture gradients in images, and terminal grids have none. Fixed thresholds per level give a tree whose levels mean something (block, group, zone, enclosure), which is worth more to selectors. The part of FH worth keeping, the "internal difference", lives on as the confidence margin.

## Interface sketch: a switchable region producer

The sketch lives in `segment.rs`:

```rust
pub enum Provenance { Declared, Structural, Graph, Temporal, Learned }
pub enum RegionKind { Screen, Frame, Enclosure, Zone, Group, Block, Gap, Line }

pub struct Region {
    pub kinds: BTreeSet<RegionKind>,   // collapsed chain, e.g. {Enclosure, Zone}
    pub bounds: Rect,                  // bbox; the cells are the union of descendant leaves
    pub cell_count: u32,
    pub provenance: Provenance,
    pub confidence: f32,               // 0..1, producer-defined margin
    pub evidence: String,              // why the boundary is where it is
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

pub struct RegionTree { pub regions: Vec<Region>, /* leaf_of[cell] */ .. }
impl RegionTree {
    fn leaf_at(&self, col, row) -> usize;
    fn region_at(&self, col, row, kind) -> Option<usize>;
    fn text(&self, grid, id) -> String;          // exact cells, not the bbox
    fn to_screen_tree(&self, grid) -> ScreenTree; // selectors walk this
}

pub struct FrameHistory<'a> { pub frames: &'a [ScreenGrid] }

pub trait RegionProducer {
    fn provenance(&self) -> Provenance;
    fn produce(&self, grid: &ScreenGrid, history: Option<&FrameHistory<'_>>) -> RegionTree;
}
```

Every producer meets the same contract:

1. The leaves are row slices that partition the grid; each cell lies in exactly one leaf.
2. Regions nest strictly: a child's cells are a subset of its parent's cells.
3. Every region carries provenance, confidence and evidence.

A region's cells need not form a rectangle. `bounds` is only a bounding box, and `text()` reads exactly the region's cells.

**Selectors.** `to_screen_tree` emits:

- a `region` element for each non-leaf region, with its kinds as roles;
- a `line` element for each leaf;
- under each leaf, the #316 `span`s clipped to the slice, plus the `cursor`.

Every node has `kind`, `provenance`, `confidence` and `evidence` attributes. Existing selectors keep working, and new region-aware selectors become possible. The synthetic tests ran these:

- `enclosure:has(cursor):not(:has(enclosure))` finds the innermost box that holds the cursor. On the inset box it returns `" > draft probe …"`.
- `block:has(cursor)` finds the Codex composer: `"› draft probe "`.
- `enclosure:has(span[inverse])` finds the Claude composer between the rules.
- `zone:has(span[inverse])` finds the highlighted menu item: `" banana   "`.

The graph tree is deeper and less regular than #316's fixed `screen > box > band > row` path. Selectors should use descendant combinators, and `:not(:has(...))` to pick the innermost match. A recognizer can prefer declared regions with `region[provenance=declared]`.

**Combining producers (not built).** The simplest composition is *refinement*. Producers run from most to least trusted: declared, then structural, then graph. Each later producer only subdivides leaves that the earlier ones left coarse. Provenance then varies by subtree, and the partition invariant still holds.

A temporal producer would not build a tree of its own. It would add edge terms to the same graph: cells that changed together in the last N frames get cheaper edges. Declared regions (the OSC proposed in #319) would enter as hard walls plus labels.

## Open problems and next steps

1. **Ground truth.** The weights fit the screens I looked at. The ratatui/Ink instrumentation spike would let us measure precision and recall of region boundaries instead of judging maps by eye.
2. **Group chaining.** Single linkage merges a transcript of paragraphs separated by single blank rows into one group. Possible fixes: join across a gap only when it is no wider than the local line spacing, use a relative-gap criterion, or cap group height.
3. **Partial dividers.** A wall run covering most of an enclosure's height or width should split the enclosure. This affects tmux, and vim splits whose divider ends at the status line.
4. **Touching frames.** Split wall components at T-junctions and corners into one frame per box, so that nested and adjacent boxes each get their own frame node.
5. **AX mode and similar.** When the paint carries no structure, the remaining signals are OSC 133, the cursor position and behaviour over time: the composer row changes on keystrokes, and status rows change on a timer.
6. **Identity across frames.** Regions are recomputed on every frame, so their IDs are not stable. A temporal producer needs regions matched between generations by overlap, and so does any UI that lets a user drag a region out.
7. **Heuristics that depend on the application.** Claude's painted cursor is found by a heuristic: a one-cell background island. Codex's placeholder text sits inside the composer block (it is faint, which adds only 0.05 to an edge weight), so telling "empty" from "draft" still needs the #313 recognizer.

## Rerun

```
cargo test -p terminal-screen --locked --test graph_segmentation -- --nocapture --test-threads 1
cargo test --release -p terminal-screen --locked --test graph_segmentation timing -- --ignored --nocapture
GRAPH_OUT=/tmp/graph cargo test -p cleat --locked --test graph_segmentation_casts -- --ignored --nocapture
GRAPH_K=8 GRAPH_OUT=/tmp/graph-k8 cargo test -p cleat --locked --test graph_segmentation_casts -- --ignored --nocapture
```

Inside a cleat session, unset `CLEAT_SESSION`, `CLEAT_DAEMON` and `CLEAT_RUNTIME_DIR` before running these. The full per-frame dumps from this run are in `/private/tmp/claude-501/-Users-robert-dev-cleat/171e25f4-ac75-4c40-b76a-1f4cf3b8359c/scratchpad/graph/`.
