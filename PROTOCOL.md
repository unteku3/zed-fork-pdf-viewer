# Graphic Extension API v1

Source of truth: `protocol/src/lib.rs`. The matching host keeps a vendored copy in `crates/viewer_protocol`; changes must be synchronized and incompatibilities require a new API version.

Transport is a local child process with piped stdin/stdout. Each frame is a little-endian u32 length followed by that many bytes (maximum 64 MiB). Requests and responses are UTF-8 JSON serialized from the Rust enums. A response declares how many raw binary frames follow it, capped at 128 frames and 128 MiB in total. stdout must contain only protocol frames.

Each request has a monotonically generated request ID and a view ID. Responses echo the request ID. `Hello` must precede other requests and agrees API version 1. A process may own multiple views. `Open` supplies the local path, effective settings and an opaque previous state. `Update` supplies viewport size, screen scale and pointer, scroll, action or refresh events. `Close` releases a view. `Context` references a bounded snapshot token and returns text and optional PNG attachments.

A scene supplies text, buttons, tooltips, rectangle overlays and BGRA image nodes in viewport coordinates. Nodes are painted in order and clipped to the host surface. All PDF geometry and toolbar definitions live in this process. Image keys identify immutable pixel content within a view. A node may omit its binary frame when the host already has the key; `Refresh` retransmits images after cache eviction. UI keys map to extension action strings; the reserved `host:add_selection` action asks the host to add the current selection to the agent. Other arbitrary host commands are not exposed.

Scenes contain opaque restoration state and optional selection summaries. The host does not interpret PDF page numbers, zoom, regions or text layers. Prepared context remains associated with its captured selection rather than the current pointer position. The host preserves already inserted attachments after disabling or uninstalling the extension.

Disabling an extension rejects further work, terminates the shared process and invalidates pending UI updates using a host generation counter. A blocked transport is terminated after 30 seconds. Malformed framing terminates the process; ordinary document errors are returned without taking down other documents. The host offers explicit retry after failure.

Packages are two frames: manifest JSON, executable bytes. Manifest fields include ID, human name, release version, API version, platform, executable basename, file extensions and default settings. Only safe single-component names are accepted. Installers never extract arbitrary paths. GitHub URLs are restricted to HTTPS owner/repository URLs; versioned release URLs are pinned before fetching the package and checksum. A checksum alone is not publisher authentication.
