// Flatten a tree-sitter tree into pre-order arrays in one recursive pass over
// its subtrees.
//
// The public route to the same arrays is a TSTreeCursor, which costs several
// calls into the library per node. This walks the Subtree structure directly,
// through the library's own inline accessors, and reproduces what the cursor
// reports: which children are visible (their own flag, or an alias the
// parent's production gives them), what symbol they show, and which field
// they hold (their parent's field map, inherited through hidden parents).
//
// It reads private headers, so it is tied to the exact tree-sitter version in
// Cargo.toml. The cursor implementation in tree.rs stays as the reference:
// ASTSCAN_CHECK_FLATTEN=1 runs both on every file and compares them.

#include "tree_sitter/api.h"

#include "language.h"
#include "subtree.h"
#include "tree.h"

#define NONE UINT32_MAX
#define F_NAMED 1
#define F_EXTRA 2
#define F_MISSING 4
#define F_NAMED_LEAF 8

typedef struct {
  uint16_t *kind;
  uint8_t *flags;
  uint32_t *start;
  uint32_t *end;
  uint32_t *parent;
  uint32_t *sub_end;
  uint32_t *prev;
  uint16_t *field;
  uint32_t len;
  uint32_t cap;
  // Set when the tree held more visible nodes than it declared; the caller
  // then discards the result.
  uint32_t overflow;
} AstscanFlat;

typedef struct {
  uint32_t idx;
  uint32_t last_child;
  bool has_named_child;
} Frame;

typedef struct {
  AstscanFlat *out;
  const TSLanguage *language;
} Ctx;

static void walk_children(Ctx *ctx, Subtree parent, uint32_t position, Frame *frame, TSFieldId inherited_field) {
  uint32_t child_count = ts_subtree_child_count(parent);
  if (child_count == 0) return;
  AstscanFlat *out = ctx->out;
  const TSLanguage *language = ctx->language;
  const TSSymbol *alias_sequence = ts_language_alias_sequence(language, parent.ptr->production_id);
  const TSFieldMapEntry *field_map, *field_map_end;
  ts_language_field_map(language, parent.ptr->production_id, &field_map, &field_map_end);
  const Subtree *children = ts_subtree_children(parent);

  uint32_t structural_index = 0;
  for (uint32_t i = 0; i < child_count; i++) {
    Subtree child = children[i];
    // The first child starts where its parent does: the parent's padding is its padding.
    if (i > 0) position += ts_subtree_padding(child).bytes;
    uint32_t size = ts_subtree_size(child).bytes;
    bool extra = ts_subtree_extra(child);

    TSSymbol alias = 0;
    TSFieldId field = 0;
    if (!extra) {
      if (alias_sequence) alias = alias_sequence[structural_index];
      for (const TSFieldMapEntry *map = field_map; map < field_map_end; map++) {
        if (!map->inherited && map->child_index == structural_index) {
          field = map->field_id;
          break;
        }
      }
      if (!field) field = inherited_field;
      structural_index++;
    }

    if (ts_subtree_visible(child) || alias) {
      if (out->len >= out->cap) {
        out->overflow = 1;
        return;
      }
      uint32_t idx = out->len++;
      bool named = alias ? ts_language_symbol_metadata(language, alias).named : ts_subtree_named(child);
      uint8_t flags = 0;
      if (named) flags |= F_NAMED;
      if (extra) flags |= F_EXTRA;
      if (ts_subtree_missing(child)) flags |= F_MISSING;
      out->kind[idx] = ts_language_public_symbol(language, alias ? alias : ts_subtree_symbol(child));
      out->start[idx] = position;
      out->end[idx] = position + size;
      out->parent[idx] = frame->idx;
      out->prev[idx] = frame->last_child;
      out->field[idx] = field;
      frame->last_child = idx;
      frame->has_named_child |= named;

      Frame own = {idx, NONE, false};
      walk_children(ctx, child, position, &own, 0);
      if (out->overflow) return;
      if (!own.has_named_child) flags |= F_NAMED_LEAF;
      out->flags[idx] = flags;
      out->sub_end[idx] = out->len;
    } else if (ts_subtree_visible_child_count(child) > 0) {
      // A hidden node: its visible descendants are children of the node above.
      walk_children(ctx, child, position, frame, field);
      if (out->overflow) return;
    }
    position += size;
  }
}

// Visible nodes in the tree, the root included: the capacity `astscan_flatten` needs.
uint32_t astscan_node_count(const TSTree *tree) {
  return ts_subtree_visible_descendant_count(tree->root) + 1;
}

void astscan_flatten(const TSTree *tree, AstscanFlat *out) {
  out->len = 0;
  out->overflow = 0;
  if (out->cap == 0) {
    out->overflow = 1;
    return;
  }
  Subtree root = tree->root;
  const TSLanguage *language = tree->language;
  uint32_t position = ts_subtree_padding(root).bytes;
  bool named = ts_subtree_named(root);
  uint8_t flags = 0;
  if (named) flags |= F_NAMED;
  if (ts_subtree_extra(root)) flags |= F_EXTRA;
  if (ts_subtree_missing(root)) flags |= F_MISSING;
  out->len = 1;
  out->kind[0] = ts_language_public_symbol(language, ts_subtree_symbol(root));
  out->start[0] = position;
  out->end[0] = position + ts_subtree_size(root).bytes;
  out->parent[0] = NONE;
  out->prev[0] = NONE;
  out->field[0] = 0;

  Ctx ctx = {out, language};
  Frame frame = {0, NONE, false};
  walk_children(&ctx, root, position, &frame, 0);
  if (out->overflow) return;
  if (!frame.has_named_child) flags |= F_NAMED_LEAF;
  out->flags[0] = flags;
  out->sub_end[0] = out->len;
}

// The `log` slot of a TSLexer is variadic, which Rust cannot define. The
// experimental parser in src/fastparse.rs points its lexer here.
void astscan_lexer_log(const TSLexer *lexer, const char *format, ...) {
  (void)lexer;
  (void)format;
}
