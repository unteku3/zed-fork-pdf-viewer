#!/usr/bin/env python3
"""No private fixtures: exercise the released process with a generated two-page PDF."""
import json, pathlib, struct, subprocess, sys, tempfile
binary = pathlib.Path(sys.argv[1]).resolve()
def pdf():
    content = b'BT /F1 20 Tf 10 70 Td (Hello PDF) Tj ET\n'
    objects = [b'<< /Type /Catalog /Pages 2 0 R >>', b'<< /Type /Pages /Kids [3 0 R 3 0 R] /Count 2 >>', b'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>', b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>', b'<< /Length '+str(len(content)).encode()+b' >>\nstream\n'+content+b'endstream']
    data = b'%PDF-1.4\n'; offsets = [0]
    for i, obj in enumerate(objects, 1):
        offsets.append(len(data)); data += f'{i} 0 obj\n'.encode()+obj+b'\nendobj\n'
    xref = len(data); data += f'xref\n0 {len(offsets)}\n0000000000 65535 f \n'.encode()
    data += b''.join(f'{x:010d} 00000 n \n'.encode() for x in offsets[1:])
    return data + f'trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n'.encode()
def read_frame(stream):
    n = struct.unpack('<I', stream.read(4))[0]
    assert n <= 64*1024**2
    data = bytearray()
    while len(data) < n:
        part = stream.read(n-len(data)); assert part
        data.extend(part)
    return bytes(data)
with tempfile.TemporaryDirectory() as d:
    path = pathlib.Path(d)/'sample.pdf'; path.write_bytes(pdf())
    p = subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    serial = 0
    def call(command, view=1):
        global serial
        serial += 1; b = json.dumps(dict(id=serial,view=view,command=command)).encode()
        p.stdin.write(struct.pack('<I', len(b))+b);p.stdin.flush()
        r = json.loads(read_frame(p.stdout)); assert r['id']==serial
        blobs = [read_frame(p.stdout) for _ in range(r['blobs'])]
        return r,blobs
    try:
        r,_=call({'Hello':{'api_version':1}});assert not r['error']
        r,_=call({'Open':{'path':str(path),'settings':{},'state':None}});assert not r['error']
        def update(event):
            r,b=call({'Update':dict(width=500,height=500,scale=1,event=event,settings={})});assert not r['error'],r;return r,b
        r,b=update('Resize');assert b and r['scene']['nodes'] and r['scene']['keys']
        update({'Pointer':dict(kind='down',x=100,y=100,alt=True)})
        r,b=update({'Pointer':dict(kind='up',x=350,y=300,alt=True)})
        selection=r['scene']['selection']; assert selection and 'Page 1 of 2' in selection['first']
        r,b=call({'Context':dict(selection=selection['token'],images=True,first=True)})
        assert not r['error'] and b[0].startswith(b'\x89PNG')
        state=update({'Action':'zoom_out'})[0]['scene']['state']
        call('Close');call({'Open':dict(path=str(path),settings={},state=state)})
        assert update('Resize')[0]['scene']['state']['zoom']==state['zoom']
        r,_=call({'Open':dict(path=str(path.with_suffix('.missing')),settings={},state=None)},view=2)
        assert r['error']
        r,_=call({'Hello':dict(api_version=99)});assert r['error']
        call('Close'); p.stdin.close();assert p.wait(timeout=5)==0
    finally:
        if p.poll() is None:p.kill();p.wait()
print('IPC integration: open, scene, input, selection, image context, restore, error, compatibility, shutdown passed')
