"""Local RPM repository and Koji endpoint used by the real CLI rehearsal."""
import hashlib
import http.server
import threading
import xml.etree.ElementTree as ET

PRIMARY = b'''<?xml version="1.0"?>
<metadata xmlns="http://linux.duke.edu/metadata/common"
 xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="1">
 <package type="rpm"><name>zlib</name><arch>x86_64</arch>
 <version epoch="0" ver="1.3" rel="1.fc44"/>
 <checksum type="sha256" pkgid="YES">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</checksum>
 <summary>zlib</summary><description>test package</description>
 <packager>Test</packager><url>https://example.invalid/zlib</url>
 <time file="1700000000" build="1700000000"/>
 <size package="1000" installed="2000" archive="3000"/>
 <location href="Packages/zlib-1.3-1.fc44.x86_64.rpm"/>
 <format><rpm:license>MIT</rpm:license><rpm:vendor>Test</rpm:vendor>
 <rpm:group>Unspecified</rpm:group><rpm:buildhost>builder.invalid</rpm:buildhost>
 <rpm:sourcerpm>zlib-1.3-1.fc44.src.rpm</rpm:sourcerpm></format>
 </package></metadata>'''
REPOMD = ('''<repomd xmlns="http://linux.duke.edu/metadata/repo">
 <revision>1700000000</revision><data type="primary">
 <checksum type="sha256">%s</checksum><location href="repodata/primary.xml"/>
 <timestamp>1700000000</timestamp><size>%s</size></data></repomd>'''
           % (hashlib.sha256(PRIMARY).hexdigest(), len(PRIMARY))).encode()
BUILD = '''<struct>
 <member><name>build_id</name><value><int>1</int></value></member>
 <member><name>owner_name</name><value><string>rehearsal</string></value></member>
 <member><name>start_time</name><value><string>2026-04-01 10:00:00</string></value></member>
 <member><name>completion_time</name><value><string>2026-04-01 10:15:00</string></value></member>
 </struct>'''
RPMS = '''<array><data><value><struct>
 <member><name>id</name><value><int>5</int></value></member>
 <member><name>arch</name><value><string>x86_64</string></value></member>
 </struct></value></data></array>'''
SIGS = '''<array><data><value><struct>
 <member><name>sigkey</name><value><string>cafebabe</string></value></member>
 </struct></value></data></array>'''
# Served at the real dist-git path shape, reached via PG_COLLECT_DIST_GIT_BASE.
# Source0 is a forge URL so the spec stage emits linkable triples rather than
# only a DQ issue -- a fragment of nothing would make the replay assertions
# vacuous.
SPEC = b'''Name: zlib
Version: 1.3
Release: 1.fc44
Summary: zlib
License: Zlib
URL: https://example.invalid/zlib
Source0: https://github.com/madler/zlib/archive/v1.3.tar.gz
BuildRequires: cmake

%description
test package

%changelog
* Mon Apr 01 2026 Spec Fixture <fixture@example.invalid> - 1.3-1
- rehearsal entry
'''


class Hub(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.rpcs = []
        self.spec_requests = []
        self.fail_signatures = False
        self.fail_specs = False
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server_port}"

    def close(self):
        self.shutdown()
        self.server_close()
        self.thread.join()


class Handler(http.server.BaseHTTPRequestHandler):
    def send(self, code, body):
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        body = {"/repo/repodata/repomd.xml": REPOMD,
                "/repo/repodata/primary.xml": PRIMARY}.get(self.path)
        if body is None and self.path.startswith("/dist-git/"):
            # Only the f44 branch exists, so the later rawhide/main candidates
            # 404 -- exercising the real fallback chain rather than assuming
            # the first URL always wins.
            self.server.spec_requests.append(self.path)
            if self.server.fail_specs:
                self.send(503, b"dist-git down")
                return
            body = SPEC if self.path == "/dist-git/rpms/zlib/raw/f44/f/zlib.spec" else None
        self.send(200 if body else 404, body or b"not found")

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        method = ET.fromstring(body).findtext("methodName")
        self.server.rpcs.append(method)
        if method == "queryRPMSigs" and self.server.fail_signatures:
            # A fault is retryable at item level without transport backoff.
            self.send(200, b"<methodResponse><fault><value><struct>"
                      b"<member><name>faultString</name><value><string>backend down</string>"
                      b"</value></member></struct></value></fault></methodResponse>")
            return
        payload = {"getBuild": BUILD, "listBuildRPMs": RPMS, "queryRPMSigs": SIGS}.get(method)
        if payload is None:
            self.send(400, b"unexpected RPC")
            return
        self.send(200, ("<methodResponse><params><param><value>" + payload +
                        "</value></param></params></methodResponse>").encode())

    def log_message(self, *_args):
        pass
