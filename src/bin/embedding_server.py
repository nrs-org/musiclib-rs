#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "torch>=2",
#     "sentence-transformers>=3.0",
#     "python-dotenv>=1.0",
#     "mediapipe",
#     "wordfreq",
# ]
# [tool.uv.sources]
# torch = [
#     { index = "pytorch-cpu" },
# ]
#
# [[tool.uv.index]]
# name = "pytorch-cpu"
# url = "https://download.pytorch.org/whl/cpu"
# explicit = true
# ///
"""ML inference server — sentence embeddings + romanized-Japanese detection.

Endpoints:
  POST /embed           {"text": "..."}      ->  {"vector": [...]}
  POST /embed_batch     {"texts": [...]}     ->  {"vectors": [[...], ...]}
  POST /detect_romaji   {"texts": [...]}     ->  {"results": [[tok, ...], ...]}

/detect_romaji returns, for each input text, the list of tokens detected as
romanised Japanese (ja-Latn). Preprocessing (conversion to kana, etc.) is
the caller's responsibility — see the embed_batch example in match.example.rhai.

Usage:
  PORT=8082 uv run src/bin/embedding_server.py
  MODEL=all-MiniLM-L6-v2 PORT=8082 uv run src/bin/embedding_server.py
"""

import json
import os
import re
import ssl
import sys
import urllib.request
from collections import defaultdict
from http.server import BaseHTTPRequestHandler, HTTPServer

from dotenv import load_dotenv
from sentence_transformers import SentenceTransformer

load_dotenv()

# ── Romaji detection ──────────────────────────────────────────────────────────

_NGRAM_N = 3
_CJK_BONUS = 0.8
_EN_PENALTY_SCALE = 0.5
_SYLLABLE_WEIGHT = 3.0

_CJK_RE = re.compile(r"[぀-ヿ一-鿿＀-￯]")
_MORA_RE = re.compile(
    r"(?i)ou|oo|uu|ii|aa|ee"
    r"|sh[auo]|sh[ei]|ch[auoe]|chi|ts[uaoe]|tsi"
    r"|ky[auo]|ny[auo]|hy[auo]|my[auo]|ry[auo]"
    r"|gy[auo]|by[auo]|py[auo]|zy[auo]|jy[auo]"
    r"|[kgsztdnhbpmyrwfjv][aeiou]"
    r"|[aeiou]"
    r"|n(?=[^aeiouny]|$)"
    r"|([ptksmbgdz])\1"
)

_MODEL_PATH = "/tmp/language_detector.tflite"
_MODEL_URL = (
    "https://storage.googleapis.com/mediapipe-models/language_detector"
    "/language_detector/float32/1/language_detector.tflite"
)


def _ensure_detector():
    if not os.path.exists(_MODEL_PATH):
        print("[embedding_server] downloading language detector model...", file=sys.stderr)
        ctx = ssl._create_unverified_context()
        with urllib.request.urlopen(_MODEL_URL, context=ctx) as r:
            with open(_MODEL_PATH, "wb") as f:
                f.write(r.read())
    import mediapipe as mp
    from mediapipe.tasks.python import text as mp_text
    opts = mp_text.LanguageDetectorOptions(
        base_options=mp.tasks.BaseOptions(model_asset_path=_MODEL_PATH)
    )
    return mp_text.LanguageDetector.create_from_options(opts)


def _tokenize(text):
    return re.findall(r"[^\s\-–—·•・/|]+", text)


def _coverage(token):
    t = token.lower()
    return sum(len(m.group()) for m in _MORA_RE.finditer(t)) / len(t) if t else 0.0


def _top_lang(detector, text):
    result = detector.detect(text)
    if not result.detections:
        return "unknown", 0.0
    top = max(result.detections, key=lambda d: d.probability)
    return top.language_code, top.probability


def detect_romaji_tokens(detector, text):
    """Return the list of tokens in text that are romanised Japanese."""
    from wordfreq import zipf_frequency

    tokens = _tokenize(text)
    has_cjk = bool(_CJK_RE.search(text))

    votes = [defaultdict(float) for _ in tokens]
    for i, tok in enumerate(tokens):
        lang, prob = _top_lang(detector, tok)
        votes[i][lang] += prob
    for start in range(len(tokens)):
        for end in range(start + 2, min(start + _NGRAM_N, len(tokens)) + 1):
            lang, prob = _top_lang(detector, " ".join(tokens[start:end]))
            for i in range(start, end):
                votes[i][lang] += prob

    adjusted = []
    for tok, v in zip(tokens, votes):
        v = dict(v)
        v["en"] = v.get("en", 0.0) + zipf_frequency(tok.lower(), "en") * _EN_PENALTY_SCALE
        if not _CJK_RE.search(tok):
            v["ja-Latn"] = v.get("ja-Latn", 0.0) + _coverage(tok) * _SYLLABLE_WEIGHT
        if has_cjk and not _CJK_RE.search(tok):
            v["ja-Latn"] = v.get("ja-Latn", 0.0) + _CJK_BONUS
        adjusted.append(v)

    preds = [max(v, key=v.__getitem__) for v in adjusted]
    for i, (tok, v) in enumerate(zip(tokens, adjusted)):
        if _CJK_RE.search(tok):
            continue
        left  = preds[i - 1] if i > 0 else None
        right = preds[i + 1] if i < len(preds) - 1 else None
        n = sum(p == "ja-Latn" for p in (left, right) if p is not None)
        if n == 2:   v["ja-Latn"] = v.get("ja-Latn", 0.0) + _SYLLABLE_WEIGHT * 0.5
        elif n == 1: v["ja-Latn"] = v.get("ja-Latn", 0.0) + _SYLLABLE_WEIGHT * 0.2

    return [tok for tok, v in zip(tokens, adjusted) if max(v, key=v.__getitem__) == "ja-Latn"]


# ── Model + detector startup ──────────────────────────────────────────────────

MODEL_NAME = os.environ.get("MODEL", "paraphrase-multilingual-MiniLM-L12-v2")
print(f"[embedding_server] loading {MODEL_NAME!r}...", file=sys.stderr)
_model = SentenceTransformer(MODEL_NAME)
print("[embedding_server] loading language detector...", file=sys.stderr)
_detector = _ensure_detector()
print("[embedding_server] ready.", file=sys.stderr)

# ── HTTP handler ──────────────────────────────────────────────────────────────

class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):  # noqa: A002
        print(f"[embedding_server] {self.address_string()} {format % args}", file=sys.stderr)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        try:
            data = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError as e:
            return self._err(400, str(e))

        if self.path == "/embed":
            text = data.get("text")
            if not text:
                return self._err(400, "missing 'text'")
            vec = _model.encode(text, normalize_embeddings=True).tolist()
            self._ok({"vector": vec})

        elif self.path == "/embed_batch":
            texts = data.get("texts")
            if not isinstance(texts, list):
                return self._err(400, "missing or non-array 'texts'")
            vecs = _model.encode(texts or [], normalize_embeddings=True, batch_size=64).tolist()
            self._ok({"vectors": vecs})

        elif self.path == "/detect_romaji":
            texts = data.get("texts")
            if not isinstance(texts, list):
                return self._err(400, "missing or non-array 'texts'")
            results = [detect_romaji_tokens(_detector, t) for t in texts]
            self._ok({"results": results})

        else:
            self._err(404, f"unknown path {self.path!r}")

    def _ok(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _err(self, status, message):
        body = json.dumps({"error": message}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    port = int(os.environ.get("PORT", 8082))
    server = HTTPServer(("0.0.0.0", port), Handler)
    print(f"[embedding_server] listening on :{port}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
