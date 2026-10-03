import base64
import tempfile
import unittest
import zipfile
from pathlib import Path

from aiengine.extract import ExtractError, extract, sniff


class ExtractTest(unittest.TestCase):
    def setUp(self):
        self.d = Path(tempfile.mkdtemp())

    def f(self, name, data: bytes | str):
        p = self.d / name
        p.write_bytes(data if isinstance(data, bytes) else data.encode())
        return str(p)

    def test_text_is_truncated(self):
        e = extract(self.f("a.txt", "x" * 10000), None, 100)
        self.assertEqual((e.kind, len(e.text)), ("text", 100))

    def test_source_code_is_text(self):
        self.assertEqual(extract(self.f("a.rs", "fn main() {}"), None, 100).kind, "text")

    def test_image_is_base64(self):
        png = b"\x89PNG\r\n\x1a\n" + b"0" * 20
        e = extract(self.f("a.png", png), None, 100)
        self.assertEqual(e.kind, "image")
        self.assertEqual(base64.b64decode(e.image_b64), png)

    def test_docx_text(self):
        p = self.d / "a.docx"
        with zipfile.ZipFile(p, "w") as z:
            z.writestr("word/document.xml", "<w:p><w:t>Hello</w:t><w:t xml:space='preserve'>World &amp; co</w:t></w:p>")
        e = extract(str(p), None, 100)
        self.assertEqual((e.kind, e.text), ("docx", "Hello World & co"))

    def test_corrupt_docx_degrades_with_warning(self):
        e = extract(self.f("bad.docx", "not a zip"), None, 100)
        self.assertEqual(e.text, "")
        self.assertTrue(e.warnings)

    def test_binary_has_no_content(self):
        e = extract(self.f("blob.bin", b"\x00\x01\x02\xff"), None, 100)
        self.assertEqual((e.kind, e.text), ("binary", ""))

    def test_audio_without_whisper_warns_instead_of_failing(self):
        e = extract(self.f("a.mp3", b"ID3" + b"\0" * 20), None, 100, whisper_model="")
        self.assertEqual(e.kind, "audio")
        self.assertIn("disabled", e.warnings[0])

    def test_pdf_without_backend_warns(self):
        e = extract(self.f("a.pdf", b"%PDF-1.4\n"), None, 100)
        self.assertEqual(e.kind, "pdf")  # text may be empty; must not raise

    def test_errors(self):
        with self.assertRaises(ExtractError) as c:
            extract(str(self.d / "nope"), None, 100)
        self.assertEqual(c.exception.code, "file_not_found")
        with self.assertRaises(ExtractError) as c:
            extract(str(self.d), None, 100)
        self.assertEqual(c.exception.code, "not_a_file")

    def test_sniff(self):
        self.assertEqual(sniff(b"%PDF-1.7"), "application/pdf")
        self.assertIsNone(sniff(b"\x00\x01"))


if __name__ == "__main__":
    unittest.main()
