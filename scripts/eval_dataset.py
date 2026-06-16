# Ground truth: token → "en" | "ja" | "ja-Latn"
# Tokens match what tokenize() in gg.py produces (split on whitespace and -–—·•/|).
# Labels on genuinely ambiguous tokens (e.g. proper nouns) are marked with a comment.

DATASET = [
    # --- pure English ---
    {
        "text": "City Of Gold",
        "labels": {"City": "en", "Of": "en", "Gold": "en"},
    },
    {
        "text": "Easy Revenge",
        "labels": {"Easy": "en", "Revenge": "en"},
    },
    {
        "text": "Bass Bootleg Edit",
        "labels": {"Bass": "en", "Bootleg": "en", "Edit": "en"},
    },
    {
        "text": "no music no life",
        "labels": {"no": "en", "music": "en", "life": "en"},
    },
    # --- pure CJK ---
    {
        "text": "みんなのメルヘン",
        "labels": {"みんなのメルヘン": "ja"},
    },
    {
        "text": "きっと",
        "labels": {"きっと": "ja"},
    },
    {
        "text": "夢見る羊",
        "labels": {"夢見る羊": "ja"},
    },
    # --- pure romaji ---
    {
        "text": "Watashi wa nihongo ga suki desu",
        "labels": {
            "Watashi": "ja-Latn", "wa": "ja-Latn", "nihongo": "ja-Latn",
            "ga": "ja-Latn", "suki": "ja-Latn", "desu": "ja-Latn",
        },
    },
    {
        "text": "Kono sekai de ikite iru",
        "labels": {
            "Kono": "ja-Latn", "sekai": "ja-Latn", "de": "ja-Latn",
            "ikite": "ja-Latn", "iru": "ja-Latn",
        },
    },
    {
        "text": "Ganbare Natsuki Subaru",
        "labels": {"Ganbare": "ja-Latn", "Natsuki": "ja-Latn", "Subaru": "ja-Latn"},
    },
    # --- CJK + romaji artist name ---
    {
        "text": "Nagi Yanagi ビードロ模様",
        "labels": {"Nagi": "ja-Latn", "Yanagi": "ja-Latn", "ビードロ模様": "ja"},
    },
    {
        "text": "Imouto no kimochi",
        "labels": {"Imouto": "ja-Latn", "no": "ja-Latn", "kimochi": "ja-Latn"},
    },
    {
        "text": "Rinon to issho ni 音楽",
        "labels": {
            "Rinon": "ja-Latn", "to": "ja-Latn", "issho": "ja-Latn",
            "ni": "ja-Latn", "音楽": "ja",
        },
    },
    # --- CJK title + English suffix ---
    {
        "text": "みんなのメルヘン Bootleg Edit",
        "labels": {"みんなのメルヘン": "ja", "Bootleg": "en", "Edit": "en"},
    },
    {
        "text": "失礼しますが RIP",
        "labels": {"失礼しますが": "ja", "RIP": "en"},
    },
    # --- mixed artist + CJK title ---
    {
        "text": "Nagi Yanagi ビードロ模様 Laser Imouto Bootleg",
        "labels": {
            "Nagi": "ja-Latn", "Yanagi": "ja-Latn", "ビードロ模様": "ja",
            "Laser": "en", "Imouto": "ja-Latn", "Bootleg": "en",  # Imouto used as EN branding here
        },
    },
    # --- "name" ambiguity ---
    {
        "text": "名前 (name)",  # "name" is the romaji gloss of 名前
        "labels": {"名前": "ja", "(name)": "ja-Latn"},
    },
    {
        "text": "Your Name",
        "labels": {"Your": "en", "Name": "en"},
    },
    # --- "no" ambiguity ---
    {
        "text": "音楽 no chikara",
        "labels": {"音楽": "ja", "no": "ja-Latn", "chikara": "ja-Latn"},
    },
    # --- "game" ambiguity ---
    {
        "text": "ゲーム Game Over",
        "labels": {"ゲーム": "ja", "Game": "en", "Over": "en"},
    },
    {
        "text": "sono game wa muzukashii",
        "labels": {
            "sono": "ja-Latn", "game": "ja-Latn",
            "wa": "ja-Latn", "muzukashii": "ja-Latn",
        },
    },
    # --- synthetic: romaji + english side by side ---
    {
        "text": "Kimi no Na wa Your Name",
        "labels": {
            "Kimi": "ja-Latn", "no": "ja-Latn", "Na": "ja-Latn", "wa": "ja-Latn",
            "Your": "en", "Name": "en",
        },
    },
    {
        "text": "Watashi wa a Good Person desu",
        "labels": {
            "Watashi": "ja-Latn", "wa": "ja-Latn", "a": "en", "Good": "en",
            "Person": "en", "desu": "ja-Latn",
        },
    },
    # --- from fixtures ---
    {
        "text": "SUPER TEK TYPE トラックメーカー",
        "labels": {"SUPER": "en", "TEK": "en", "TYPE": "en", "トラックメーカー": "ja"},
    },
    {
        "text": "君色ハナミズキ",
        "labels": {"君色ハナミズキ": "ja"},
    },
]
