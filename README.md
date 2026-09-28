# PDF Viewer for Zed fork

An optional PDF viewer for **Zed fork with Graphic Extension API v1**, on macOS Apple Silicon. This is **not compatible with official Zed** or its WASM extension marketplace.

The package contains the PDF engine, page layout, toolbar, input handling, selection logic and agent-context preparation. It runs as a separate process. The fork supplies a generic GPUI surface and transports events, images, clipboard requests and agent attachments.

## Install

1. In the compatible fork, open the command palette and run **Zed: Install Extension from GitHub** (or open Extensions).
2. Enter `https://github.com/unteku3/zed-fork-pdf-viewer`.
3. Review the release and choose **Install and enable**.
4. Open a local PDF file.

No Rust toolchain or editor rebuild is needed to install or update the extension. In Extensions, **Disable** stops the process immediately and turns open tabs into placeholders. **Enable** restores their position and zoom. **Uninstall** removes the installed package; settings and previously attached conversation content are retained.

This is a native executable running with your user permissions, not a sandboxed WASM extension. Install only a repository you trust. Downloads use HTTPS and verify SHA-256; checksums detect corruption, not a compromised publisher. Releases are not Apple-notarized.

## Reading and agent context

- Scroll; use the toolbar or Cmd–plus/minus and Cmd–0 to zoom or fit width.
- Drag to select text. Option-drag to select an area; Cmd–C copies selected text.
- Press Return to add the selection to the agent panel. Images preserve formula layout; extracted text alone may not.
- Region auto-copy is optional. The extension can also include nearby full pages as supporting context.

Existing `pdf_viewer` settings remain supported. Defaults come from the extension, not the editor:

```json
{
  "pdf_viewer": {
    "send": "image_and_text",
    "include_current_page": false,
    "pages_before": 0,
    "pages_after": 0,
    "auto_copy_region": false,
    "preamble_first": null,
    "preamble_repeat": null
  }
}
```

`send` accepts `image`, `text`, or `image_and_text`. Neighbour counts are clamped to 20. Preambles are UTF-8 files (up to 64 KiB); relative paths are resolved next to `settings.json`. Placeholders: `{path}`, `{filename}`, `{page}`, `{page_count}`, `{area}`. Equivalent new settings can be placed under `graphic_extension_settings.zed-fork-pdf-viewer`.

The viewer uses a bounded raster cache and renders at most two page jobs per update. The host has a shared 128 MiB image cache. These are cache budgets, **not a total process-memory ceiling**: source documents, text, temporary render buffers and agent attachments use additional memory. Scanned documents do not gain OCR automatically.

## Develop

```sh
cargo test --locked --workspace
cargo build --locked --release
python3 tests/ipc.py target/release/zed-fork-pdf-viewer
python3 scripts/package.py
```

The `.zgv` package is intentionally flat: a length-prefixed JSON manifest followed by a length-prefixed executable. It contains no archive paths, symlinks or installation scripts. The installer checks API/platform compatibility and starts a handshake before committing the new version. Previous installations remain usable if staging fails.

See [PROTOCOL.md](PROTOCOL.md) for the fork-specific interface. UI changes and PDF-engine updates can be released independently of the editor as long as API v1 remains compatible. Local Cargo incremental compilation is disabled to avoid unbounded development caches.

## License and attribution

GPL-3.0-or-later. Extracted from the PDF viewer implemented in the Zed fork; includes code previously integrated with Zed's GPL components. The renderer uses the Hayro family of crates. Dependency versions are recorded in `Cargo.lock`; their respective licenses apply. No personal PDF documents or editor profiles are included.
