# Project preferences

- Always write `TODOs.md` with LF (`\n`) line endings, never CRLF. The user
  sees carriage returns as `^M` in their editor. Preserve the existing content
  and verify there are no `\r` bytes after editing. On Windows, explicitly use
  `newline="\n"` for Python text writes or write UTF-8 bytes directly.
