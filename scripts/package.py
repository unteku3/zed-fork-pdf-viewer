#!/usr/bin/env python3
"""Build the deliberately flat v1 package: framed manifest + framed executable."""
import hashlib, json, pathlib, struct, sys
root = pathlib.Path(__file__).resolve().parents[1]
binary = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else root / 'target/release/zed-fork-pdf-viewer'
out = root / 'dist'
out.mkdir(exist_ok=True)
manifest = json.loads((root / 'manifest.json').read_text())
parts = [json.dumps(manifest).encode(), binary.read_bytes()]
assert all(len(part) <= 64 * 1024**2 for part in parts)
data = b''.join(struct.pack('<I', len(part)) + part for part in parts)
path = out / 'viewer-aarch64-apple-darwin.zgv'
path.write_bytes(data)
path.with_suffix('.zgv.sha256').write_text(hashlib.sha256(data).hexdigest() + '  ' + path.name + '\n')
print(path)
