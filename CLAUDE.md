# Project Instructions

## Memory

Persistent memory for this project is stored **in this repository**, under
`docs/memory/`, instead of Claude's default out-of-repo memory location.

- `docs/memory/MEMORY.md` is the always-relevant index — one line per memory
  file, pointing into this same directory.
- Each memory file follows Claude's standard memory format (frontmatter with
  `name`, `description`, `metadata.type`, then content structured per that
  type's convention).
- This applies to every agent/session working in this repository, not just
  the one that set up this convention. When creating or updating a memory
  item here, write it to `docs/memory/`, update `docs/memory/MEMORY.md`'s
  index, and commit it like any other repo change — do not also write it to
  the default out-of-repo memory path.
