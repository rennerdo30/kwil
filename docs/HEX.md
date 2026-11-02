# Hex editor

Choose **Open as Hex** or **Toggle Hex View** in the command palette. Binary
files offer a choice of Hex, Text or Cancel. Hex mode edits physical file bytes,
including encoding markers and compressed data.

- Click a byte in the hex or ASCII column. **Tab** switches columns. Type two
  hexadecimal digits in the hex column, or one printable ASCII character in the
  ASCII column. Overwrite is the default; **Insert** toggles insertion.
- **Go to Line** (**Ctrl+G** with the default keymap) becomes **Go to offset**:
  enter a zero-based decimal offset or `0x` hexadecimal offset, then Enter.
- **Find** (**Ctrl+F**) accepts hexadecimal byte pairs such as `00 FF 41`.
  Enter searches forward and wraps once; a match is selected. Esc cancels a prompt.
- Select with Shift and the arrow keys or drag, then **Copy** (**Ctrl+C**) to copy
  uppercase hexadecimal pairs. Copy accepts up to 64 KiB; larger selections are
  refused. In the TUI the terminal must permit OSC 52 clipboard writes.
- Undo, redo and save use the document's existing history and atomic save path.
  Search and copy do not change bytes or add undo entries.

Search accepts up to 256 bytes and streams through the file on the document
worker. Display pages, query buffers and clipboard output have fixed bounds;
the file does not need to fit in RAM. Changes pending on the worker temporarily
make byte input read-only.
