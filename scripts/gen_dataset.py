# /// script
# dependencies = [
#     "pykakasi",
#     "requests",
# ]
# ///
"""
Generates a large synthetic evaluation dataset by:
  1. Pulling English phrases from NLTK Brown corpus
  2. Pulling Japanese sentences from Wikipedia API and romanizing them
  3. Mixing them at the phrase level with known labels

Output: eval_dataset_large.json
"""
import json
import random
import re
import sys

random.seed(42)

TOKENIZE_RE = re.compile(r"[^\s\-–—·•/|]+")


def tokenize(text: str) -> list[str]:
    return TOKENIZE_RE.findall(text)


# --- English source ---

# A few public-domain Project Gutenberg books (plain text UTF-8)
_GUTENBERG_URLS = [
    "https://www.gutenberg.org/files/1342/1342-0.txt",   # Pride and Prejudice
    "https://www.gutenberg.org/files/11/11-0.txt",       # Alice in Wonderland
    "https://www.gutenberg.org/files/84/84-0.txt",       # Frankenstein
    "https://www.gutenberg.org/files/1661/1661-0.txt",   # Sherlock Holmes
]

def load_english_phrases(n: int = 2000) -> list[list[str]]:
    import urllib.request, ssl
    ctx = ssl._create_unverified_context()
    sent_re = re.compile(r"[A-Za-z][^.!?]*[.!?]")
    word_re = re.compile(r"[A-Za-z]+")
    phrases = []
    for url in _GUTENBERG_URLS:
        with urllib.request.urlopen(url, context=ctx) as r:
            text = r.read().decode("utf-8", errors="ignore")
        for m in sent_re.finditer(text):
            words = word_re.findall(m.group())
            if 3 <= len(words) <= 20:
                phrases.append(words)
        if len(phrases) >= n:
            break
    return phrases[:n]


# --- Japanese source ---

def fetch_ja_wikipedia(n_articles: int = 80) -> list[str]:
    import urllib.request
    import json as j
    import ssl

    ctx = ssl._create_unverified_context()
    headers = {"User-Agent": "musiclib-rs-dataset-gen/1.0 (research; contact ngoduyanh.chip@gmail.com)"}
    sentences = []

    def get(url: str):
        req = urllib.request.Request(url, headers=headers)
        with urllib.request.urlopen(req, context=ctx) as r:
            return j.loads(r.read())

    while len(sentences) < n_articles * 5:
        data = get(
            "https://ja.wikipedia.org/w/api.php"
            "?action=query&list=random&rnnamespace=0&rnlimit=10&format=json"
        )
        for page in data["query"]["random"]:
            pid = page["id"]
            data2 = get(
                f"https://ja.wikipedia.org/w/api.php"
                f"?action=query&pageids={pid}&prop=extracts"
                f"&exsentences=8&explaintext=1&format=json"
            )
            for p in data2["query"]["pages"].values():
                extract = p.get("extract", "")
                for sent in re.split(r"[。！？\n]", extract):
                    sent = sent.strip()
                    if len(sent) >= 6 and re.search(r"[ぁ-んァ-ン一-龯]", sent):
                        sentences.append(sent)

        print(f"  fetched {len(sentences)} JP sentences so far...", file=sys.stderr)

    return sentences


def is_labelable_romaji(tok: str) -> bool:
    """True if a token is unambiguously romanized Japanese (not a number, abbreviation, or symbol)."""
    # must contain at least one lowercase letter
    if not re.search(r"[a-z]", tok):
        return False
    # reject purely numeric or mixed number strings
    if re.fullmatch(r"[\d\s\-]+", tok):
        return False
    # reject tokens that are mostly non-alpha (punctuation, symbols)
    alpha = sum(c.isalpha() for c in tok)
    if alpha < len(tok) * 0.6:
        return False
    return True


_LATIN_WORD_RE = re.compile(r"[A-Za-z][A-Za-z\-']*")

def strip_latin(sentence: str) -> str:
    """Remove Latin-script words from a Japanese sentence before romanizing."""
    return _LATIN_WORD_RE.sub("", sentence)


def romanize_sentence(kks, sentence: str) -> list[str]:
    """Convert a JP sentence to romaji tokens, dropping noise (numbers, symbols, all-caps)."""
    result = kks.convert(strip_latin(sentence))
    tokens = []
    for item in result:
        hep = item["hepburn"].strip()
        if hep and is_labelable_romaji(hep):
            tokens.append(hep)
    return tokens


def is_labelable_english(tok: str) -> bool:
    """True if a token is unambiguous English (alphabetic, not all-caps abbreviation)."""
    if not re.search(r"[A-Za-z]", tok):
        return False
    if re.fullmatch(r"[A-Z]{1,4}", tok):  # GL, RIP, etc. — skip
        return False
    return True


# --- Dataset generation ---

def make_entries(
    en_phrases: list[list[str]],
    ja_phrases: list[list[str]],
    n: int = 1000,
) -> list[dict]:
    entries = []

    def phrase_to_entry(words: list[str], lang: str, filter_fn=None) -> dict:
        text = " ".join(words)
        labels = {}
        for w in tokenize(text):
            if filter_fn is None or filter_fn(w):
                labels[w] = lang
        return {"text": text, "labels": labels}

    # how many of each type
    n_pure_en   = n // 4
    n_pure_ja   = n // 4
    n_mix_en_ja = n // 4   # english prefix + romaji suffix
    n_mix_ja_en = n - n_pure_en - n_pure_ja - n_mix_en_ja

    # pure English
    for phrase in random.sample(en_phrases, min(n_pure_en, len(en_phrases))):
        entries.append(phrase_to_entry(phrase, "en", is_labelable_english))

    # pure romaji
    for phrase in random.sample(ja_phrases, min(n_pure_ja, len(ja_phrases))):
        entries.append(phrase_to_entry(phrase, "ja-Latn", is_labelable_romaji))

    # mixed: english prefix + romaji suffix
    for _ in range(n_mix_en_ja):
        ep = random.choice(en_phrases)
        jp = random.choice(ja_phrases)
        en_slice = ep[: random.randint(1, max(1, len(ep) // 2))]
        ja_slice = jp[: random.randint(1, max(1, len(jp) // 2))]
        text = " ".join(en_slice + ja_slice)
        labels = {w: "en" for w in tokenize(" ".join(en_slice)) if is_labelable_english(w)}
        labels.update({w: "ja-Latn" for w in tokenize(" ".join(ja_slice)) if is_labelable_romaji(w)})
        entries.append({"text": text, "labels": labels})

    # mixed: romaji prefix + english suffix
    for _ in range(n_mix_ja_en):
        ep = random.choice(en_phrases)
        jp = random.choice(ja_phrases)
        ja_slice = jp[: random.randint(1, max(1, len(jp) // 2))]
        en_slice = ep[: random.randint(1, max(1, len(ep) // 2))]
        text = " ".join(ja_slice + en_slice)
        labels = {w: "ja-Latn" for w in tokenize(" ".join(ja_slice)) if is_labelable_romaji(w)}
        labels.update({w: "en" for w in tokenize(" ".join(en_slice)) if is_labelable_english(w)})
        entries.append({"text": text, "labels": labels})

    random.shuffle(entries)
    return entries


if __name__ == "__main__":
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 1000

    print("Loading English phrases...", file=sys.stderr)
    en_phrases = load_english_phrases(n * 2)
    print(f"  {len(en_phrases)} phrases", file=sys.stderr)

    print("Fetching Japanese Wikipedia...", file=sys.stderr)
    ja_sentences = fetch_ja_wikipedia(n_articles=max(20, n // 10))

    print("Romanizing...", file=sys.stderr)
    from pykakasi import kakasi
    kks = kakasi()
    ja_phrases = []
    for sent in ja_sentences:
        tokens = romanize_sentence(kks, sent)
        # require at least 3 clean romaji tokens (romanize_sentence already filters noise)
        if 3 <= len(tokens) <= 20:
            ja_phrases.append(tokens)
    print(f"  {len(ja_phrases)} romanized phrases", file=sys.stderr)

    print("Generating entries...", file=sys.stderr)
    entries = make_entries(en_phrases, ja_phrases, n=n)
    print(f"  {len(entries)} entries", file=sys.stderr)

    out = "eval_dataset_large.json"
    with open(out, "w", encoding="utf-8") as f:
        json.dump(entries, f, ensure_ascii=False, indent=2)
    print(f"Written to {out}", file=sys.stderr)
